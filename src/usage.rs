//! One-shot usage queries using the original ChatGPT usage endpoint.
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::{accounts, auth::refresh::refresh_account, storage::Storage, types::*};

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const ISSUER: &str = "https://auth.openai.com";

#[derive(Debug, Serialize)]
pub struct AccountUsage {
    pub id: String,
    pub name: String,
    pub is_active: bool,
    pub status: &'static str,
    pub plan_type: Option<String>,
    pub windows: Vec<UsageWindow>,
    pub credits: Option<Credits>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UsageWindow {
    pub label: String,
    pub used_percent: f64,
    pub remaining_percent: f64,
    pub limit_window_seconds: Option<i64>,
    pub resets_at: Option<DateTime<Utc>>,
    pub resets_in_seconds: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Credits {
    pub has_credits: Option<bool>,
    pub unlimited: Option<bool>,
    pub balance: Option<String>,
}

#[derive(Deserialize)]
struct Payload {
    plan_type: Option<String>,
    rate_limit: Option<Limits>,
    credits: Option<Credits>,
}

#[derive(Deserialize)]
struct Limits {
    primary_window: Option<Window>,
    secondary_window: Option<Window>,
}

#[derive(Deserialize)]
struct Window {
    used_percent: f64,
    limit_window_seconds: Option<i64>,
    reset_at: Option<i64>,
    reset_after_seconds: Option<i64>,
}

impl Window {
    fn into_usage(self, fallback_label: &str, now: DateTime<Utc>) -> UsageWindow {
        let label = match self.limit_window_seconds {
            Some(18_000) => "5h".into(),
            Some(604_800) => "weekly".into(),
            Some(seconds) => format!("{seconds}s"),
            None => fallback_label.into(),
        };
        let resets_at = self
            .reset_at
            .and_then(|value| DateTime::from_timestamp(value, 0))
            .or_else(|| {
                self.reset_after_seconds.and_then(|value| {
                    now.timestamp()
                        .checked_add(value)
                        .and_then(|value| DateTime::from_timestamp(value, 0))
                })
            });
        UsageWindow {
            label,
            used_percent: self.used_percent,
            remaining_percent: (100.0 - self.used_percent).clamp(0.0, 100.0),
            limit_window_seconds: self.limit_window_seconds,
            resets_at,
            resets_in_seconds: resets_at
                .map(|time| time.timestamp().saturating_sub(now.timestamp()).max(0)),
        }
    }
}

struct UsageClient {
    client: Client,
    url: String,
    issuer: String,
}

impl UsageClient {
    async fn send(&self, account: &StoredAccount) -> Result<reqwest::Response> {
        let AuthData::ChatGPT {
            access_token,
            account_id,
            id_token,
            ..
        } = &account.auth_data
        else {
            anyhow::bail!("Usage is unavailable for API key accounts");
        };
        anyhow::ensure!(
            !access_token.trim().is_empty(),
            "Missing access token; sign in again"
        );
        let mut request = self
            .client
            .get(&self.url)
            .bearer_auth(access_token)
            .header("Accept", "application/json");
        if let Some(id) = account_id
            .clone()
            .or_else(|| parse_chatgpt_id_token_claims(id_token).account_id)
        {
            request = request.header("chatgpt-account-id", id);
        }
        request.send().await.context("Usage request failed")
    }

    async fn fetch(&self, storage: &Storage, account: &StoredAccount) -> Result<Payload> {
        let mut response = self.send(account).await?;
        // Refresh only after 401. A forbidden response is not evidence that a
        // refresh token needs rotating. Serialize credential writes with switch.
        if response.status() == StatusCode::UNAUTHORIZED {
            let _lock = storage.lock()?;
            let mut store = accounts::load_current(storage)?;
            let index = accounts::resolve(&store, &account.id)?;
            let current = &store.accounts[index];
            let old_token = match &account.auth_data {
                AuthData::ChatGPT { access_token, .. } => access_token,
                _ => unreachable!(),
            };
            let token_changed = matches!(&current.auth_data, AuthData::ChatGPT { access_token, .. } if access_token != old_token);
            if token_changed {
                // A running Codex session or another CLI command may already
                // have rotated the credentials. Try those before refreshing.
                response = self.send(current).await?;
            }
            if response.status() == StatusCode::UNAUTHORIZED {
                let (updated, deferred_error) = refresh_account(current, true, &self.issuer)
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "Token refresh failed; sign in again or check your connection"
                        )
                    })?;
                store.accounts[index] = updated.clone();
                storage.save(&store)?;
                if store.active_account_id.as_deref() == Some(&updated.id) {
                    // Preserve a rotated refresh token in both files even when
                    // the returned ID token is unusable.
                    storage.write_auth(&updated)?;
                }
                if let Some(error) = deferred_error {
                    return Err(error);
                }
                response = self.send(&updated).await?;
            }
        }
        let status = response.status();
        anyhow::ensure!(
            status.is_success(),
            "Usage API returned HTTP {}{}",
            status.as_u16(),
            if status == StatusCode::UNAUTHORIZED {
                "; sign in again"
            } else {
                ""
            }
        );
        response.json().await.context("Invalid usage response")
    }
}

