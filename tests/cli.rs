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
        // Never touch real app-server processes from tests.
        command
            .env("HOME", self.0.path())
            .arg("--no-restart")
            .arg("--store-dir")
            .arg(self.storage().directory)
            .arg("--codex-home")
            .arg(self.storage().codex_home)
            .args(args);
        command
    }

    /// Run without --codex-home, for `-w`, which conflicts with it.
    fn in_workspace(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_codex-switcher"))
            .env("HOME", self.0.path())
            .env_remove("CODEX_HOME")
            .arg("--no-restart")
            .arg("--store-dir")
            .arg(self.storage().directory)
            .args(args)
            .output()
            .unwrap()
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

fn warmup_server(request_count: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
    use tiny_http::{Response, Server};
    let server = Server::http("127.0.0.1:0").unwrap();
    let root = format!("http://{}", server.server_addr());
    let handle = std::thread::spawn(move || {
        let mut posted = Vec::new();
        for _ in 0..request_count {
            let mut request = server
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .expect("missing request");
            let header = |name: &'static str| {
                request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv(name))
                    .unwrap()
                    .value
                    .as_str()
                    .to_owned()
            };
            let id = header("chatgpt-account-id");
            assert_eq!(header("authorization"), format!("Bearer access-{id}"));
            if request.method().as_str() == "GET" {
                assert_eq!(request.url(), "/api/codex/usage");
                let window = json!({"used_percent": if id == "used" {0.1} else {0.0}, "limit_window_seconds":18000});
                let limits = match id.as_str() {
                    "missing" => {
                        json!({"primary_window":{"used_percent":0,"limit_window_seconds":604800}})
                    }
                    "secondary" => json!({"secondary_window":window}),
                    _ => json!({"primary_window":window}),
                };
                request
                    .respond(
                        Response::from_string(json!({"rate_limit":limits}).to_string())
                            .with_status_code(if id == "usage-error" { 403 } else { 200 }),
                    )
                    .unwrap();
            } else {
                assert_eq!(request.method().as_str(), "POST");
                assert_eq!(request.url(), "/api/codex/responses");
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let body: Value = serde_json::from_str(&body).unwrap();
                assert_eq!(body["model"], "gpt-6-luna");
                assert_eq!(body["input"][0]["content"][0]["text"], "hi");
                assert_eq!(body["stream"], true);
                assert_eq!(body["store"], false);
                let event = if id == "post-error" {
                    json!({"type":"response.failed","error":{"message":"access-secret"}})
                } else {
                    json!({"type":"response.completed","response":{"status":"completed"}})
                };
                request
                    .respond(Response::from_string(format!("data: {event}\n\n")))
                    .unwrap();
                posted.push(id);
            }
        }
        assert!(server
            .recv_timeout(std::time::Duration::from_millis(100))
            .unwrap()
            .is_none());
        posted
    });
    (root, handle)
}

#[test]
fn warmup_all_checks_exact_5h_zero_and_continues_after_errors() {
    let fixture = Fixture::new();
    for name in [
        "zero",
        "used",
        "missing",
        "usage-error",
        "post-error",
        "secondary",
    ] {
        fixture.write_auth(&oauth_auth(name, name));
        fixture.ok(&["add", name]);
    }
    fixture.key("api", "sk-test");
    let before = fs::read(fixture.storage().auth_path()).unwrap();
    let store_before = fs::read(fixture.storage().directory.join("accounts.json")).unwrap();
    let (root, server) = warmup_server(9);
    let output = fixture.run(&["warmup", "--base-url", &root, "--json"]);
    assert!(!output.status.success());
    let rows: Value = serde_json::from_slice(&output.stdout).unwrap();
    let statuses: Vec<_> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        statuses,
        ["warmed", "skipped", "skipped", "error", "error", "warmed", "skipped"]
    );
    assert_eq!(server.join().unwrap(), ["zero", "post-error", "secondary"]);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("access-"));
    assert_eq!(fs::read(fixture.storage().auth_path()).unwrap(), before);
    assert_eq!(
        fs::read(fixture.storage().directory.join("accounts.json")).unwrap(),
        store_before
    );
    assert!(!fixture.storage().codex_home.join("config.toml").exists());
}

