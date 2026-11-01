//! A completion in flight, and the stop that goes with it (ADR-0020).
//!
//! DeepSeek keeps a message `wip` until it is finished or stopped. Closing the
//! HTTP connection alone does not stop it, so a caller that gives up on a stream
//! early leaves the message unfinished and the next turn that parents to it is
//! rejected. The web client avoids this with its stop button, which sends
//! `POST /api/v0/chat/stop_stream`. This type makes the same call for its users:
//!
//! - [`Completion::stop`] stops the message while the connection is still open,
//!   then closes it.
//! - Dropping an unfinished [`Completion`] does the same in the background, so
//!   an early exit (a client hang-up, a timeout, an error) cannot leave a `wip`
//!   message behind.
//!
//! The message id is read from the stream itself (`response_message_id`, first
//! one wins, as aiodeepseek does), so callers never pass it in.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use crate::{Client, ClientError};

/// The key the first event carries the generating message's id under.
const MESSAGE_ID_KEY: &str = "\"response_message_id\":";
/// Bytes kept between chunks, enough to hold a key split across two of them.
const SCAN_KEEP: usize = 64;
/// Upper bound on the stop call; the connection is closed regardless.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// The body of one `/api/v0/chat/completion` request.
pub struct Completion {
    body: ByteStream,
    client: Client,
    session_id: String,
    /// Id of the message being generated, once the stream has named it.
    message_id: Option<i64>,
    /// Tail of the text not yet searched for the message id.
    scan: String,
    /// True once the stream has ended or the message has been stopped.
    done: bool,
}

impl Completion {
    /// Wrap an open response. `stream_completion` does this; it is public so
    /// callers that build their own response can get the same stop behaviour.
    pub fn new(resp: reqwest::Response, client: Client, session_id: String) -> Self {
        Self {
            body: Box::pin(resp.bytes_stream()),
            client,
            session_id,
            message_id: None,
            scan: String::new(),
            done: false,
        }
    }

    /// The id of the message being generated, once the stream has named it.
    pub fn message_id(&self) -> Option<i64> {
        self.message_id
    }

    /// Stop the message DeepSeek is generating, then close the connection.
    ///
    /// Safe to call once the stream has ended: it does nothing then. Returns
    /// the error of the stop call; the connection is closed either way.
    pub async fn stop(&mut self) -> Result<(), ClientError> {
        if self.done {
            return Ok(());
        }
        self.done = true;
        let Some(message_id) = self.message_id else {
            return Err(ClientError::Shape("stop: no message id seen yet".into()));
        };
        match tokio::time::timeout(
            STOP_TIMEOUT,
            self.client.stop_stream(&self.session_id, message_id),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(ClientError::Shape("stop_stream timed out".into())),
        }
    }

