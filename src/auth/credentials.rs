use crate::types::{parse_chatgpt_id_token_claims, AuthDotJson, StoredAccount};
use anyhow::{Context, Result};

/// Import an account from auth.json file contents.
pub fn import_from_auth_json_contents(
    content: &str,
    account_name: String,
) -> Result<StoredAccount> {
    let auth: AuthDotJson =
        serde_json::from_str(content).context("Failed to parse auth.json contents")?;
    let account_name = account_name.trim().to_string();

    // Determine auth mode and create account
    if let Some(api_key) = auth.openai_api_key.filter(|key| !key.trim().is_empty()) {
        Ok(StoredAccount::new_api_key(account_name, api_key))
    } else if let Some(tokens) = auth.tokens {
        anyhow::ensure!(
            !tokens.id_token.trim().is_empty()
                && !tokens.access_token.trim().is_empty()
                && !tokens.refresh_token.trim().is_empty(),
            "auth.json contains empty OAuth tokens"
        );
        let claims = parse_chatgpt_id_token_claims(&tokens.id_token);

        Ok(StoredAccount::new_chatgpt(
            account_name,
            claims.email,
            claims.plan_type,
            claims.subscription_expires_at,
            tokens.id_token,
            tokens.access_token,
            tokens.refresh_token,
            claims.account_id.or(tokens.account_id),
        ))
    } else {
        anyhow::bail!("auth.json contains neither API key nor tokens");
    }
}

#[cfg(test)]
mod tests {
    use super::import_from_auth_json_contents;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use serde_json::json;

    fn auth_json(payload: serde_json::Value, account_id: &str) -> String {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        serde_json::json!({
            "tokens": {
                "id_token": format!("header.{payload}.signature"),
                "access_token": "access",
                "refresh_token": "refresh",
                "account_id": account_id
            }
        })
        .to_string()
    }

    #[test]
    fn import_blank_name_uses_email() {
        let account = import_from_auth_json_contents(
            &auth_json(json!({"email": "imported@example.com"}), "acct-import"),
            "".into(),
        )
        .unwrap();
        assert_eq!(account.name, "imported@example.com");
    }

    #[test]
    fn import_explicit_name_is_trimmed() {
        let account = import_from_auth_json_contents(
            &auth_json(json!({"email": "imported@example.com"}), "acct-import"),
            "  Imported Account  ".into(),
        )
        .unwrap();
        assert_eq!(account.name, "Imported Account");
    }

    #[test]
    fn import_without_email_uses_account_id_fallback() {
        let account =
            import_from_auth_json_contents(&auth_json(json!({}), "acct-87654321"), "".into())
                .unwrap();
        assert_eq!(account.name, "ChatGPT account (87654321)");
    }
}
