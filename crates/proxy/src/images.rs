//! Image intake for one turn: find the image in the delta, bound it, and hand
//! raw bytes to the client.
//!
//! Port of `deepseek_proxy/images.py`. Three rules come from there and are not
//! negotiable:
//!
//! - DeepSeek takes **one** image per turn (`ref_file_ids` is single-element),
//!   so a turn with more than one keeps the first and drops the rest, telling
//!   the model what it lost (see [`Loaded::note`]). Rejecting instead would
//!   wedge the session: the delta is rebuilt from the same messages on every
//!   retry, so a 400 here repeats forever;
//! - the size bound applies to the base64 text *and* the decoded bytes, because
//!   a 10MB image is ~14MB of request body;
//! - a remote URL must resolve to public addresses only, so the proxy cannot be
//!   pointed at `169.254.169.254` or a LAN host.

use base64::Engine;
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// How long a remote image download may take (Python: 10s total).
const FETCH_TIMEOUT_SECS: u64 = 10;

/// Result of image intake for one turn.
///
/// `dropped > 0` means the caller must surface [`Loaded::note`] to the model;
/// the bytes of the dropped images are simply not sent.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Loaded {
    pub data: Option<Vec<u8>>,
    pub dropped: usize,
    pub total: usize,
}

impl Loaded {
    /// Notice for the model when images had to be dropped, else `None`.
    pub fn note(&self) -> Option<String> {
        if self.dropped == 0 {
            return None;
        }
        Some(format!(
            "[attachment notice] {} images were attached in this turn, but only one \
             image per turn can be sent. The first image was sent and is above. The \
             remaining {} were dropped and NOT seen. If they matter, re-attach them \
             one at a time in later turns.",
            self.total, self.dropped
        ))
    }
}

/// Rejected image: the message is returned to the client verbatim.
#[derive(Debug)]
pub struct ImageError(pub String);

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn err(msg: &str) -> ImageError {
    ImageError(msg.to_string())
}

/// Every `image_url` in `messages`, in order.
pub fn image_urls(messages: &[crate::Msg]) -> Vec<String> {
    let mut out = Vec::new();
    for m in messages {
        let Some(Value::Array(parts)) = &m.content else {
            continue;
        };
        for part in parts {
            if part.get("type").and_then(|t| t.as_str()) != Some("image_url") {
                continue;
            }
            let url = part
                .get("image_url")
                .and_then(|i| i.get("url"))
                .and_then(|u| u.as_str());
            if let Some(url) = url {
                out.push(url.to_string());
            }
        }
    }
    out
}

/// Raw bytes for the turn's image, if it has one, plus how many were dropped.
///
/// `messages` must be the *new* messages only (the delta), matching Python's
/// `load_image(messages, ...)` call site: an image from an earlier turn is
/// already attached to the remote conversation and must not be re-uploaded.
///
/// Extra images are *not* an error: the first is kept and the rest are
/// reported through [`Loaded::dropped`]. Failing the turn instead would be
/// unrecoverable, because the client retries the very same delta.
pub async fn load(messages: &[crate::Msg], max_bytes: usize) -> Result<Loaded, ImageError> {
    let urls = image_urls(messages);
    let total = urls.len();
    let Some(url) = urls.first() else {
        return Ok(Loaded::default());
    };
    let dropped = total - 1;

    let data = if let Some(rest) = url.strip_prefix("data:image/") {
        decode_data_url(rest, max_bytes)?
    } else {
        fetch_remote(url, max_bytes).await?
    };

    if data.is_empty() || data.len() > max_bytes {
        return Err(err("Invalid or unavailable image"));
    }
    Ok(Loaded {
        data: Some(data),
        dropped,
        total,
    })
}

/// Decode the part after `data:image/` — `png;base64,AAAA...`.
fn decode_data_url(rest: &str, max_bytes: usize) -> Result<Vec<u8>, ImageError> {
    let (header, encoded) = rest
        .split_once(',')
        .ok_or_else(|| err("Invalid or unavailable image"))?;
    if !header.ends_with(";base64") {
        return Err(err("Invalid or unavailable image"));
    }
    // Base64 inflates by 4/3; check before allocating the decode buffer.
    if encoded.len() > max_bytes.div_ceil(3) * 4 {
        return Err(err("Invalid or unavailable image"));
    }
    base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|_| err("Invalid or unavailable image"))
}

