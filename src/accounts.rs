use anyhow::{Context, Result};

use crate::auth::{refresh::refresh_if_needed, sync::sync_active_account_tokens};
use crate::storage::Storage;
use crate::types::{
    parse_chatgpt_id_token_claims, AccountsStore, AuthData, AuthDotJson, StoredAccount,
};

/// Resolve exact IDs first, then exact names. Never guess between ambiguous names.
pub fn resolve(store: &AccountsStore, selector: &str) -> Result<usize> {
    if let Some(index) = store
        .accounts
        .iter()
        .position(|account| account.id == selector)
    {
        return Ok(index);
    }
    let matches: Vec<_> = store
        .accounts
        .iter()
        .enumerate()
        .filter(|(_, account)| account.name == selector)
        .collect();
    match matches.as_slice() {
        [(index, _)] => Ok(*index),
        [] => anyhow::bail!(
            "Account '{selector}' not found; use `codex-switcher list` to see accounts"
        ),
        _ => anyhow::bail!("Account name '{selector}' is ambiguous; use its full ID"),
    }
}

fn matches_auth(account: &StoredAccount, auth: &AuthDotJson) -> bool {
    match &account.auth_data {
        AuthData::ApiKey { key } => auth.openai_api_key.as_ref() == Some(key),
        AuthData::ChatGPT {
            id_token,
            account_id,
            ..
        } => {
            if auth
                .openai_api_key
                .as_ref()
                .is_some_and(|key| !key.is_empty())
            {
                return false;
            }
            let Some(tokens) = &auth.tokens else {
                return false;
            };
            if *id_token == tokens.id_token {
                return true;
            }
            let stored = parse_chatgpt_id_token_claims(id_token);
            let current = parse_chatgpt_id_token_claims(&tokens.id_token);
            let stored_id = stored.account_id.or_else(|| account_id.clone());
            let current_id = current.account_id.or_else(|| tokens.account_id.clone());
            stored_id.is_some()
                && stored_id == current_id
                && (stored.email.is_none()
                    || current.email.is_none()
                    || stored.email == current.email)
        }
    }
}

/// Recover the actual active identity even after an external login or a partially
/// completed switch; only ingest live tokens when the identities match.
pub fn reconcile(store: &mut AccountsStore, auth: Option<&AuthDotJson>) {
    let Some(auth) = auth else {
        store.active_account_id = None;
        return;
    };
    let current_matches = store.accounts.iter().any(|account| {
        store.active_account_id.as_deref() == Some(&account.id) && matches_auth(account, auth)
    });
    if !current_matches {
        let matches: Vec<_> = store
            .accounts
            .iter()
            .filter(|account| matches_auth(account, auth))
            .collect();
        store.active_account_id = match matches.as_slice() {
            [account] => Some(account.id.clone()),
            _ => None,
        };
    }
    sync_active_account_tokens(store, auth);
}

pub fn load_current(storage: &Storage) -> Result<AccountsStore> {
    let mut store = storage.load()?;
    reconcile(&mut store, storage.read_auth()?.as_ref());
    Ok(store)
}

pub fn validate_name(store: &AccountsStore, name: &str, except_id: Option<&str>) -> Result<()> {
    anyhow::ensure!(!name.trim().is_empty(), "Account name must not be blank");
    anyhow::ensure!(
        !store
            .accounts
            .iter()
            .any(|account| Some(account.id.as_str()) != except_id && account.name == name),
        "An account with name '{name}' already exists"
    );
    Ok(())
}

pub fn add(storage: &Storage, account: StoredAccount) -> Result<()> {
    let _lock = storage.lock()?;
    let mut store = load_current(storage)?;
    validate_name(&store, &account.name, None)?;
    store.accounts.push(account);
    reconcile(&mut store, storage.read_auth()?.as_ref());
    storage.save(&store)
}

/// Removal only forgets the saved profile; it does not log Codex out or select a
/// different account behind the user's back.
pub fn remove(storage: &Storage, selector: &str) -> Result<String> {
    let _lock = storage.lock()?;
    let mut store = load_current(storage)?;
    let index = resolve(&store, selector)?;
    let removed = store.accounts.remove(index);
    if store.active_account_id.as_deref() == Some(&removed.id) {
        store.active_account_id = None;
    }
    storage.save(&store)?;
    Ok(removed.name)
}

/// Credential edits retain the profile ID, creation time and display name.
/// Returns the name and whether the live auth.json was rewritten.
pub fn edit(
    storage: &Storage,
    selector: &str,
    name: Option<String>,
    replacement: Option<StoredAccount>,
) -> Result<(String, bool)> {
    let _lock = storage.lock()?;
    let mut store = load_current(storage)?;
    let index = resolve(&store, selector)?;
    let account = &store.accounts[index];
    let name = name.unwrap_or_else(|| account.name.clone());
    validate_name(&store, &name, Some(&account.id))?;
    let mut updated = replacement.clone().unwrap_or_else(|| account.clone());
    updated.id = account.id.clone();
    updated.created_at = account.created_at;
    updated.last_used_at = account.last_used_at;
    updated.name = name.clone();
    // Save credentials first: if writing auth.json fails, a retry can recover them.
    let update_live =
        replacement.is_some() && store.active_account_id.as_deref() == Some(&updated.id);
    store.accounts[index] = updated.clone();
    storage.save(&store)?;
    if update_live {
        storage
            .write_auth(&updated)
            .context("Profile saved, but updating the active auth.json failed; run switch again")?;
    }
    Ok((name, update_live))
}

pub async fn switch(storage: &Storage, selector: &str) -> Result<String> {
    let _lock = storage.lock()?;
    let mut store = load_current(storage)?;
    let index = resolve(&store, selector)?;
    // Preserve live rotated tokens before any network request or auth.json write.
    storage.save(&store)?;
    let (account, deferred_error) = refresh_if_needed(&store.accounts[index]).await?;
    store.accounts[index] = account.clone();
    storage.save(&store)?;
    if let Some(error) = deferred_error {
        return Err(error);
    }
    storage.write_auth(&account)?;
    store.active_account_id = Some(account.id.clone());
    store.accounts[index].last_used_at = Some(chrono::Utc::now());
    storage.save(&store)?;
    Ok(account.name)
}