#[test]
fn warmup_selects_exact_name_or_id_and_handles_empty_and_unknown() {
    let fixture = Fixture::new();
    assert_eq!(fixture.ok(&["warmup", "--json"]).trim(), "[]");
    assert!(!fixture.run(&["warmup", "unknown"]).status.success());
    fixture.write_auth(&oauth_auth("zero", "zero"));
    fixture.ok(&["add", "So Zhang@example.com"]);
    fixture.key("other", "sk-other");
    let id = fixture.list()[0]["id"].as_str().unwrap().to_owned();
    for selector in ["So Zhang@example.com", &id] {
        let (root, server) = warmup_server(2);
        let output = fixture.ok(&["warmup", selector, "--base-url", &root, "--json"]);
        let rows: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["status"], "warmed");
        assert_eq!(server.join().unwrap(), ["zero"]);
    }
}

#[test]
fn switch_without_account_cycles_in_saved_order() {
    let fixture = Fixture::new();
    fixture.key("one", "sk-one-test");
    fixture.key("two", "sk-two-test");
    fixture.key("three", "sk-three-test");
    for name in ["one", "two", "three", "one"] {
        assert!(fixture
            .ok(&["switch"])
            .contains(&format!("Switched to {name}")));
        assert_eq!(fixture.auth()["OPENAI_API_KEY"], format!("sk-{name}-test"));
    }
    // Follow the actual live login, even if it changed outside this tool.
    fixture.write_auth(&json!({"OPENAI_API_KEY": "sk-two-test"}));
    fixture.ok(&["switch"]);
    assert_eq!(fixture.auth()["OPENAI_API_KEY"], "sk-three-test");
    fixture.write_auth(&json!({"OPENAI_API_KEY": "sk-external-test"}));
    fixture.ok(&["switch"]);
    assert_eq!(fixture.auth()["OPENAI_API_KEY"], "sk-one-test");
}