/// Download `url`, refusing private/loopback/link-local targets.
async fn fetch_remote(url: &str, max_bytes: usize) -> Result<Vec<u8>, ImageError> {
    let parsed = reqwest::Url::parse(url).map_err(|_| err("Invalid or unavailable image"))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.host_str().is_none()
    {
        return Err(err("Invalid or unavailable image"));
    }
    let host = parsed.host_str().unwrap_or_default().to_string();

    // Python resolves the name and rejects any non-global answer; so does this.
    // The name is resolved again by the connector, so a rebinding resolver
    // could still reach a private address — acceptable here because the
    // endpoint is a user-supplied image URL, not a trust boundary we own.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_global(ip) {
            return Err(err("Invalid or unavailable image"));
        }
    } else {
        let addrs =
            tokio::net::lookup_host((host.as_str(), parsed.port_or_known_default().unwrap_or(80)))
                .await
                .map_err(|_| err("Invalid or unavailable image"))?;
        let mut seen = false;
        for addr in addrs {
            seen = true;
            if !is_global(addr.ip()) {
                return Err(err("Invalid or unavailable image"));
            }
        }
        if !seen {
            return Err(err("Invalid or unavailable image"));
        }
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
        // A redirect could hop from a public host to a private one.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| err("Invalid or unavailable image"))?;

    let resp = client
        .get(parsed)
        .send()
        .await
        .map_err(|_| err("Invalid or unavailable image"))?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(err("Invalid or unavailable image"));
    }
    if let Some(len) = resp.content_length() {
        if len > max_bytes as u64 {
            return Err(err("Invalid or unavailable image"));
        }
    }

    // Stream rather than `bytes()`: a lying `content-length` must not be able
    // to make the proxy allocate an unbounded buffer.
    use futures_util::StreamExt;
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| err("Invalid or unavailable image"))?;
        if out.len() + chunk.len() > max_bytes {
            return Err(err("Invalid or unavailable image"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// `ipaddress.is_global` for the address families we can actually see.
fn is_global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_global_v4(v4),
        IpAddr::V6(v6) => is_global_v6(v6),
    }
}

fn is_global_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || ip.is_unspecified()
        // 100.64.0.0/10 carrier-grade NAT.
        || (o[0] == 100 && (64..128).contains(&o[1]))
        // 192.0.0.0/24 IETF protocol assignments.
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        // 198.18.0.0/15 benchmarking.
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        // 240.0.0.0/4 reserved.
        || o[0] >= 240)
}

fn is_global_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    let seg = ip.segments();
    // Unique local fc00::/7 and link local fe80::/10.
    if seg[0] & 0xfe00 == 0xfc00 || seg[0] & 0xffc0 == 0xfe80 {
        return false;
    }
    // IPv4-mapped: judge the embedded address.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_global_v4(v4);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_with(content: Value) -> crate::Msg {
        serde_json::from_value(serde_json::json!({
            "role": "user",
            "content": content,
        }))
        .unwrap()
    }

    fn image_part(url: &str) -> Value {
        serde_json::json!([{ "type": "image_url", "image_url": { "url": url } }])
    }

    #[test]
    fn decodes_base64_data_url() {
        let raw = b"\x89PNG\r\n\x1a\n";
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        let got = decode_data_url(&format!("png;base64,{encoded}"), 1024).unwrap();
        assert_eq!(got, raw);
    }

    #[test]
    fn rejects_non_base64_data_url() {
        assert!(decode_data_url("png,abc", 1024).is_err());
    }

    #[test]
    fn rejects_oversized_base64_before_decoding() {
        let encoded = "A".repeat(4096);
        assert!(decode_data_url(&format!("png;base64,{encoded}"), 100).is_err());
    }

    #[tokio::test]
    async fn keeps_first_image_and_drops_the_rest() {
        let msgs = vec![
            user_with(image_part("data:image/png;base64,AA==")),
            user_with(image_part("data:image/png;base64,AA==")),
            user_with(image_part("data:image/png;base64,AA==")),
        ];
        let got = load(&msgs, 1024).await.expect("extra images must not fail");
        assert_eq!(got.data.as_deref(), Some(&b"\x00"[..]));
        assert_eq!(got.total, 3);
        assert_eq!(got.dropped, 2);
        let note = got.note().expect("dropping images must be reported");
        assert!(note.contains("3 images"), "{note}");
        assert!(note.contains("remaining 2"), "{note}");
    }

    #[tokio::test]
    async fn one_image_has_no_note() {
        let msgs = vec![user_with(image_part("data:image/png;base64,AA=="))];
        let got = load(&msgs, 1024).await.unwrap();
        assert_eq!(got.dropped, 0);
        assert!(got.note().is_none());
    }

    #[tokio::test]
    async fn no_image_is_not_an_error() {
        let msgs = vec![user_with(Value::String("hi".into()))];
        let got = load(&msgs, 1024).await.unwrap();
        assert!(got.data.is_none());
        assert_eq!(got.dropped, 0);
        assert!(got.note().is_none());
    }

    #[tokio::test]
    async fn refuses_loopback_url() {
        let msgs = vec![user_with(image_part("http://127.0.0.1/a.png"))];
        assert!(load(&msgs, 1024).await.is_err());
    }

    #[test]
    fn private_addresses_are_not_global() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
        ] {
            assert!(!is_global(ip.parse().unwrap()), "{ip} must be rejected");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700::1111"] {
            assert!(is_global(ip.parse().unwrap()), "{ip} must be allowed");
        }
    }
}
