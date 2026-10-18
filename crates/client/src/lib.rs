//! DeepSeek private-API client: auth, PoW header, streaming completions.
//!
//! Port of `aiodeepseek`'s `_AuthClient` + `_BaseClient` + `_ChatClient`.
//! The proxy is the only consumer; there is deliberately no `Conversation`
//! abstraction (that was the main source of monkey-patching in Python).

pub mod device_ids;

pub use device_ids::get_device_id;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

pub const BASE_URL: &str = "https://chat.deepseek.com";
pub const COMPLETION_PATH: &str = "/api/v0/chat/completion";
const LOGIN_PATH: &str = "/api/v0/users/login";
const SESSION_CREATE_PATH: &str = "/api/v0/chat_session/create";
const POW_CHALLENGE_PATH: &str = "/api/v0/chat/create_pow_challenge";

/// Headers the shipped iOS client sends. Kept byte-identical to Python so the
/// server sees the same client identity.
pub fn base_headers() -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut h = HeaderMap::new();
    let pairs = [
        ("Content-Type", "application/json"),
        ("Accept", "application/json"),
        (
            "User-Agent",
            "DeepSeek/2 CFNetwork/1568.100.1 Darwin/24.0.0",
        ),
        ("x-client-platform", "ios"),
        ("x-client-version", "2.0.4"),
        ("x-client-bundle-id", "com.deepseek.chat"),
        ("x-client-locale", "en_US"),
        ("x-client-timezone-offset", "3600"),
    ];
    for (k, v) in pairs {
        h.insert(
            HeaderName::from_static(k.to_ascii_lowercase().leak()),
            HeaderValue::from_static(v),
        );
    }
    h
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api error {code}: {message}")]
    Api { code: i64, message: String },
    #[error("unexpected response shape: {0}")]
    Shape(String),
    #[error("pow: no nonce found within difficulty {difficulty}")]
    PowUnsolved { difficulty: u64 },
    #[error("invalid challenge: {0}")]
    BadChallenge(String),
}

/// A proof-of-work challenge from `/api/v0/chat/create_pow_challenge`.
#[derive(Debug, Clone, Deserialize)]
pub struct Challenge {
    pub salt: String,
    #[serde(deserialize_with = "de_string")]
    pub expire_at: String,
    #[serde(deserialize_with = "de_u64")]
    pub difficulty: u64,
    pub challenge: String,
    #[serde(default)]
    pub algorithm: String,
    #[serde(default)]
    pub signature: String,
}

fn de_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    // The server sends this as a JSON string today but it is a number in some
    // responses; accept both.
    let v = Value::deserialize(d)?;
    match v {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n.to_string()),
        other => Err(serde::de::Error::custom(format!(
            "expected string or number, got {other}"
        ))),
    }
}

fn de_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = Value::deserialize(d)?;
    match v {
        Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| serde::de::Error::custom("difficulty not a u64")),
        Value::String(s) => s
            .parse()
            .map_err(|_| serde::de::Error::custom("difficulty not numeric")),
        other => Err(serde::de::Error::custom(format!(
            "expected number, got {other}"
        ))),
    }
}

/// Body sent to `/api/v0/chat/completion`.
#[derive(Debug, Serialize)]
pub struct CompletionBody {
    pub chat_session_id: String,
    pub parent_message_id: Option<i64>,
    pub prompt: String,
    pub ref_file_ids: Vec<String>,
    pub thinking_enabled: bool,
    pub search_enabled: bool,
    pub audio_id: Option<String>,
    pub preempt: bool,
    pub model_type: String,
}

impl CompletionBody {
    pub fn new(session_id: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            chat_session_id: session_id.into(),
            parent_message_id: None,
            prompt: prompt.into(),
            ref_file_ids: Vec::new(),
            thinking_enabled: false,
            search_enabled: false,
            audio_id: None,
            preempt: false,
            model_type: "default".into(),
        }
    }
}

/// Authenticated DeepSeek client.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    token: String,
}

