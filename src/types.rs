//! Core types for Codex Switcher

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The main storage structure for all accounts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountsStore {
    /// Schema version for future migrations
    pub version: u32,
    /// List of all stored accounts
    pub accounts: Vec<StoredAccount>,
    /// Currently active account ID
    pub active_account_id: Option<String>,
}

impl Default for AccountsStore {
    fn default() -> Self {
        Self {
            version: 1,
            accounts: Vec::new(),
            active_account_id: None,
        }
    }
}

/// A stored account with all its metadata and credentials
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAccount {
    /// Unique identifier (UUID)
    pub id: String,
    /// User-defined display name
    pub name: String,
    /// Email extracted from ID token (for ChatGPT auth)
    pub email: Option<String>,
    /// Plan type: free, plus, pro, team, business, enterprise, edu
    pub plan_type: Option<String>,
    /// Subscription expiration extracted from ChatGPT ID token, when available
    #[serde(default)]
    pub subscription_expires_at: Option<DateTime<Utc>>,
    /// Authentication mode
    pub auth_mode: AuthMode,
    /// Authentication credentials
    pub auth_data: AuthData,
    /// When the account was added
    pub created_at: DateTime<Utc>,
    /// Last time this account was used
    pub last_used_at: Option<DateTime<Utc>>,
}

impl StoredAccount {
    fn resolved_name(
        name: String,
        email: Option<&String>,
        account_id: Option<&String>,
        kind: &str,
    ) -> String {
        if !name.trim().is_empty() {
            return name;
        }
        if let Some(email) = email.filter(|email| !email.trim().is_empty()) {
            return email.clone();
        }
        if let Some(account_id) = account_id.filter(|id| !id.trim().is_empty()) {
            let suffix: String = account_id
                .chars()
                .rev()
                .take(8)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            return format!("{kind} account ({suffix})");
        }
        format!("{kind} account")
    }

    /// Create a new account with API key authentication
    pub fn new_api_key(name: String, api_key: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: Self::resolved_name(name, None, None, "API key"),
            email: None,
            plan_type: None,
            subscription_expires_at: None,
            auth_mode: AuthMode::ApiKey,
            auth_data: AuthData::ApiKey { key: api_key },
            created_at: Utc::now(),
            last_used_at: None,
        }
    }

    /// Create a new account with ChatGPT OAuth authentication
    #[allow(clippy::too_many_arguments)] // Preserve the extracted credential constructor.
    pub fn new_chatgpt(
        name: String,
        email: Option<String>,
        plan_type: Option<String>,
        subscription_expires_at: Option<DateTime<Utc>>,
        id_token: String,
        access_token: String,
        refresh_token: String,
        account_id: Option<String>,
    ) -> Self {
        let name = Self::resolved_name(name, email.as_ref(), account_id.as_ref(), "ChatGPT");
        Self {
            id: Uuid::new_v4().to_string(),
            name,
            email,
            plan_type,
            subscription_expires_at,
            auth_mode: AuthMode::ChatGPT,
            auth_data: AuthData::ChatGPT {
                id_token,
                access_token,
                refresh_token,
                account_id,
            },
            created_at: Utc::now(),
            last_used_at: None,
        }
    }
}

#[cfg(test)]
mod account_name_tests {
    use super::StoredAccount;

    #[test]
    fn blank_name_defaults_to_email() {
        let account = StoredAccount::new_chatgpt(
            "".into(),
            Some("user@example.com".into()),
            None,
            None,
            "id".into(),
            "access".into(),
            "refresh".into(),
            Some("acct-12345678".into()),
        );
        assert_eq!(account.name, "user@example.com");
    }

    #[test]
    fn explicit_name_is_preserved() {
        let account = StoredAccount::new_chatgpt(
            "  My Account  ".into(),
            Some("user@example.com".into()),
            None,
            None,
            "id".into(),
            "access".into(),
            "refresh".into(),
            None,
        );
        assert_eq!(account.name, "  My Account  ");
    }

    #[test]
    fn missing_email_uses_account_id_suffix() {
        let account = StoredAccount::new_chatgpt(
            "".into(),
            None,
            None,
            None,
            "id".into(),
            "access".into(),
            "refresh".into(),
            Some("acct-12345678".into()),
        );
        assert_eq!(account.name, "ChatGPT account (12345678)");
    }

    #[test]
    fn missing_email_and_account_id_use_generic_fallback() {
        let account = StoredAccount::new_chatgpt(
            "".into(),
            None,
            None,
            None,
            "id".into(),
            "access".into(),
            "refresh".into(),
            None,
        );
        assert_eq!(account.name, "ChatGPT account");
    }
}

/// Authentication mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Using an OpenAI API key
    ApiKey,
    /// Using ChatGPT OAuth tokens
    ChatGPT,
}

/// Authentication data (credentials)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthData {
    /// API key authentication
    ApiKey {
        /// The API key
        key: String,
    },
    /// ChatGPT OAuth authentication
    ChatGPT {
        /// JWT ID token containing user info
        id_token: String,
        /// Access token for API calls
        access_token: String,
        /// Refresh token for token renewal
        refresh_token: String,
        /// ChatGPT account ID
        account_id: Option<String>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ChatGptIdTokenClaims {
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub account_id: Option<String>,
    pub subscription_expires_at: Option<DateTime<Utc>>,
}

pub fn parse_chatgpt_id_token_claims(id_token: &str) -> ChatGptIdTokenClaims {
    let parts: Vec<&str> = id_token.split('.').collect();
    if parts.len() != 3 {
        return ChatGptIdTokenClaims::default();
    }

    let payload = match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]) {
        Ok(bytes) => bytes,
        Err(_) => return ChatGptIdTokenClaims::default(),
    };

    let json: serde_json::Value = match serde_json::from_slice(&payload) {
        Ok(value) => value,
        Err(_) => return ChatGptIdTokenClaims::default(),
    };

    let auth_claims = json.get("https://api.openai.com/auth");

    ChatGptIdTokenClaims {
        email: json.get("email").and_then(|v| v.as_str()).map(String::from),
        plan_type: auth_claims
            .and_then(|auth| auth.get("chatgpt_plan_type"))
            .and_then(|v| v.as_str())
            .map(String::from),
        account_id: auth_claims
            .and_then(|auth| auth.get("chatgpt_account_id"))
            .and_then(|v| v.as_str())
            .map(String::from),
        subscription_expires_at: auth_claims
            .and_then(|auth| auth.get("chatgpt_subscription_active_until"))
            .and_then(|v| v.as_str())
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc)),
    }
}

// ============================================================================
// Types for Codex's auth.json format (for compatibility)
// ============================================================================

/// The official Codex auth.json format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthDotJson {
    /// OpenAI API key (for API key auth mode)
    #[serde(rename = "OPENAI_API_KEY", skip_serializing_if = "Option::is_none")]
    pub openai_api_key: Option<String>,
    /// OAuth tokens (for ChatGPT auth mode)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenData>,
    /// Last token refresh timestamp
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<DateTime<Utc>>,
}

/// Token data stored in auth.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenData {
    /// JWT ID token
    pub id_token: String,
    /// Access token
    pub access_token: String,
    /// Refresh token
    pub refresh_token: String,
    /// Account ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// Information needed to finish a browser OAuth login.
#[derive(Debug, Clone)]
pub struct OAuthLoginInfo {
    pub auth_url: String,
    pub callback_port: u16,
}
