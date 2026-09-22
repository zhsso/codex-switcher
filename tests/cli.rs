use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use codex_switcher::storage::Storage;
use serde_json::{json, Value};
use tempfile::TempDir;

struct Fixture(TempDir);

impl Fixture {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    fn storage(&self) -> Storage {
        Storage::new(
            Some(self.0.path().join("store")),
            Some(self.0.path().join("codex")),
        )
        .unwrap()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codex-switcher"));
        command
            .arg("--store-dir")
            .arg(self.storage().directory)
            .arg("--codex-home")
            .arg(self.storage().codex_home)
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn key(&self, name: &str, key: &str) {
        let mut child = self
            .command(&["add", name, "--api-key-stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(key.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains(key));
    }

    fn write_auth(&self, auth: &Value) {
        fs::create_dir_all(self.storage().codex_home).unwrap();
        fs::write(self.storage().auth_path(), auth.to_string()).unwrap();
    }

    fn auth(&self) -> Value {
        serde_json::from_slice(&fs::read(self.storage().auth_path()).unwrap()).unwrap()
    }

    fn list(&self) -> Value {
        serde_json::from_str(&self.ok(&["list", "--json"])).unwrap()
    }
}

fn oauth_auth(identity: &str, suffix: &str) -> Value {
    let claims = json!({
        "exp": chrono::Utc::now().timestamp() + 3600,
        "email": format!("{identity}@example.com"),
        "https://api.openai.com/auth": {"chatgpt_account_id": identity}
    });
    json!({"tokens": {
        "id_token": format!("header.{}.{suffix}", URL_SAFE_NO_PAD.encode(claims.to_string())),
        "access_token": format!("access-{suffix}"),
        "refresh_token": format!("refresh-{suffix}"),
        "account_id": identity
    }})
}

#[test]
fn account_lifecycle_by_name_and_id() {
    let fixture = Fixture::new();
    fixture.key("personal", "sk-personal-test");
    fixture.key("work", "sk-work-test");
    let rows = fixture.list();
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert!(!rows[0]["is_active"].as_bool().unwrap());
    assert!(!fixture.storage().auth_path().exists());
    fixture.ok(&["switch", "personal"]);
    assert_eq!(fixture.auth()["OPENAI_API_KEY"], "sk-personal-test");
    fixture.ok(&["edit", "personal", "--name", "private"]);
    let rows = fixture.list();
    assert_eq!(rows[0]["name"], "private");
    assert_eq!(rows[0]["is_active"], true);
    fixture.ok(&["switch", rows[1]["id"].as_str().unwrap()]);
    assert_eq!(fixture.auth()["OPENAI_API_KEY"], "sk-work-test");
    fixture.ok(&["remove", "private"]);
    fixture.ok(&["remove", "work"]);
    assert_eq!(fixture.list(), json!([]));
    // Removing an active profile must not silently activate another or log out.
    assert_eq!(fixture.auth()["OPENAI_API_KEY"], "sk-work-test");
    assert!(fixture
        .storage()
        .load()
        .unwrap()
        .active_account_id
        .is_none());
}

#[test]
fn imports_current_login_and_replaces_credentials_without_changing_identity() {
    let fixture = Fixture::new();
    fixture.write_auth(&json!({"OPENAI_API_KEY": "sk-initial"}));
    fixture.ok(&["add", "current"]);
    let original = fixture.storage().load().unwrap().accounts.remove(0);
    assert_eq!(fixture.list()[0]["is_active"], true);
    let replacement = fixture.0.path().join("replacement.json");
    fs::write(
        &replacement,
        json!({"OPENAI_API_KEY": "sk-replacement"}).to_string(),
    )
    .unwrap();
    fixture.ok(&[
        "edit",
        "current",
        "--file",
        replacement.to_str().unwrap(),
        "--name",
        "new",
    ]);
    let updated = fixture.storage().load().unwrap().accounts.remove(0);
    assert_eq!(updated.id, original.id);
    assert_eq!(updated.created_at, original.created_at);
    assert_eq!(fixture.auth()["OPENAI_API_KEY"], "sk-replacement");
    assert_eq!(fixture.list()[0]["is_active"], true);
}

#[test]
fn rejects_duplicate_unknown_and_empty_edits_without_mutating_files() {
    let fixture = Fixture::new();
    fixture.key("one", "sk-one");
    fixture.key("two", "sk-two");
    let path = fixture.storage().directory.join("accounts.json");
    let before = fs::read(&path).unwrap();
    for args in [
        vec!["edit", "two", "--name", "one"],
        vec!["edit", "one", "--name", "  "],
        vec!["edit", "one"],
        vec!["remove", "unknown"],
        vec!["switch", "unknown"],
        vec!["add", "one"],
        vec!["add", "  "],
    ] {
        assert!(!fixture.run(&args).status.success(), "{args:?}");
        assert_eq!(fs::read(&path).unwrap(), before, "{args:?}");
    }
    assert!(!fixture.storage().auth_path().exists());
}

#[test]
fn switch_preserves_live_rotated_oauth_tokens_and_handles_external_login() {
    let fixture = Fixture::new();
    fixture.write_auth(&oauth_auth("workspace-a", "old"));
    fixture.ok(&["add", "oauth"]);
    fixture.key("api", "sk-alternate");
    fixture.write_auth(&oauth_auth("workspace-a", "rotated"));
    fixture.ok(&["switch", "api"]);
    fixture.ok(&["switch", "oauth"]);
    assert_eq!(fixture.auth()["tokens"]["refresh_token"], "refresh-rotated");
    // Reconcile an external login before preserving its live tokens.
    fixture.write_auth(&json!({"OPENAI_API_KEY": "sk-alternate"}));
    assert_eq!(fixture.list()[1]["is_active"], true);
    fixture.ok(&["switch", "oauth"]);
    assert_eq!(fixture.auth()["tokens"]["refresh_token"], "refresh-rotated");
}

#[test]
fn repeated_switch_restores_deleted_or_unrelated_auth() {
    let fixture = Fixture::new();
    fixture.key("one", "sk-one");
    fixture.ok(&["switch", "one"]);
    fs::remove_file(fixture.storage().auth_path()).unwrap();
    fixture.ok(&["switch", "one"]);
    assert_eq!(fixture.auth()["OPENAI_API_KEY"], "sk-one");
    fixture.write_auth(&json!({"OPENAI_API_KEY": "sk-external"}));
    assert_eq!(fixture.list()[0]["is_active"], false);
    fixture.ok(&["switch", "one"]);
    assert_eq!(fixture.auth()["OPENAI_API_KEY"], "sk-one");
}

#[test]
fn refuses_concurrent_mutation_and_recovers_when_lock_released() {
    let fixture = Fixture::new();
    fixture.key("one", "sk-one");
    let lock = fixture.storage().lock().unwrap();
    let output = fixture.run(&["remove", "one"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Another codex-switcher"));
    assert_eq!(fixture.list().as_array().unwrap().len(), 1);
    drop(lock);
    fixture.ok(&["remove", "one"]);
}

#[test]
fn failed_auth_write_does_not_claim_the_target_is_active() {
    let fixture = Fixture::new();
    fixture.key("one", "sk-one");
    // A file in place of the Codex directory makes creating auth.json fail.
    fs::write(fixture.storage().codex_home, "blocked").unwrap();
    assert!(!fixture.run(&["switch", "one"]).status.success());
    assert!(fixture
        .storage()
        .load()
        .unwrap()
        .active_account_id
        .is_none());
    fs::remove_file(fixture.storage().codex_home).unwrap();
    fixture.ok(&["switch", "one"]);
}

#[test]
fn supports_legacy_store_and_never_prints_credentials() {
    let fixture = Fixture::new();
    fixture.key("legacy", "sk-super-secret-test");
    let path = fixture.storage().directory.join("accounts.json");
    let mut store: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    store["masked_account_ids"] = json!([store["accounts"][0]["id"]]);
    fs::write(&path, store.to_string()).unwrap();
    let output = fixture.ok(&["list", "--json"]);
    assert!(output.contains("legacy"));
    assert!(!output.contains("sk-super-secret-test"));
    assert!(!output.contains("auth_data"));
    fixture.ok(&["switch", "legacy"]);
}

#[test]
fn malformed_credentials_and_conflicting_sources_fail_before_writes() {
    let fixture = Fixture::new();
    let path = fixture.0.path().join("bad.json");
    for invalid in [
        "{",
        "{}",
        r#"{"OPENAI_API_KEY":" "}"#,
        r#"{"tokens":{"id_token":"","access_token":"","refresh_token":""}}"#,
    ] {
        fs::write(&path, invalid).unwrap();
        assert!(!fixture
            .run(&["add", "bad", "--file", path.to_str().unwrap()])
            .status
            .success());
        assert!(!fixture.storage().directory.join("accounts.json").exists());
    }
    for args in [
        vec!["add", "--login", "--file", "anything"],
        vec!["add", "--file", "anything", "--api-key-stdin"],
        vec!["add", "--no-browser"],
        vec!["monitor"],
    ] {
        assert_eq!(fixture.run(&args).status.code(), Some(2));
    }
}

#[test]
fn empty_api_key_is_rejected() {
    let fixture = Fixture::new();
    let output = fixture
        .command(&["add", "empty", "--api-key-stdin"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!fixture.storage().directory.join("accounts.json").exists());
}

#[test]
fn explicit_blank_name_is_not_replaced_with_a_default() {
    let fixture = Fixture::new();
    fixture.write_auth(&json!({"OPENAI_API_KEY": "sk-current"}));
    assert!(!fixture.run(&["add", "  "]).status.success());
    assert!(!fixture.storage().directory.join("accounts.json").exists());
    fixture.ok(&["add"]);
    assert_eq!(fixture.list()[0]["name"], "API key account");
}

#[test]
fn invalid_or_future_store_is_not_overwritten() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.storage().directory).unwrap();
    let path = fixture.storage().directory.join("accounts.json");
    for content in [
        "not json",
        r#"{"version":99,"accounts":[],"active_account_id":null}"#,
    ] {
        fs::write(&path, content).unwrap();
        assert!(!fixture.run(&["remove", "one"]).status.success());
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
    }
}

#[cfg(unix)]
#[test]
fn credentials_are_private_after_creation_and_replacement() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.key("one", "sk-one");
    fixture.ok(&["switch", "one"]);
    for path in [
        fixture.storage().directory.join("accounts.json"),
        fixture.storage().auth_path(),
    ] {
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    }
    fixture.ok(&["switch", "one"]);
    for path in [
        fixture.storage().directory.join("accounts.json"),
        fixture.storage().auth_path(),
    ] {
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn status_handles_missing_managed_unmanaged_and_selected_accounts_read_only() {
    let fixture = Fixture::new();
    let snapshot: Value = serde_json::from_str(&fixture.ok(&["status", "--json"])).unwrap();
    assert_eq!(snapshot["login_status"], "not_logged_in");
    assert!(snapshot["account"].is_null());
    assert!(!fixture.storage().directory.exists());
    fixture.key("saved", "sk-local-status-secret");
    fixture.write_auth(&json!({"OPENAI_API_KEY": "sk-external-status-secret"}));
    let before_store = fs::read(fixture.storage().directory.join("accounts.json")).unwrap();
    let before_auth = fs::read(fixture.storage().auth_path()).unwrap();
    let output = fixture.ok(&["status", "--json"]);
    let snapshot: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(snapshot["login_status"], "unmanaged");
    assert!(snapshot["account"]["id"].is_null());
    assert_eq!(snapshot["account"]["credential_status"], "api_key_present");
    assert!(!output.contains("sk-external-status-secret"));
    let selected: Value =
        serde_json::from_str(&fixture.ok(&["status", "saved", "--json"])).unwrap();
    assert_eq!(selected["account"]["name"], "saved");
    assert_eq!(selected["account"]["is_active"], false);
    assert!(!fixture.run(&["status", "unknown"]).status.success());
    for args in [
        vec!["status"],
        vec!["status", "saved"],
        vec!["list"],
        vec!["list", "--json"],
    ] {
        let output = fixture.ok(&args);
        assert!(!output.contains("sk-local-status-secret"));
        assert!(!output.contains("sk-external-status-secret"));
    }
    assert_eq!(
        fs::read(fixture.storage().directory.join("accounts.json")).unwrap(),
        before_store
    );
    assert_eq!(
        fs::read(fixture.storage().auth_path()).unwrap(),
        before_auth
    );
    fixture.ok(&["switch", "saved"]);
    let snapshot: Value = serde_json::from_str(&fixture.ok(&["status", "--json"])).unwrap();
    assert_eq!(snapshot["login_status"], "managed");
    assert_eq!(snapshot["account"]["name"], "saved");
    assert_eq!(snapshot["account"]["is_active"], true);
    assert!(snapshot["account"]["last_used_at"].is_string());
}

#[test]
fn status_uses_live_oauth_expiry_without_refreshing_or_exposing_tokens() {
    let fixture = Fixture::new();
    fixture.write_auth(&oauth_auth("status-account", "original"));
    fixture.ok(&["add", "oauth"]);
    let mut live = oauth_auth("status-account", "rotated");
    let payload = json!({"exp": 1, "email": "status-account@example.com",
        "https://api.openai.com/auth": {"chatgpt_account_id": "status-account", "chatgpt_plan_type": "plus"}});
    live["tokens"]["id_token"] = json!(format!(
        "header.{}.rotated",
        URL_SAFE_NO_PAD.encode(payload.to_string())
    ));
    fixture.write_auth(&live);
    let before = fs::read(fixture.storage().directory.join("accounts.json")).unwrap();
    let output = fixture.ok(&["status", "--json"]);
    let snapshot: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(snapshot["account"]["credential_status"], "expired");
    assert_eq!(snapshot["account"]["plan_type"], "plus");
    assert_eq!(snapshot["account"]["refresh_token_present"], true);
    assert_eq!(
        snapshot["account"]["id_token_expires_at"],
        "1970-01-01T00:00:01Z"
    );
    assert_eq!(fixture.list()[0]["credential_status"], "expired");
    assert!(fixture.ok(&["list"]).contains("expired"));
    assert!(!output.contains("refresh-rotated"));
    assert!(!output.contains("access-rotated"));
    assert!(!output.contains(live["tokens"]["id_token"].as_str().unwrap()));
    assert_eq!(fixture.auth(), live);
    assert_eq!(
        fs::read(fixture.storage().directory.join("accounts.json")).unwrap(),
        before
    );
}