impl Client {
    fn new(token: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .danger_accept_invalid_certs(false)
                .build()
                .expect("reqwest client"),
            token,
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    fn authed_headers(&self) -> reqwest::header::HeaderMap {
        let mut h = base_headers();
        h.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.token))
                .expect("token header"),
        );
        h
    }

    /// `POST /api/v0/users/login` -> bearer token.
    ///
    /// `device_id` must come from [`get_device_id`]; the server rejects
    /// fabricated values with `RISK_DEVICE_DETECTED`.
    pub async fn login(email: &str, password: &str, device_id: &str) -> Result<Self, ClientError> {
        let http = reqwest::Client::new();
        let body = json!({
            "email": email,
            "password": password,
            "device_id": device_id,
            "os": "ios",
        });
        let resp = http
            .post(format!("{BASE_URL}{LOGIN_PATH}"))
            .headers(base_headers())
            .json(&body)
            .send()
            .await?;
        let data: Value = resp.json().await?;
        let token = data
            .pointer("/data/biz_data/user/token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ClientError::Shape(format!("login: {data}")))?
            .to_string();
        Ok(Self::new(token))
    }

    /// `POST /api/v0/chat_session/create` -> session id.
    pub async fn create_chat_session(&self) -> Result<String, ClientError> {
        let resp = self
            .http
            .post(format!("{BASE_URL}{SESSION_CREATE_PATH}"))
            .headers(self.authed_headers())
            .json(&json!({ "character_id": 1 }))
            .send()
            .await?;
        let data: Value = resp.json().await?;
        data.pointer("/data/biz_data/chat_session/id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| ClientError::Shape(format!("create_session: {data}")))
    }

    /// Fetch a fresh PoW challenge for `target_path`.
    pub async fn pow_challenge(&self, target_path: &str) -> Result<Challenge, ClientError> {
        let resp = self
            .http
            .post(format!("{BASE_URL}{POW_CHALLENGE_PATH}"))
            .headers(self.authed_headers())
            .json(&json!({ "target_path": target_path }))
            .send()
            .await?;
        let data: Value = resp.json().await?;
        let ch = data
            .pointer("/data/biz_data/challenge")
            .or_else(|| data.pointer("/data/biz_data"))
            .ok_or_else(|| ClientError::Shape(format!("pow_challenge: {data}")))?;
        serde_json::from_value(ch.clone())
            .map_err(|e| ClientError::BadChallenge(format!("{e}: {ch}")))
    }

    /// Solve a challenge and return the base64 `X-DS-PoW-Response` value.
    ///
    /// The JSON key order matches Python (`algorithm, challenge, salt,
    /// signature, answer, target_path`) and uses compact separators, so the
    /// header is byte-identical.
    pub async fn pow_header(&self, target_path: &str) -> Result<String, ClientError> {
        let ch = self.pow_challenge(target_path).await?;
        let base = pow::pow_base(&ch.salt, &ch.expire_at);
        let nonce = pow::solve_hex(&base, &ch.challenge, ch.difficulty).ok_or(
            ClientError::PowUnsolved {
                difficulty: ch.difficulty,
            },
        )?;

        let algorithm = if ch.algorithm.is_empty() {
            "DeepSeekHashV1"
        } else {
            ch.algorithm.as_str()
        };
        let payload = format!(
            concat!(
                "{{\"algorithm\":{},\"challenge\":{},\"salt\":{},",
                "\"signature\":{},\"answer\":{},\"target_path\":{}}}"
            ),
            serde_json::to_string(algorithm)?,
            serde_json::to_string(&ch.challenge)?,
            serde_json::to_string(&ch.salt)?,
            serde_json::to_string(&ch.signature)?,
            nonce,
            serde_json::to_string(target_path)?,
        );
        Ok(base64::engine::general_purpose::STANDARD.encode(payload))
    }

    /// Start a streaming completion. Caller drives the byte stream.
    pub async fn stream_completion(
        &self,
        body: &CompletionBody,
    ) -> Result<reqwest::Response, ClientError> {
        let pow = self.pow_header(COMPLETION_PATH).await?;
        let mut headers = self.authed_headers();
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            reqwest::header::HeaderName::from_static("x-ds-pow-response"),
            reqwest::header::HeaderValue::from_str(&pow).expect("pow header"),
        );
        let resp = self
            .http
            .post(format!("{BASE_URL}{COMPLETION_PATH}"))
            .headers(headers)
            .json(body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ClientError::Api {
                code: status.as_u16() as i64,
                message: text.chars().take(400).collect(),
            });
        }
        Ok(resp)
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(e: serde_json::Error) -> Self {
        ClientError::Shape(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_accepts_string_and_number_expire_at() {
        let a: Challenge = serde_json::from_value(json!({
            "salt": "s", "expire_at": "1792333031301", "difficulty": 144000,
            "challenge": "ab", "algorithm": "DeepSeekHashV1", "signature": "x"
        }))
        .unwrap();
        assert_eq!(a.expire_at, "1792333031301");
        assert_eq!(a.difficulty, 144000);

        let b: Challenge = serde_json::from_value(json!({
            "salt": "s", "expire_at": 1792333031301u64, "difficulty": "144000",
            "challenge": "ab"
        }))
        .unwrap();
        assert_eq!(b.expire_at, "1792333031301");
        assert_eq!(b.difficulty, 144000);
    }
}