#[test]
fn switch_without_account_handles_empty_and_single_account() {
    let fixture = Fixture::new();
    let output = fixture.run(&["switch"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("No saved accounts"));
    assert!(!fixture.storage().auth_path().exists());
    fixture.key("one", "sk-one-test");
    for _ in 0..2 {
        fixture.ok(&["switch"]);
        assert_eq!(fixture.auth()["OPENAI_API_KEY"], "sk-one-test");
    }
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
fn ls_is_an_alias_for_list() {
    let fixture = Fixture::new();
    assert_eq!(fixture.ok(&["ls", "--json"]).trim(), "[]");
}

#[test]
fn daemon_command_is_removed() {
    let fixture = Fixture::new();
    let output = fixture.run(&["daemon", "run"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand 'daemon'"));
}

#[test]
fn editing_active_credentials_does_not_restart_app_servers() {
    let fixture = Fixture::new();
    fixture.write_auth(&json!({"OPENAI_API_KEY": "sk-initial"}));
    fixture.ok(&["add", "current"]);
    let replacement = fixture.0.path().join("replacement.json");
    fs::write(
        &replacement,
        json!({"OPENAI_API_KEY": "sk-replacement"}).to_string(),
    )
    .unwrap();
    // No --no-restart: edit must not attempt process control by default.
    // An empty PATH also prevents touching real processes if this regresses.
    let output = Command::new(env!("CARGO_BIN_EXE_codex-switcher"))
        .env("PATH", fixture.0.path())
        .arg("--store-dir")
        .arg(fixture.storage().directory)
        .arg("--codex-home")
        .arg(fixture.storage().codex_home)
        .arg("--codex-bin")
        .arg(fixture.0.path().join("missing-codex"))
        .args(["edit", "current", "--file"])
        .arg(replacement)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.auth()["OPENAI_API_KEY"], "sk-replacement");
    assert!(!fixture.storage().codex_home.join("config.toml").exists());
}

#[test]
fn switch_joins_unquoted_name_words() {
    let fixture = Fixture::new();
    fixture.key("So Zhang", "sk-space-name");
    fixture.ok(&["switch", "So", "Zhang"]);
    assert_eq!(fixture.auth()["OPENAI_API_KEY"], "sk-space-name");
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
fn relogin_rejects_unknown_accounts_before_starting_oauth() {
    let fixture = Fixture::new();
    let output = fixture.run(&["relogin", "missing", "--no-browser"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not found"));
    assert!(!stderr.contains("Open this URL"));
    assert_eq!(fixture.run(&["relogin"]).status.code(), Some(2));
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
fn status_defaults_to_all_accounts_and_accepts_an_id() {
    let fixture = Fixture::new();
    assert_eq!(
        serde_json::from_str::<Value>(&fixture.ok(&["status", "--json"])).unwrap(),
        json!([])
    );
    assert!(!fixture.storage().directory.exists());
    fixture.key("one", "sk-one-secret");
    fixture.key("two", "sk-two-secret");
    let rows: Value = serde_json::from_str(&fixture.ok(&["status", "--json"])).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert_eq!(rows[0]["status"], "unsupported");
    assert!(rows[0]["windows"].as_array().unwrap().is_empty());
    let selected: Value =
        serde_json::from_str(&fixture.ok(&["status", rows[1]["id"].as_str().unwrap(), "--json"]))
            .unwrap();
    assert_eq!(selected.as_array().unwrap().len(), 1);
    assert_eq!(selected[0]["name"], "two");
    assert!(!fixture.run(&["status", "unknown"]).status.success());
    let output = fixture.ok(&["status"]);
    assert!(output.contains("unavailable for API key"));
    assert!(!output.contains("sk-one-secret"));
    assert!(!output.contains("sk-two-secret"));
}

#[test]
fn status_uses_official_codex_api_route_and_auth_headers_for_custom_backend() {
    let fixture = Fixture::new();
    fixture.write_auth(&oauth_auth("workspace-official", "official"));
    fixture.ok(&["add", "official"]);
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", server.server_addr());
    let worker = std::thread::spawn(move || {
        let request = server
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(request.url(), "/api/codex/usage");
        let header = |name: &'static str| {
            request
                .headers()
                .iter()
                .find(|header| header.field.equiv(name))
                .map(|header| header.value.as_str())
        };
        assert_eq!(header("authorization"), Some("Bearer access-official"));
        assert_eq!(header("chatgpt-account-id"), Some("workspace-official"));
        assert_eq!(header("user-agent"), Some("codex-cli"));
        assert!(header("origin").is_none());
        assert!(header("referer").is_none());
        request.respond(tiny_http::Response::from_string(r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":12,"limit_window_seconds":18000,"reset_after_seconds":600}}}"#)).unwrap();
    });
    let output = fixture
        .command(&["status", "official", "--base-url", &base_url, "--json"])
        .env("NO_PROXY", "127.0.0.1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    worker.join().unwrap();
    let rows: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rows[0]["windows"][0]["used_percent"], 12.0);
    assert_eq!(rows[0]["windows"][0]["resets_in_seconds"], 600);
}

#[cfg(target_os = "linux")]
mod workspaces {
    use super::*;

    /// Two saved OAuth accounts, with `a` active in the main home.
    fn workspace_fixture() -> Fixture {
        let fixture = Fixture::new();
        let files = fixture.0.path().join("auth");
        fs::create_dir_all(&files).unwrap();
        for name in ["a", "b"] {
            let file = files.join(name);
            fs::write(&file, oauth_auth(name, name).to_string()).unwrap();
            fixture.ok(&["add", name, "--file", file.to_str().unwrap()]);
        }
        fixture.ok(&["switch", "a"]);
        fixture
    }

    fn workspace_home(fixture: &Fixture, name: &str) -> std::path::PathBuf {
        fixture.ok(&["workspace", "path", name]).trim().into()
    }

    #[test]
    fn workspace_keeps_only_what_codex_needs_and_shares_customizations() {
        let fixture = workspace_fixture();
        let codex = fixture.storage().codex_home;
        fs::write(codex.join("config.toml"), "model = \"custom\"\n").unwrap();
        for directory in ["skills", "sessions", "packages"] {
            fs::create_dir_all(codex.join(directory)).unwrap();
        }
        for file in ["auth.json.other", "history.jsonl", "state_5.sqlite"] {
            fs::write(codex.join(file), "private").unwrap();
        }

        let output = fixture.ok(&["workspace", "create", "work", "--account", "b"]);
        assert!(output.contains("Created workspace work using b"));
        let home = workspace_home(&fixture, "work");

        let auth: Value =
            serde_json::from_slice(&fs::read(home.join("auth.json")).unwrap()).unwrap();
        assert_eq!(auth["tokens"]["refresh_token"], "refresh-b");
        assert_eq!(fixture.auth()["tokens"]["refresh_token"], "refresh-a");

        let config = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(config.contains("model = \"custom\""));
        assert!(config.contains("daemon_auto_start = true"));
        assert!(fs::symlink_metadata(home.join("skills"))
            .unwrap()
            .file_type()
            .is_symlink());
        for private in [
            "sessions",
            "packages",
            "auth.json.other",
            "history.jsonl",
            "state_5.sqlite",
        ] {
            assert!(!home.join(private).exists(), "{private} was copied");
        }

        let workspaces: Value =
            serde_json::from_str(&fixture.ok(&["workspace", "list", "--json"])).unwrap();
        assert_eq!(workspaces[0]["name"], "default");
        assert_eq!(workspaces[1]["name"], "work");
        assert_eq!(workspaces[1]["account"], "b");
    }

    #[test]
    fn an_oauth_account_can_only_be_used_by_one_home() {
        let fixture = workspace_fixture();
        let refused = fixture.run(&["workspace", "create", "work", "--account", "a"]);
        assert!(!refused.status.success());
        assert!(String::from_utf8_lossy(&refused.stderr).contains("already in use"));
        assert!(fixture.run(&["workspace", "path", "work"]).status.code() == Some(1));
        assert!(!fixture.storage().directory.join("workspaces/work").exists());

        fixture.ok(&["workspace", "create", "work", "--account", "b"]);
        let refused = fixture.run(&["switch", "b"]);
        assert!(!refused.status.success());
        assert!(String::from_utf8_lossy(&refused.stderr).contains("already in use"));
        assert_eq!(fixture.auth()["tokens"]["refresh_token"], "refresh-a");

        // Rotation skips the account held by the workspace and stays on the only free one.
        assert!(fixture.ok(&["switch"]).contains("Switched to a"));
    }

    #[test]
    fn api_key_accounts_can_be_shared_between_homes() {
        let fixture = Fixture::new();
        fixture.key("key", "sk-shared");
        fixture.ok(&["switch", "key"]);
        fixture.ok(&["workspace", "create", "work", "--account", "key"]);
        let home = workspace_home(&fixture, "work");
        assert!(fs::read_to_string(home.join("auth.json"))
            .unwrap()
            .contains("sk-shared"));
    }

    #[test]
    fn tokens_rotated_in_a_workspace_survive_its_removal_and_reach_a_new_login() {
        let fixture = workspace_fixture();
        fixture.ok(&["workspace", "create", "work", "--account", "b"]);
        let home = workspace_home(&fixture, "work");
        fs::write(
            home.join("auth.json"),
            oauth_auth("b", "rotated").to_string(),
        )
        .unwrap();

        // Without --yes and without a terminal, nothing is deleted.
        let refused = fixture
            .command(&["workspace", "remove", "work"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!refused.status.success());
        assert!(home.exists());

        fixture.ok(&["workspace", "remove", "work", "--yes"]);
        assert!(!home.exists());
        assert!(fixture.storage().codex_home.exists());

        fixture.ok(&["switch", "b"]);
        assert_eq!(fixture.auth()["tokens"]["refresh_token"], "refresh-rotated");
    }

    #[test]
    fn workspace_run_sets_codex_home_and_keeps_the_exit_code() {
        let fixture = workspace_fixture();
        fixture.ok(&["workspace", "create", "work", "--account", "b"]);
        let home = workspace_home(&fixture, "work");

        let output = fixture.ok(&["workspace", "run", "work", "sh", "-c", "echo $CODEX_HOME"]);
        assert_eq!(output.trim(), home.to_str().unwrap());

        let failed = fixture.run(&["workspace", "run", "work", "--", "sh", "-c", "exit 7"]);
        assert_eq!(failed.status.code(), Some(7));

        let missing = fixture.run(&["workspace", "run", "nope", "true"]);
        assert!(String::from_utf8_lossy(&missing.stderr).contains("not found"));
    }

    #[test]
    fn workspace_run_defaults_to_codex() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = workspace_fixture();
        fixture.ok(&["workspace", "create", "work", "--account", "b"]);
        let script = fixture.0.path().join("fake-codex");
        fs::write(&script, "#!/bin/sh\necho \"fake $CODEX_HOME\"\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let output = fixture.ok(&[
            "--codex-bin",
            script.to_str().unwrap(),
            "workspace",
            "run",
            "work",
        ]);
        let home = workspace_home(&fixture, "work");
        assert_eq!(output.trim(), format!("fake {}", home.display()));
    }

    #[test]
    fn workspace_shell_starts_the_users_shell_in_the_workspace() {
        let fixture = workspace_fixture();
        fixture.ok(&["workspace", "create", "work", "--account", "b"]);
        let mut child = fixture
            .command(&["workspace", "shell", "work"])
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"echo $CODEX_HOME\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            workspace_home(&fixture, "work").to_str().unwrap()
        );
    }

    #[test]
    fn switch_targets_only_the_chosen_workspace() {
        let fixture = workspace_fixture();
        let created = fixture.ok(&["workspace", "create", "work"]);
        assert!(created.contains("switch ACCOUNT -w work"));
        let home = workspace_home(&fixture, "work");
        assert!(!home.join("auth.json").exists());

        let output = fixture.in_workspace(&["switch", "b", "-w", "work"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let auth: Value =
            serde_json::from_slice(&fs::read(home.join("auth.json")).unwrap()).unwrap();
        assert_eq!(auth["tokens"]["refresh_token"], "refresh-b");
        assert_eq!(fixture.auth()["tokens"]["refresh_token"], "refresh-a");

        let refused = fixture.in_workspace(&["-w", "work", "switch", "a"]);
        assert!(!refused.status.success());
        let message = String::from_utf8_lossy(&refused.stderr).into_owned();
        assert!(message.contains("already in use"), "{message}");

        let listed = fixture.in_workspace(&["-w", "work", "workspace", "list", "--json"]);
        let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
        assert_eq!(listed[0]["current"], false);
        assert_eq!(listed[1]["current"], true);
        assert_eq!(listed[1]["account"], "b");
    }

    #[test]
    fn account_lists_name_the_workspaces_using_each_account() {
        let fixture = workspace_fixture();
        fixture.ok(&["workspace", "create", "work", "--account", "b"]);
        let file = fixture.0.path().join("auth/c");
        fs::write(&file, oauth_auth("c", "c").to_string()).unwrap();
        fixture.ok(&["add", "c", "--file", file.to_str().unwrap()]);

        let text = fixture.ok(&["--color", "never", "list"]);
        let line = |name: &str| {
            text.lines()
                .find(|line| line.contains(&format!("● {name}")))
                .unwrap()
                .to_owned()
        };
        assert!(line("a").ends_with("[current]"), "{text}");
        assert!(line("b").ends_with("[work]"), "{text}");
        assert!(!line("c").contains('['), "{text}");

        let rows = fixture.list();
        assert_eq!(rows[1]["workspaces"], json!(["work"]));
        assert_eq!(rows[2]["workspaces"], json!([]));

        let in_work = fixture.in_workspace(&["-w", "work", "--color", "never", "list"]);
        let in_work = String::from_utf8_lossy(&in_work.stdout);
        assert!(in_work.contains("[work]"), "{in_work}");
    }

    #[test]
    fn disabling_frees_the_account_until_enable_or_switch() {
        let fixture = workspace_fixture();
        fixture.ok(&["workspace", "create", "work", "--account", "b"]);
        let home = workspace_home(&fixture, "work");
        // Tokens rotated while the workspace ran must survive being disabled.
        fs::write(
            home.join("auth.json"),
            oauth_auth("b", "rotated").to_string(),
        )
        .unwrap();

        let output = fixture.ok(&["workspace", "disable", "work"]);
        assert!(output.contains("b is free"), "{output}");
        assert!(!home.join("auth.json").exists());
        assert!(!fixture
            .run(&["workspace", "disable", "work"])
            .status
            .success());

        let listed: Value =
            serde_json::from_str(&fixture.ok(&["workspace", "list", "--json"])).unwrap();
        assert_eq!(listed[1]["disabled"], true);
        assert_eq!(listed[1]["disabled_account"], "b");

        let refused = fixture.run(&["workspace", "run", "work"]);
        assert!(String::from_utf8_lossy(&refused.stderr).contains("is disabled"));

        // Another home takes the account, so enable has to wait for it.
        fixture.ok(&["switch", "b"]);
        assert_eq!(fixture.auth()["tokens"]["refresh_token"], "refresh-rotated");
        let refused = fixture.run(&["workspace", "enable", "work"]);
        assert!(String::from_utf8_lossy(&refused.stderr).contains("already in use"));
        assert!(!home.join("auth.json").exists());

        fixture.ok(&["switch", "a"]);
        let output = fixture.ok(&["workspace", "enable", "work"]);
        assert!(
            output.contains("Enabled workspace work using b"),
            "{output}"
        );
        let auth: Value =
            serde_json::from_slice(&fs::read(home.join("auth.json")).unwrap()).unwrap();
        assert_eq!(auth["tokens"]["refresh_token"], "refresh-rotated");
        assert!(!fixture
            .run(&["workspace", "enable", "work"])
            .status
            .success());
    }

    #[test]
    fn switching_a_disabled_workspace_enables_it_with_the_chosen_account() {
        let fixture = workspace_fixture();
        let file = fixture.0.path().join("auth/c");
        fs::write(&file, oauth_auth("c", "c").to_string()).unwrap();
        fixture.ok(&["add", "c", "--file", file.to_str().unwrap()]);
        fixture.ok(&["workspace", "create", "work", "--account", "b"]);
        fixture.ok(&["workspace", "disable", "work"]);

        let output = fixture.in_workspace(&["switch", "c", "-w", "work"]);
        assert!(output.status.success());
        let listed: Value =
            serde_json::from_str(&fixture.ok(&["workspace", "list", "--json"])).unwrap();
        assert_eq!(listed[1]["disabled"], false);
        assert_eq!(listed[1]["account"], "c");
        assert!(!fixture
            .run(&["workspace", "enable", "work"])
            .status
            .success());
    }

    #[test]
    fn disable_keeps_a_login_that_is_not_saved() {
        let fixture = workspace_fixture();
        fixture.ok(&["workspace", "create", "work"]);
        let refused = fixture.run(&["workspace", "disable", "work"]);
        assert!(String::from_utf8_lossy(&refused.stderr).contains("not logged in"));

        let home = workspace_home(&fixture, "work");
        fs::write(
            home.join("auth.json"),
            oauth_auth("unsaved", "x").to_string(),
        )
        .unwrap();
        let refused = fixture.run(&["workspace", "disable", "work"]);
        assert!(String::from_utf8_lossy(&refused.stderr).contains("not a saved account"));
        assert!(home.join("auth.json").exists());
    }

    #[test]
    fn dot_codex_is_the_default_workspace() {
        let fixture = workspace_fixture();
        let path = fixture.ok(&["workspace", "path", "default"]);
        let dot_codex = fixture.0.path().join(".codex");
        assert_eq!(path.trim(), dot_codex.to_str().unwrap());

        fixture.ok(&["workspace", "create", "work", "--account", "b"]);
        // Selecting default still works from a shell inside another workspace.
        let output = Command::new(env!("CARGO_BIN_EXE_codex-switcher"))
            .env("HOME", fixture.0.path())
            .env("CODEX_HOME", workspace_home(&fixture, "work"))
            .arg("--no-restart")
            .arg("--store-dir")
            .arg(fixture.storage().directory)
            .args(["-w", "default", "switch", "a"])
            .output()
            .unwrap();
        // `a` is active in the fixture's own --codex-home, which is a known source.
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("already in use"));

        let file = fixture.0.path().join("auth/c");
        fs::write(&file, oauth_auth("c", "c").to_string()).unwrap();
        fixture.ok(&["add", "c", "--file", file.to_str().unwrap()]);
        let output = fixture.in_workspace(&["-w", "default", "switch", "c"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(dot_codex.join("auth.json").exists());

        for args in [
            ["workspace", "create", "default"].as_slice(),
            ["workspace", "remove", "default", "--yes"].as_slice(),
        ] {
            assert!(!fixture.run(args).status.success(), "{args:?}");
        }
        assert!(dot_codex.exists());
    }

    #[test]
    fn workspace_flag_rejects_unknown_names_and_explicit_homes() {
        let fixture = workspace_fixture();
        let missing = fixture.in_workspace(&["-w", "nope", "list"]);
        assert!(String::from_utf8_lossy(&missing.stderr).contains("not found"));
        assert!(!fixture.run(&["-w", "default", "list"]).status.success());
    }

    #[test]
    fn workspace_names_cannot_escape_their_directory() {
        let fixture = workspace_fixture();
        for name in ["../escape", ".hidden", "a/b", ""] {
            let output = fixture.run(&["workspace", "create", name, "--account", "b"]);
            assert!(!output.status.success(), "{name}");
        }
        assert!(!fixture.storage().directory.join("escape").exists());
        let output = fixture.run(&["workspace", "remove", "../../codex", "--yes"]);
        assert!(!output.status.success());
        assert!(fixture.storage().codex_home.exists());
    }
}

#[cfg(unix)]
#[test]
fn empty_codex_home_falls_back_to_the_default_directory() {
    let fixture = Fixture::new();
    fixture.key("key", "sk-default");
    let output = Command::new(env!("CARGO_BIN_EXE_codex-switcher"))
        .env("HOME", fixture.0.path())
        .env("CODEX_HOME", "")
        .current_dir(fixture.0.path())
        .args(["--no-restart", "--store-dir"])
        .arg(fixture.storage().directory)
        .args(["switch", "key"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.0.path().join(".codex/auth.json").exists());
    assert!(!fixture.0.path().join("auth.json").exists());
}
