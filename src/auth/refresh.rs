//! On-demand refresh, called only by an explicit account switch.
use crate::types::{parse_chatgpt_id_token_claims, AuthData, StoredAccount};
use anyhow::{Context, Result};
use base64::Engine;
use chrono::Utc;
use tokio::time::{sleep, Duration};

const DEFAULT_ISSUER: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const EXPIRY_SKEW_SECONDS: i64 = 60;

#[derive(Debug, serde::Deserialize)]
struct RefreshTokenResponse {
    #[serde(default)]
    id_token: Option<String>,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Debug)]
struct TokenRefreshUpdate {
    id_token: String,
    access_token: String,
    refresh_token: String,
    id_token_error: Option<anyhow::Error>,
}

/// Return updated credentials even if the new ID token is unusable. The caller
/// must persist rotated refresh tokens before reporting the deferred error.
pub async fn refresh_if_needed(
    account: &StoredAccount,
) -> Result<(StoredAccount, Option<anyhow::Error>)> {
    if !chatgpt_tokens_need_refresh(account) {
        return Ok((account.clone(), None));
    }
    let AuthData::ChatGPT {
        id_token,
        refresh_token,
        account_id,
        ..
    } = &account.auth_data
    else {
        return Ok((account.clone(), None));
    };
    anyhow::ensure!(
        !refresh_token.is_empty(),
        "Missing refresh token; add this account again with --login"
    );
    let response = refresh_tokens_with_refresh_token(refresh_token).await?;
    let next = merge_refresh_response(
        id_token.clone(),
        refresh_token.clone(),
        response,
        Utc::now().timestamp(),
    );
    let claims = parse_chatgpt_id_token_claims(&next.id_token);
    let mut updated = account.clone();
    updated.email = claims.email.or(updated.email);
    updated.plan_type = claims.plan_type.or(updated.plan_type);
    updated.subscription_expires_at = claims
        .subscription_expires_at
        .or(updated.subscription_expires_at);
    updated.auth_data = AuthData::ChatGPT {
        id_token: next.id_token,
        access_token: next.access_token,
        refresh_token: next.refresh_token,
        account_id: claims.account_id.or_else(|| account_id.clone()),
    };
    Ok((updated, next.id_token_error))
}
fn chatgpt_tokens_need_refresh(account: &StoredAccount) -> bool {
    match &account.auth_data {
        AuthData::ApiKey { .. } => false,
        AuthData::ChatGPT {
            id_token,
            access_token,
            ..
        } => chatgpt_tokens_need_refresh_at(id_token, access_token, Utc::now().timestamp()),
    }
}

fn chatgpt_tokens_need_refresh_at(id_token: &str, access_token: &str, now: i64) -> bool {
    id_token_needs_refresh_at(id_token, now) || token_expired_or_near_expiry_at(access_token, now)
}

fn id_token_needs_refresh_at(token: &str, now: i64) -> bool {
    match parse_jwt_exp(token) {
        Some(expiry) => expiry <= now + EXPIRY_SKEW_SECONDS,
        None => true,
    }
}

fn token_expired_or_near_expiry_at(token: &str, now: i64) -> bool {
    match parse_jwt_exp(token) {
        Some(expiry) => expiry <= now + EXPIRY_SKEW_SECONDS,
        None => false,
    }
}

fn resolve_refreshed_id_token(
    current_id_token: String,
    refreshed_id_token: Option<String>,
    now: i64,
) -> Result<String> {
    match refreshed_id_token {
        Some(id_token) if id_token_needs_refresh_at(&id_token, now) => {
            anyhow::bail!("Token refresh returned an invalid or expired id_token")
        }
        Some(id_token) => Ok(id_token),
        None if id_token_needs_refresh_at(&current_id_token, now) => {
            anyhow::bail!(
                "Token refresh did not return a fresh id_token; sign in to the account again"
            )
        }
        None => Ok(current_id_token),
    }
}

fn merge_refresh_response(
    current_id_token: String,
    current_refresh_token: String,
    refreshed: RefreshTokenResponse,
    now: i64,
) -> TokenRefreshUpdate {
    let (id_token, id_token_error) =
        match resolve_refreshed_id_token(current_id_token.clone(), refreshed.id_token, now) {
            Ok(id_token) => (id_token, None),
            Err(error) => (current_id_token, Some(error)),
        };

    TokenRefreshUpdate {
        id_token,
        access_token: refreshed.access_token,
        refresh_token: refreshed.refresh_token.unwrap_or(current_refresh_token),
        id_token_error,
    }
}

