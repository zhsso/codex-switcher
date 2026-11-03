use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::auth::{
    refresh::refresh_if_needed,
    sync::{sync_account_tokens, sync_active_account_tokens},
};
use crate::storage::Storage;
use crate::types::{
    parse_chatgpt_id_token_claims, AccountsStore, AuthData, AuthDotJson, StoredAccount,
};
use crate::workspace;

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

/// The saved account an auth.json belongs to, when that is unambiguous.
pub fn account_for_auth<'a>(
    store: &'a AccountsStore,
    auth: &AuthDotJson,
) -> Option<&'a StoredAccount> {
    let mut matches = store
        .accounts
        .iter()
        .filter(|account| matches_auth(account, auth));
    let account = matches.next()?;
    matches.next().is_none().then_some(account)
}

/// Other Codex homes currently logged in to this account.
pub fn peer_homes_using(storage: &Storage, account: &StoredAccount) -> Vec<PathBuf> {
    workspace::peer_auths(storage)
        .into_iter()
        .filter(|(_, auth)| matches_auth(account, auth))
        .map(|(home, _)| home)
        .collect()
}

/// A rotating refresh token only works in one place. API keys can be shared.
fn holder<'a>(account: &StoredAccount, peers: &'a [(PathBuf, AuthDotJson)]) -> Option<&'a Path> {
    if !matches!(account.auth_data, AuthData::ChatGPT { .. }) {
        return None;
    }
    peers
        .iter()
        .find(|(_, auth)| matches_auth(account, auth))
        .map(|(home, _)| home.as_path())
}

/// Tokens rotate wherever the account is running, so collect them from every
/// known home. The current home goes last: its auth.json wins any conflict.
pub fn load_current(storage: &Storage) -> Result<AccountsStore> {
    let mut store = storage.load()?;
    for (_, auth) in workspace::peer_auths(storage) {
        let Some(id) = account_for_auth(&store, &auth).map(|account| account.id.clone()) else {
            continue;
        };
        if let Some(account) = store.accounts.iter_mut().find(|account| account.id == id) {
            sync_account_tokens(account, &auth);
        }
    }
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
/// Returns the name and the Codex homes whose live auth.json was rewritten.
pub fn edit(
    storage: &Storage,
    selector: &str,
    name: Option<String>,
    replacement: Option<StoredAccount>,
) -> Result<(String, Vec<PathBuf>)> {
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
    let mut live_homes = Vec::new();
    if replacement.is_some() {
        if store.active_account_id.as_deref() == Some(&updated.id) {
            live_homes.push(storage.codex_home.clone());
        }
        live_homes.extend(peer_homes_using(storage, account));
    }
    store.accounts[index] = updated.clone();
    storage.save(&store)?;
    for home in &live_homes {
        storage
            .with_home(home.clone())
            .write_auth(&updated)
            .context("Profile saved, but updating the active auth.json failed; run switch again")?;
    }
    Ok((name, live_homes))
}

pub async fn switch(storage: &Storage, selector: &str) -> Result<String> {
    switch_target(storage, Some(selector)).await
}

/// Cycle in saved order, starting at the first account if none is active.
pub async fn switch_next(storage: &Storage) -> Result<String> {
    switch_target(storage, None).await
}

async fn switch_target(storage: &Storage, selector: Option<&str>) -> Result<String> {
    let _lock = storage.lock()?;
    let mut store = load_current(storage)?;
    let peers = workspace::peer_auths(storage);
    let index = match selector {
        Some(selector) => {
            let index = resolve(&store, selector)?;
            if let Some(home) = holder(&store.accounts[index], &peers) {
                anyhow::bail!(
                    "Account '{}' is already in use by {}; switch that to another account first, because using it in two places would invalidate its login",
                    store.accounts[index].name,
                    workspace::describe(storage, home)
                );
            }
            index
        }
        None => {
            let count = store.accounts.len();
            anyhow::ensure!(
                count > 0,
                "No saved accounts; run `codex-switcher add NAME` first"
            );
            let start = store
                .accounts
                .iter()
                .position(|account| store.active_account_id.as_deref() == Some(&account.id))
                .map_or(0, |index| index + 1);
            (0..count)
                .map(|offset| (start + offset) % count)
                .find(|&index| holder(&store.accounts[index], &peers).is_none())
                .context("Every saved account is in use by another workspace")?
        }
    };
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
