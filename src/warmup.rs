//! Explicit, one-shot warmup without changing the active account.
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::{accounts, storage::Storage, types::*, usage};

#[derive(Serialize)]
pub struct WarmupResult {
    pub id: String,
    pub name: String,
    pub status: &'static str,
    pub message: String,
}

pub async fn run(
    storage: &Storage,
    selector: Option<&str>,
    base_url: &str,
) -> Result<Vec<WarmupResult>> {
    // Use the same validation and official-host normalization as usage queries.
    let usage_url = usage::usage_url(base_url)?;
    let response_url = if let Some(root) = usage_url.strip_suffix("/wham/usage") {
        format!("{root}/codex/responses")
    } else {
        format!("{}/responses", usage_url.strip_suffix("/usage").unwrap())
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("codex-cli")
        .build()?;
    let mut results = Vec::new();
    for row in usage::query(storage, selector, base_url).await? {
        let mut result = WarmupResult {
            id: row.id.clone(),
            name: row.name,
            status: "skipped",
            message: String::new(),
        };
        let window = row
            .windows
            .iter()
            .find(|w| w.limit_window_seconds == Some(18_000));
        if row.status == "error" {
            result.status = "error";
            result.message = row.error.unwrap_or_else(|| "Usage query failed".into());
        } else if row.status == "unsupported" {
            result.message = "API key accounts have no ChatGPT 5h usage".into();
        } else if let Some(window) = window {
            if window.used_percent == 0.0 {
                match send(storage, &row.id, &client, &response_url).await {
                    Ok(()) => {
                        result.status = "warmed";
                        result.message = "gpt-6-luna completed hi".into();
                    }
                    Err(error) => {
                        result.status = "error";
                        result.message = error.to_string();
                    }
                }
            } else {
                result.message = format!("5h usage is {}% (requires 0%)", window.used_percent);
            }
        } else {
            result.message = "5h usage window unavailable".into();
        }
        results.push(result);
    }
    Ok(results)
}

async fn send(storage: &Storage, id: &str, client: &reqwest::Client, url: &str) -> Result<()> {
    // The usage query may have refreshed credentials; never reuse its old snapshot.
    let store = accounts::load_current(storage)?;
    let account = &store.accounts[accounts::resolve(&store, id)?];
    let AuthData::ChatGPT {
        access_token,
        account_id,
        id_token,
        ..
    } = &account.auth_data
    else {
        anyhow::bail!("Account no longer has ChatGPT credentials");
    };
    anyhow::ensure!(
        !access_token.trim().is_empty(),
        "Missing access token; sign in again"
    );
    let account_id = account_id
        .clone()
        .or_else(|| parse_chatgpt_id_token_claims(id_token).account_id)
        .filter(|id| !id.trim().is_empty())
        .context("Missing ChatGPT account ID; sign in again")?;
    let mut response = client.post(url)
        .bearer_auth(access_token)
        .header("chatgpt-account-id", account_id)
        .header("Accept", "text/event-stream")
        .json(&json!({
            "model": "gpt-6-luna",
            "instructions": "Reply briefly.",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "tools": [], "store": false, "stream": true
        }))
        .send().await.context("Warmup request failed")?;
    anyhow::ensure!(
        response.status().is_success(),
        "Warmup API returned HTTP {}",
        response.status().as_u16()
    );
    // Do not treat HTTP 200 or response.created as success: errors can arrive in SSE.
    // Bound both response size and duration. Never print server bodies or credentials.
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("Failed to read warmup response")?
    {
        anyhow::ensure!(
            body.len() + chunk.len() <= 1_048_576,
            "Warmup response exceeded 1 MiB"
        );
        body.extend_from_slice(&chunk);
    }
    validate_completion(std::str::from_utf8(&body).context("Invalid warmup response encoding")?)
}

fn validate_completion(body: &str) -> Result<()> {
    let mut completed = false;
    for event in body.replace("\r\n", "\n").split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let value: Value = serde_json::from_str(&data).context("Invalid warmup event")?;
        match value["type"].as_str() {
            Some("response.completed") => {
                anyhow::ensure!(
                    value["response"]["status"] == "completed",
                    "Warmup response did not complete"
                );
                completed = true;
            }
            Some("error" | "response.failed" | "response.incomplete") => {
                anyhow::bail!("Warmup stream reported a failure")
            }
            _ => {}
        }
    }
    anyhow::ensure!(
        completed,
        "Warmup stream ended without a completed response"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_completed_event_and_rejects_stream_failures() {
        let completed = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\r\n\r\n";
        assert!(validate_completion(completed).is_ok());
        for body in [
            "",
            "data: [DONE]\n\n",
            "data: {\"type\":\"response.created\"}\n\n",
            "data: invalid\n\n",
            "data: {\"type\":\"response.failed\"}\n\n",
            "data: {\"type\":\"response.incomplete\"}\n\n",
        ] {
            assert!(validate_completion(body).is_err());
        }
        assert!(
            validate_completion(&format!("{completed}data: {{\"type\":\"error\"}}\n\n")).is_err()
        );
    }
}