pub(crate) fn parse_jwt_exp(token: &str) -> Option<i64> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }

    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    json.get("exp").and_then(|v| v.as_i64())
}

async fn refresh_tokens_with_refresh_token(refresh_token: &str) -> Result<RefreshTokenResponse> {
    let client = reqwest::Client::new();
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        urlencoding::encode(refresh_token),
        urlencoding::encode(CLIENT_ID),
    );

    let mut last_send_error = None;
    let mut response = None;

    for attempt in 1..=3u8 {
        match client
            .post(format!("{DEFAULT_ISSUER}/oauth/token"))
            .timeout(Duration::from_secs(10))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body.clone())
            .send()
            .await
        {
            Ok(resp) => {
                response = Some(resp);
                break;
            }
            Err(err) => {
                last_send_error = Some(err);
                if attempt < 3 {
                    sleep(Duration::from_millis(250 * u64::from(attempt))).await;
                }
            }
        }
    }

    let response = match response {
        Some(resp) => resp,
        None => {
            let err = last_send_error.context("Failed to send token refresh request")?;
            return Err(err.into());
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Token refresh failed: {status} - {body}");
    }

    response
        .json::<RefreshTokenResponse>()
        .await
        .context("Failed to parse token refresh response")
}

#[cfg(test)]
mod tests {
    use super::{
        chatgpt_tokens_need_refresh_at, merge_refresh_response, resolve_refreshed_id_token,
        RefreshTokenResponse,
    };
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

    fn jwt_with_exp(exp: i64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("header.{payload}.signature")
    }

    #[test]
    fn refresh_required_when_id_token_expired_but_access_token_valid() {
        let now = 1_800_000_000;
        let id_token = jwt_with_exp(now - 3_600);
        let access_token = jwt_with_exp(now + 3_600);

        assert!(chatgpt_tokens_need_refresh_at(
            &id_token,
            &access_token,
            now
        ));
    }

    #[test]
    fn refresh_not_required_when_both_tokens_are_valid() {
        let now = 1_800_000_000;
        let id_token = jwt_with_exp(now + 3_600);
        let access_token = jwt_with_exp(now + 3_600);

        assert!(!chatgpt_tokens_need_refresh_at(
            &id_token,
            &access_token,
            now
        ));
    }

    #[test]
    fn refresh_required_when_access_token_expired() {
        let now = 1_800_000_000;
        let id_token = jwt_with_exp(now + 3_600);
        let access_token = jwt_with_exp(now - 3_600);

        assert!(chatgpt_tokens_need_refresh_at(
            &id_token,
            &access_token,
            now
        ));
    }

    #[test]
    fn expired_id_token_requires_replacement_from_refresh_response() {
        let now = 1_800_000_000;
        let current_id_token = jwt_with_exp(now - 3_600);

        let error = resolve_refreshed_id_token(current_id_token, None, now).unwrap_err();

        assert!(error
            .to_string()
            .contains("did not return a fresh id_token"));
    }

    #[test]
    fn valid_id_token_can_be_preserved_when_refresh_response_omits_it() {
        let now = 1_800_000_000;
        let current_id_token = jwt_with_exp(now + 3_600);

        let resolved = resolve_refreshed_id_token(current_id_token.clone(), None, now).unwrap();

        assert_eq!(resolved, current_id_token);
    }

    #[test]
    fn rotated_refresh_token_is_retained_when_id_token_is_missing() {
        let now = 1_800_000_000;
        let current_id_token = jwt_with_exp(now - 3_600);
        let refreshed = RefreshTokenResponse {
            id_token: None,
            access_token: "new-access".into(),
            refresh_token: Some("rotated-refresh".into()),
        };

        let update = merge_refresh_response(
            current_id_token.clone(),
            "old-refresh".into(),
            refreshed,
            now,
        );

        assert_eq!(update.id_token, current_id_token);
        assert_eq!(update.refresh_token, "rotated-refresh");
        assert!(update.id_token_error.is_some());
    }
}
