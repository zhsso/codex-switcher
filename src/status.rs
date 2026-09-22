//! Local status snapshots. Never refresh credentials or serialize secrets.
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::{auth::refresh::parse_jwt_exp, types::*};

#[derive(Debug, Serialize)]
pub struct AccountStatus {
    pub id: Option<String>,
    pub name: Option<String>,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub auth_mode: AuthMode,
    pub is_active: bool,
    pub credential_status: &'static str,
    pub id_token_expires_at: Option<DateTime<Utc>>,
    pub access_token_expires_at: Option<DateTime<Utc>>,
    pub refresh_token_present: bool,
    pub created_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
}

impl AccountStatus {
    fn from_credentials(data: &AuthData, now: i64) -> Self {
        let mut result = Self {
            id: None,
            name: None,
            email: None,
            plan_type: None,
            auth_mode: AuthMode::ApiKey,
            is_active: false,
            credential_status: "missing",
            id_token_expires_at: None,
            access_token_expires_at: None,
            refresh_token_present: false,
            created_at: None,
            last_used_at: None,
        };
        match data {
            AuthData::ApiKey { key } => {
                if !key.trim().is_empty() {
                    result.credential_status = "api_key_present";
                }
            }
            AuthData::ChatGPT {
                id_token,
                access_token,
                refresh_token,
                ..
            } => {
                result.auth_mode = AuthMode::ChatGPT;
                let claims = parse_chatgpt_id_token_claims(id_token);
                result.email = claims.email;
                result.plan_type = claims.plan_type;
                result.id_token_expires_at =
                    parse_jwt_exp(id_token).and_then(|exp| DateTime::from_timestamp(exp, 0));
                result.access_token_expires_at =
                    parse_jwt_exp(access_token).and_then(|exp| DateTime::from_timestamp(exp, 0));
                result.refresh_token_present = !refresh_token.trim().is_empty();
                let expiry = [result.id_token_expires_at, result.access_token_expires_at]
                    .into_iter()
                    .flatten()
                    .map(|date| date.timestamp())
                    .min();
                result.credential_status =
                    if id_token.trim().is_empty() || access_token.trim().is_empty() {
                        "missing"
                    } else if expiry.is_some_and(|exp| exp <= now) {
                        "expired"
                    } else if expiry.is_some_and(|exp| exp <= now.saturating_add(60)) {
                        "expiring_soon"
                    } else if result.id_token_expires_at.is_none() {
                        "unknown"
                    } else {
                        "not_expired"
                    };
            }
        }
        result
    }

    pub fn from_stored(account: &StoredAccount, active: Option<&str>, now: i64) -> Self {
        let mut result = Self::from_credentials(&account.auth_data, now);
        result.id = Some(account.id.clone());
        result.name = Some(account.name.clone());
        result.email = result.email.or_else(|| account.email.clone());
        result.plan_type = result.plan_type.or_else(|| account.plan_type.clone());
        result.is_active = active == Some(account.id.as_str());
        result.created_at = Some(account.created_at);
        result.last_used_at = account.last_used_at;
        result
    }

    pub fn auth_label(&self) -> &'static str {
        match self.auth_mode {
            AuthMode::ApiKey => "API key",
            AuthMode::ChatGPT => "ChatGPT OAuth",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    fn jwt(exp: i64) -> String {
        format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#))
        )
    }

    #[test]
    fn reports_expiry_without_claiming_server_validity() {
        let now = 1_800_000_000;
        for (id, access, expected) in [
            (jwt(now), jwt(now + 3600), "expired"),
            (jwt(now + 3600), jwt(now - 1), "expired"),
            (jwt(now + 60), "opaque".into(), "expiring_soon"),
            (jwt(now + 61), "opaque".into(), "not_expired"),
            ("malformed".into(), "opaque".into(), "unknown"),
            (jwt(i64::MAX), "opaque".into(), "unknown"),
            (jwt(now + 3600), "".into(), "missing"),
        ] {
            let data = AuthData::ChatGPT {
                id_token: id,
                access_token: access,
                refresh_token: "".into(),
                account_id: None,
            };
            let status = AccountStatus::from_credentials(&data, now);
            assert_eq!(status.credential_status, expected);
            assert!(!status.refresh_token_present);
        }
        assert_eq!(
            AccountStatus::from_credentials(&AuthData::ApiKey { key: "key".into() }, now)
                .credential_status,
            "api_key_present"
        );
    }
}
