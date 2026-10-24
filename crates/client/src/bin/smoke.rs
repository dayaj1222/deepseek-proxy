//! End-to-end smoke test against the live DeepSeek API.
//!
//! Reads credentials from the project's git-ignored `.env` (`DEEPSEEK_EMAIL_0` /
//! `DEEPSEEK_PASSWORD_0`) so no secrets are committed.
//!
//!     cargo run -p client --bin client-smoke

use client::{Client, CompletionBody, COMPLETION_PATH};
use protocol::{route_stream, Channel};
use std::path::Path;

fn env_from(file: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    text.lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let env = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env");
    let email = env_from(&env, "DEEPSEEK_EMAIL_0").ok_or("DEEPSEEK_EMAIL_0 missing")?;
    let password = env_from(&env, "DEEPSEEK_PASSWORD_0").ok_or("DEEPSEEK_PASSWORD_0 missing")?;

    println!("1. login");
    let client = Client::login(&email, &password, client::get_device_id()).await?;
    println!(
        "   token: {}...",
        &client.token()[..12.min(client.token().len())]
    );

    println!("2. create_chat_session");
    let session = client.create_chat_session().await?;
    println!("   session: {session}");

    println!("3. pow_challenge + solve");
    let ch = client.pow_challenge(COMPLETION_PATH).await?;
    println!(
        "   salt={} expire_at={} difficulty={} algo={}",
        ch.salt, ch.expire_at, ch.difficulty, ch.algorithm
    );
    let header = client.pow_header(COMPLETION_PATH).await?;
    println!(
        "   header: {}... ({} b64 chars)",
        &header[..24.min(header.len())],
        header.len()
    );

    println!("4. stream completion (thinking on, search off)");
    let mut body = CompletionBody::new(&session, "What is 17*23? Answer in one short line.");
    body.thinking_enabled = true;
    let resp = client.stream_completion(&body).await?;
    println!("   status: {}", resp.status());

    let raw = resp.text().await?;
    println!("   raw SSE: {} bytes", raw.len());

    println!("5. route fragments");
    let events = route_stream(&raw);
    let reasoning: String = events
        .iter()
        .filter(|e| e.channel == Channel::Reasoning)
        .map(|e| e.text.as_str())
        .collect();
    let answer: String = events
        .iter()
        .filter(|e| e.channel == Channel::Text)
        .map(|e| e.text.as_str())
        .collect();
    println!("   events={}", events.len());
    println!(
        "   reasoning ({} chars): {}",
        reasoning.len(),
        truncate(&reasoning, 160)
    );
    println!(
        "   answer    ({} chars): {}",
        answer.len(),
        truncate(&answer, 160)
    );

    if answer.is_empty() {
        return Err("no answer text produced".into());
    }
    println!("\nOK");
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect::<String>().replace('\n', " ")
}