// Follow openai/codex's backend-client PathStyle and rate_limit_status_url.
// Parse the hostname exactly instead of matching a URL string prefix.
pub(crate) fn usage_url(base_url: &str) -> Result<String> {
    let mut url = url::Url::parse(base_url).context("Invalid backend base URL")?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "Backend base URL must use HTTP or HTTPS"
    );
    anyhow::ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Backend base URL must not contain credentials, a query, or a fragment"
    );
    let mut path = url.path().trim_end_matches('/').to_string();
    if url.scheme() == "https"
        && matches!(url.host_str(), Some("chatgpt.com" | "chat.openai.com"))
        && !path.contains("/backend-api")
    {
        path.push_str("/backend-api");
    }
    path.push_str(if path.contains("/backend-api") {
        "/wham/usage"
    } else {
        "/api/codex/usage"
    });
    url.set_path(&path);
    Ok(url.into())
}

pub async fn query(
    storage: &Storage,
    selector: Option<&str>,
    base_url: &str,
) -> Result<Vec<AccountUsage>> {
    let client = UsageClient {
        client: Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("codex-cli")
            .build()?,
        url: usage_url(base_url)?,
        issuer: ISSUER.into(),
    };
    query_with_client(storage, selector, &client).await
}

async fn query_with_client(
    storage: &Storage,
    selector: Option<&str>,
    client: &UsageClient,
) -> Result<Vec<AccountUsage>> {
    let store = accounts::load_current(storage)?;
    let selected = match selector {
        Some(selector) => vec![&store.accounts[accounts::resolve(&store, selector)?]],
        None => store.accounts.iter().collect(),
    };
    let mut rows = Vec::new();
    for account in selected {
        let mut row = AccountUsage {
            id: account.id.clone(),
            name: account.name.clone(),
            is_active: store.active_account_id.as_deref() == Some(&account.id),
            status: "ok",
            plan_type: None,
            windows: Vec::new(),
            credits: None,
            error: None,
        };
        if matches!(account.auth_data, AuthData::ApiKey { .. }) {
            row.status = "unsupported";
            row.error =
                Some("Usage and reset information is unavailable for API key accounts".into());
        } else {
            match client.fetch(storage, account).await {
                Ok(payload) => {
                    row.plan_type = payload.plan_type;
                    row.credits = payload.credits;
                    if let Some(limits) = payload.rate_limit {
                        let now = Utc::now();
                        for (window, label) in [
                            (limits.primary_window, "primary"),
                            (limits.secondary_window, "secondary"),
                        ] {
                            if let Some(window) = window {
                                row.windows.push(window.into_usage(label, now));
                            }
                        }
                        row.windows
                            .sort_by_key(|window| window.limit_window_seconds.unwrap_or(i64::MAX));
                    }
                }
                Err(error) => {
                    row.status = "error";
                    row.error = Some(error.to_string());
                }
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use serde_json::json;
    use std::thread;
    use tiny_http::{Response, Server};

    #[test]
    fn selects_usage_paths_like_the_official_backend_client() {
        for (base, expected) in [
            (
                "https://chatgpt.com",
                "https://chatgpt.com/backend-api/wham/usage",
            ),
            (
                "https://chatgpt.com/backend-api/",
                "https://chatgpt.com/backend-api/wham/usage",
            ),
            (
                "https://chat.openai.com/",
                "https://chat.openai.com/backend-api/wham/usage",
            ),
            (
                "https://example.test/",
                "https://example.test/api/codex/usage",
            ),
            (
                "https://example.test/backend-api/",
                "https://example.test/backend-api/wham/usage",
            ),
            (
                "https://chatgpt.com.example.test",
                "https://chatgpt.com.example.test/api/codex/usage",
            ),
        ] {
            assert_eq!(usage_url(base).unwrap(), expected);
        }
        for invalid in [
            "not a URL",
            "file:///tmp",
            "https://user:pass@example.test",
            "https://example.test?key=value",
            "https://example.test/#fragment",
        ] {
            assert!(usage_url(invalid).is_err());
        }
    }

    fn jwt() -> String {
        let payload = json!({"exp": Utc::now().timestamp()+3600,
            "https://api.openai.com/auth": {"chatgpt_account_id": "workspace"}});
        format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(payload.to_string())
        )
    }

    fn setup() -> (tempfile::TempDir, Storage, StoredAccount) {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(
            Some(dir.path().join("store")),
            Some(dir.path().join("codex")),
        )
        .unwrap();
        let account = StoredAccount::new_chatgpt(
            "test".into(),
            None,
            None,
            None,
            jwt(),
            "test-access".into(),
            "test-refresh".into(),
            Some("workspace".into()),
        );
        storage
            .save(&AccountsStore {
                accounts: vec![account.clone()],
                ..AccountsStore::default()
            })
            .unwrap();
        (dir, storage, account)
    }

    fn mock(
        responses: Vec<(u16, String)>,
    ) -> (UsageClient, thread::JoinHandle<Vec<(String, String)>>) {
        let server = Server::http("127.0.0.1:0").unwrap();
        let root = format!("http://{}", server.server_addr());
        let thread = thread::spawn(move || {
            let mut requests = Vec::new();
            for (code, body) in responses {
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .expect("missing request");
                let authorization = request
                    .headers()
                    .iter()
                    .find(|header| header.field.equiv("authorization"))
                    .map(|header| header.value.as_str().to_string())
                    .unwrap_or_default();
                if request.url() == "/usage" {
                    assert_eq!(
                        request
                            .headers()
                            .iter()
                            .find(|header| header.field.equiv("chatgpt-account-id"))
                            .unwrap()
                            .value
                            .as_str(),
                        "workspace"
                    );
                }
                requests.push((request.url().to_string(), authorization));
                request
                    .respond(Response::from_string(body).with_status_code(code))
                    .unwrap();
            }
            requests
        });
        (
            UsageClient {
                client: Client::builder()
                    .no_proxy()
                    .timeout(Duration::from_secs(2))
                    .build()
                    .unwrap(),
                url: format!("{root}/usage"),
                issuer: root,
            },
            thread,
        )
    }

    fn payload() -> String {
        json!({"plan_type":"plus", "rate_limit": {
            "primary_window": {"used_percent": 80, "limit_window_seconds":604800, "reset_at":1800000000},
            "secondary_window": {"used_percent": 25, "limit_window_seconds":18000, "reset_after_seconds":60}},
            "credits": {"has_credits":true,"unlimited":false,"balance":"10"}}).to_string()
    }

    #[tokio::test]
    async fn queries_all_and_keeps_successes_when_another_account_fails() {
        let (_dir, storage, account) = setup();
        let mut store = storage.load().unwrap();
        let mut second = account.clone();
        second.id = "second".into();
        second.name = "second".into();
        store.accounts.push(second);
        storage.save(&store).unwrap();
        let (client, server) = mock(vec![
            (200, payload()),
            (403, "test-access must never be printed".into()),
        ]);
        let rows = query_with_client(&storage, None, &client).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].windows[0].label, "5h");
        assert_eq!(rows[0].windows[0].remaining_percent, 75.0);
        assert_eq!(rows[0].windows[0].resets_in_seconds, Some(60));
        assert_eq!(rows[0].windows[1].label, "weekly");
        assert_eq!(
            rows[0].windows[1].resets_at.unwrap().timestamp(),
            1800000000
        );
        assert_eq!(rows[1].status, "error");
        assert!(rows[1].error.as_ref().unwrap().contains("403"));
        assert_eq!(server.join().unwrap().len(), 2); // No refresh on 403.
        let output = serde_json::to_string(&rows).unwrap();
        assert!(!output.contains("test-access"));
        assert!(!output.contains("test-refresh"));
    }

    #[tokio::test]
    async fn selection_fetches_only_one_account_and_missing_windows_stay_unknown() {
        let (_dir, storage, account) = setup();
        let mut store = storage.load().unwrap();
        store
            .accounts
            .push(StoredAccount::new_api_key("api".into(), "key".into()));
        storage.save(&store).unwrap();
        let (client, server) = mock(vec![(
            200,
            r#"{"plan_type":"free","rate_limit":null}"#.into(),
        )]);
        let rows = query_with_client(&storage, Some(&account.id), &client)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "ok");
        assert!(rows[0].windows.is_empty());
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unauthorized_refreshes_and_persists_tokens_without_switching_accounts() {
        let (_dir, storage, account) = setup();
        let active = StoredAccount::new_api_key("active".into(), "current-key".into());
        let mut store = storage.load().unwrap();
        store.active_account_id = Some(active.id.clone());
        store.accounts.push(active.clone());
        storage.save(&store).unwrap();
        storage.write_auth(&active).unwrap();
        let before = std::fs::read(storage.auth_path()).unwrap();
        let refreshed =
            json!({"id_token":jwt(), "access_token":"new-access", "refresh_token":"new-refresh"})
                .to_string();
        let (client, server) = mock(vec![(401, "{}".into()), (200, refreshed), (200, payload())]);
        let rows = query_with_client(&storage, Some(&account.id), &client)
            .await
            .unwrap();
        assert_eq!(rows[0].status, "ok");
        let requests = server.join().unwrap();
        assert_eq!(requests[1].0, "/oauth/token");
        assert_eq!(requests[2].1, "Bearer new-access");
        let store = storage.load().unwrap();
        assert_eq!(store.active_account_id, Some(active.id));
        assert!(
            matches!(&store.accounts[0].auth_data, AuthData::ChatGPT {refresh_token, ..} if refresh_token == "new-refresh")
        );
        assert_eq!(std::fs::read(storage.auth_path()).unwrap(), before);
    }

    #[test]
    fn weekly_only_unknown_and_elapsed_reset_windows_are_not_fabricated() {
        let now = DateTime::from_timestamp(1800000000, 0).unwrap();
        let window = Window {
            used_percent: 100.,
            limit_window_seconds: Some(604800),
            reset_at: Some(1799999999),
            reset_after_seconds: None,
        }
        .into_usage("primary", now);
        assert_eq!(window.label, "weekly");
        assert_eq!(window.resets_in_seconds, Some(0));
        assert_eq!(window.remaining_percent, 0.);
        let unknown = Window {
            used_percent: 1.,
            limit_window_seconds: None,
            reset_at: None,
            reset_after_seconds: None,
        }
        .into_usage("primary", now);
        assert!(unknown.resets_at.is_none());
        assert!(unknown.resets_in_seconds.is_none());
    }
}