    /// Read the whole body as text. The non-streaming counterpart of polling
    /// the stream; the stream is finished when this returns.
    pub async fn text(mut self) -> Result<String, reqwest::Error> {
        let mut buf = Vec::new();
        while let Some(chunk) = self.next().await {
            buf.extend_from_slice(&chunk?);
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Look for the message id in a chunk, until it is found.
    fn scan_for_message_id(&mut self, chunk: &[u8]) {
        if self.message_id.is_some() {
            return;
        }
        self.scan.push_str(&String::from_utf8_lossy(chunk));
        if let Some(at) = self.scan.find(MESSAGE_ID_KEY) {
            let digits: String = self.scan[at + MESSAGE_ID_KEY.len()..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(id) = digits.parse::<i64>() {
                self.message_id = Some(id);
                self.scan.clear();
                return;
            }
        }
        if self.scan.len() > SCAN_KEEP {
            let mut cut = self.scan.len() - SCAN_KEEP;
            while !self.scan.is_char_boundary(cut) {
                cut += 1;
            }
            self.scan.drain(..cut);
        }
    }
}

impl Stream for Completion {
    type Item = Result<Bytes, reqwest::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        match this.body.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.scan_for_message_id(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(None) => {
                this.done = true;
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let Some(message_id) = self.message_id else {
            // Nothing was generated that DeepSeek has named yet, so there is no
            // message to stop.
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                message_id,
                "completion dropped outside a runtime; message left wip"
            );
            return;
        };
        let client = self.client.clone();
        let session_id = std::mem::take(&mut self.session_id);
        runtime.spawn(async move {
            match tokio::time::timeout(STOP_TIMEOUT, client.stop_stream(&session_id, message_id))
                .await
            {
                Ok(Ok(())) => tracing::debug!(message_id, "stopped abandoned completion"),
                Ok(Err(e)) => tracing::warn!(message_id, error = %e, "stop_stream failed"),
                Err(_) => tracing::warn!(message_id, "stop_stream timed out"),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::STOP_STREAM_PATH;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    const READY: &str = "event: ready\ndata: {\"request_message_id\":1,\"response_message_id\":7,\"model_type\":\"default\"}\n\n";

    /// A minimal upstream: `/stream/hold` sends the ready event and keeps the
    /// connection open; `/stream/done` sends it and closes. Every
    /// `stop_stream` request is reported on the returned channel.
    async fn mock_upstream() -> (String, mpsc::UnboundedReceiver<(String, String)>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    let head_end = loop {
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break p + 4;
                        }
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                    let len: usize = head
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length: ")
                                .or(l.strip_prefix("Content-Length: "))
                        })
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    while buf.len() < head_end + len {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
                    if path == "/api/v0/chat/stop_stream" {
                        let _ = tx.send((path.clone(), body));
                        let json = r#"{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":null}}"#;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{json}",
                            json.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        return;
                    }
                    // A stream: headers, then the ready event, then either hold
                    // the connection or end it.
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n")
                        .await;
                    let _ = sock.write_all(READY.as_bytes()).await;
                    if path.ends_with("/done") {
                        let _ = sock.shutdown().await;
                    } else {
                        // Hold the stream open until the test ends.
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                });
            }
        });
        (format!("http://{addr}"), rx)
    }

    async fn open(base: &str, path: &str) -> Completion {
        let client = Client::with_base_url("tok".into(), base);
        let resp = reqwest::Client::new()
            .get(format!("{base}{path}"))
            .send()
            .await
            .unwrap();
        Completion::new(resp, client, "sess".into())
    }

    #[tokio::test]
    async fn stop_names_the_message_the_stream_named() {
        let (base, mut stops) = mock_upstream().await;
        let mut c = open(&base, "/stream/hold").await;
        let first = c.next().await.unwrap().unwrap();
        assert!(!first.is_empty());
        assert_eq!(c.message_id(), Some(7));

        c.stop().await.unwrap();

        let (path, body) = stops.recv().await.unwrap();
        assert_eq!(path, STOP_STREAM_PATH);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"chat_session_id": "sess", "message_id": 7})
        );
    }

    #[tokio::test]
    async fn dropping_an_unfinished_completion_stops_it() {
        let (base, mut stops) = mock_upstream().await;
        let mut c = open(&base, "/stream/hold").await;
        c.next().await.unwrap().unwrap();
        drop(c);

        let (path, body) = tokio::time::timeout(Duration::from_secs(5), stops.recv())
            .await
            .expect("drop should stop the message")
            .unwrap();
        assert_eq!(path, STOP_STREAM_PATH);
        assert!(body.contains("\"message_id\":7"), "{body}");
    }

    #[tokio::test]
    async fn a_finished_completion_is_not_stopped() {
        let (base, mut stops) = mock_upstream().await;
        let c = open(&base, "/stream/done").await;
        let text = c.text().await.unwrap();
        assert!(text.contains("response_message_id"));

        let quiet = tokio::time::timeout(Duration::from_millis(300), stops.recv()).await;
        assert!(quiet.is_err(), "a finished message must not be stopped");
    }
}
