//! Foreground watcher: poll the active account's usage and, when it runs out,
//! switch to the saved account with the most quota and restart the app-server.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    accounts, config, processes,
    storage::Storage,
    usage::{self, AccountUsage},
};

#[derive(Clone, Debug)]
pub struct Settings {
    /// Switch when any window's remaining percent drops below this value.
    pub threshold: f64,
    pub min_interval: Duration,
    pub max_interval: Duration,
    /// Minimum time between automatic switches.
    pub cooldown: Duration,
    pub base_url: String,
    pub codex_bin: String,
    /// Restart and clean up app-server processes on start and after a switch.
    pub manage_app_server: bool,
}

/// Last observation, written after every check for `daemon status`.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct State {
    pub pid: u32,
    pub checked_at: Option<DateTime<Utc>>,
    pub active: Option<String>,
    pub remaining_5h: Option<f64>,
    pub next_check_at: Option<DateTime<Utc>>,
    pub last_switch: Option<Switch>,
    pub last_error: Option<String>,
    /// Set when every account is exhausted: no checks or switches until this
    /// time, unless the active account is changed manually first.
    #[serde(default)]
    pub paused_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub paused_active_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Switch {
    pub at: DateTime<Utc>,
    pub from: String,
    pub to: String,
}

pub fn state_path(storage: &Storage) -> PathBuf {
    storage.directory.join("daemon-state.json")
}

pub fn read_state(storage: &Storage) -> Result<Option<State>> {
    let path = state_path(storage);
    match std::fs::read(&path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .with_context(|| format!("Failed to parse {}", path.display()))
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
}

fn write_state(storage: &Storage, state: &State) -> Result<()> {
    std::fs::create_dir_all(&storage.directory)?;
    let temporary = tempfile::NamedTempFile::new_in(&storage.directory)?;
    serde_json::to_writer_pretty(temporary.as_file(), state)?;
    temporary.persist(state_path(storage))?;
    Ok(())
}

fn window(row: &AccountUsage, label: &str) -> Option<f64> {
    row.windows
        .iter()
        .find(|window| window.label == label)
        .map(|window| window.remaining_percent)
}

/// Lowest remaining percent across all windows; `None` when usage is unknown.
fn min_remaining(row: &AccountUsage) -> Option<f64> {
    if row.status != "ok" {
        return None;
    }
    row.windows
        .iter()
        .map(|window| window.remaining_percent)
        .reduce(f64::min)
}

/// Poll faster as the 5h window drains; tiers scale with `max_interval`.
pub fn next_interval(remaining_5h: Option<f64>, settings: &Settings) -> Duration {
    let max = settings.max_interval;
    let interval = match remaining_5h {
        None => max,
        Some(r) if r > 50.0 => max,
        Some(r) if r > 20.0 => max / 2,
        Some(r) if r > 5.0 => max / 5,
        Some(_) => settings.min_interval,
    };
    interval.clamp(settings.min_interval, max)
}

/// The usable account with the most 5h quota left, excluding `active`.
/// Every known window must be above the threshold, so a spent weekly limit
/// disqualifies an account whose 5h window has just reset.
pub fn pick_next<'a>(
    rows: &'a [AccountUsage],
    active: Option<&str>,
    threshold: f64,
) -> Option<&'a AccountUsage> {
    rows.iter()
        .filter(|row| Some(row.id.as_str()) != active)
        .filter(|row| min_remaining(row).is_some_and(|value| value >= threshold))
        .max_by(|a, b| {
            let key = |row: &AccountUsage| {
                (
                    window(row, "5h").unwrap_or(100.0),
                    min_remaining(row).unwrap_or(0.0),
                )
            };
            key(a).partial_cmp(&key(b)).unwrap()
        })
}

/// Seconds until the first account (including the active one) has every
/// blocking window reset.
fn earliest_reset(rows: &[AccountUsage], threshold: f64) -> Option<i64> {
    rows.iter()
        .filter(|row| row.status == "ok")
        .filter_map(|row| {
            row.windows
                .iter()
                .filter(|window| window.remaining_percent < threshold)
                .filter_map(|window| window.resets_in_seconds)
                .max()
        })
        .min()
}

fn log(message: impl std::fmt::Display) {
    eprintln!("[{}] {message}", Utc::now().format("%Y-%m-%d %H:%M:%S"));
}

/// Resolve the Codex executable: a path is used as given, a bare name is
/// looked up on PATH, then in the managed daemon's install (service managers
/// often run with a PATH that lacks npm or user bin directories).
pub fn resolve_codex_bin(codex_bin: &str, codex_home: &Path) -> Option<PathBuf> {
    if codex_bin.contains('/') {
        return Some(PathBuf::from(codex_bin));
    }
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(codex_bin))
        .chain(
            (codex_bin == "codex")
                .then(|| codex_home.join("packages/app-server-daemon/current/bin/codex")),
        )
        .find(|path| path.is_file())
}

fn codex_daemon(settings: &Settings, codex_home: &Path, action: &str) -> Result<()> {
    let codex = resolve_codex_bin(&settings.codex_bin, codex_home).with_context(|| {
        format!(
            "Could not find `{}` on PATH; pass --codex-bin with an absolute path",
            settings.codex_bin
        )
    })?;
    let output = Command::new(&codex)
        .args(["app-server", "daemon", action])
        .output()
        .with_context(|| format!("Could not run {}", codex.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "`{} app-server daemon {action}` failed: {}",
        codex.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Stop app-servers that clients spawned themselves (e.g. the desktop app) so
/// they reconnect through the managed daemon, then start or restart it. The
/// managed daemon itself is only ever restarted through Codex: killing it
/// directly leaves clients unable to reconnect.
fn refresh_app_servers(settings: &Settings, codex_home: &Path, action: &str) -> Result<()> {
    let stopped = processes::stop_standalone_app_servers()?;
    if !stopped.is_empty() {
        log(format!(
            "Stopped {} standalone app-server process(es)",
            stopped.len()
        ));
    }
    codex_daemon(settings, codex_home, action)
}

/// One check. Returns how long to sleep before the next one.
async fn tick(storage: &Storage, settings: &Settings, state: &mut State) -> Result<Duration> {
    let store = accounts::load_current(storage)?;
    let Some(active_id) = store.active_account_id.clone() else {
        state.active = None;
        state.remaining_5h = None;
        log("No active account; waiting");
        return Ok(settings.max_interval);
    };
    let rows = usage::query(storage, Some(&active_id), &settings.base_url).await?;
    let row = rows.first().context("Active account disappeared")?;
    state.active = Some(row.name.clone());
    state.remaining_5h = window(row, "5h");
    match row.status {
        "ok" => {}
        "unsupported" => {
            log(format!("{} is an API key account; not monitored", row.name));
            return Ok(settings.max_interval);
        }
        _ => anyhow::bail!(
            "{}: {}",
            row.name,
            row.error.as_deref().unwrap_or("usage query failed")
        ),
    }
    let remaining = min_remaining(row);
    if remaining.is_none_or(|value| value >= settings.threshold) {
        return Ok(next_interval(state.remaining_5h, settings));
    }

    if let Some(last) = &state.last_switch {
        let elapsed = Utc::now()
            .signed_duration_since(last.at)
            .to_std()
            .unwrap_or_default();
        if elapsed < settings.cooldown {
            log(format!(
                "{} is exhausted but still in switch cooldown",
                row.name
            ));
            return Ok((settings.cooldown - elapsed).max(settings.min_interval));
        }
    }

    log(format!(
        "{} has {:.1}% left; looking for another account",
        row.name,
        remaining.unwrap_or(0.0)
    ));
    let all = usage::query(storage, None, &settings.base_url).await?;
    let Some(next) = pick_next(&all, Some(&active_id), settings.threshold) else {
        let wait = earliest_reset(&all, settings.threshold)
            .map(|seconds| Duration::from_secs(seconds.max(0) as u64 + 5))
            .unwrap_or(settings.max_interval)
            .max(settings.min_interval);
        let until = Utc::now() + chrono::Duration::from_std(wait)?;
        state.paused_until = Some(until);
        state.paused_active_id = Some(active_id);
        log(format!(
            "No account has quota left; paused until {} (switch manually or restart the daemon to check sooner)",
            until.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S")
        ));
        return Ok(wait);
    };
    let from = row.name.clone();
    let to = accounts::switch(storage, &next.id).await?;
    log(format!(
        "Switched {from} -> {to} ({:.1}% of 5h left)",
        window(next, "5h").unwrap_or(100.0)
    ));
    state.last_switch = Some(Switch {
        at: Utc::now(),
        from,
        to: to.clone(),
    });
    state.active = Some(to);
    state.remaining_5h = window(next, "5h");
    if settings.manage_app_server {
        refresh_app_servers(settings, &storage.codex_home, "restart")?;
        log("Restarted app-server");
    }
    Ok(next_interval(state.remaining_5h, settings))
}

pub async fn run(storage: &Storage, settings: Settings) -> Result<()> {
    if config::ensure_daemon_auto_start(&storage.codex_home)? {
        log("Set [features] daemon_auto_start = true in config.toml");
    }
    if settings.manage_app_server {
        if let Err(error) = refresh_app_servers(&settings, &storage.codex_home, "start") {
            log(format!("App-server cleanup failed: {error:#}"));
        }
    }
    let mut state = read_state(storage)?.unwrap_or_default();
    state.pid = std::process::id();
    // Starting (or restarting) the daemon is a manual refresh.
    state.paused_until = None;
    state.paused_active_id = None;
    let mut failures = 0u32;
    log(format!(
        "Watching usage (threshold {}%, interval {}-{}s)",
        settings.threshold,
        settings.min_interval.as_secs(),
        settings.max_interval.as_secs()
    ));
    loop {
        let wait = match tick(storage, &settings, &mut state).await {
            Ok(wait) => {
                failures = 0;
                state.last_error = None;
                wait
            }
            Err(error) => {
                failures = failures.saturating_add(1);
                log(format!("error: {error:#}"));
                state.last_error = Some(format!("{error:#}"));
                (settings.min_interval * 2u32.saturating_pow(failures.min(10) - 1))
                    .min(settings.max_interval)
            }
        };
        let now = Utc::now();
        state.checked_at = Some(now);
        state.next_check_at = chrono::Duration::from_std(wait).ok().map(|wait| now + wait);
        if let Err(error) = write_state(storage, &state) {
            log(format!("Could not write state: {error:#}"));
        }
        tokio::select! {
            _ = sleep(storage, &settings, &mut state, wait) => {}
            signal = tokio::signal::ctrl_c() => {
                signal?;
                log("Stopping");
                return Ok(());
            }
        }
    }
}

/// Sleep for `wait`. While paused, also watch (locally, without network) for
/// a manual switch and resume as soon as one happens.
async fn sleep(storage: &Storage, settings: &Settings, state: &mut State, wait: Duration) {
    let Some(paused_id) = state.paused_active_id.clone() else {
        tokio::time::sleep(wait).await;
        return;
    };
    let deadline = tokio::time::Instant::now() + wait;
    let step = settings.min_interval.min(Duration::from_secs(30));
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + step)).await;
        let active = accounts::load_current(storage)
            .ok()
            .and_then(|store| store.active_account_id);
        if active.is_some_and(|id| id != paused_id) {
            log("Active account changed manually; resuming");
            break;
        }
    }
    state.paused_until = None;
    state.paused_active_id = None;
}

#[cfg(target_os = "linux")]
pub mod systemd {
    use super::*;

    pub const UNIT: &str = "codex-switcher.service";

    fn unit_path() -> Result<PathBuf> {
        Ok(dirs::config_dir()
            .context("Could not find config directory")?
            .join("systemd/user")
            .join(UNIT))
    }

    /// Start, stop, or restart the installed service.
    pub fn control(action: &str) -> Result<()> {
        anyhow::ensure!(
            unit_path()?.exists(),
            "{UNIT} is not installed; run `codex-switcher daemon install` first"
        );
        systemctl(&[action, UNIT])
    }

    fn systemctl(args: &[&str]) -> Result<()> {
        let status = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .status()
            .context("Failed to run systemctl")?;
        anyhow::ensure!(
            status.success(),
            "systemctl --user {} failed",
            args.join(" ")
        );
        Ok(())
    }

    fn quote(arg: &str) -> String {
        if !arg.is_empty()
            && arg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+".contains(c))
        {
            arg.to_owned()
        } else {
            format!(
                "\"{}\"",
                arg.replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('%', "%%")
            )
        }
    }

    /// `path` is the installing shell's PATH, so npm-installed `codex`
    /// wrappers can find `node` under the service manager.
    pub fn render(args: &[String], path: Option<&str>) -> String {
        let exec = args
            .iter()
            .map(|arg| quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        let environment = path
            .map(|path| format!("Environment={}\n", quote(&format!("PATH={path}"))))
            .unwrap_or_default();
        format!(
            "[Unit]\nDescription=Codex account auto-switcher\nAfter=network-online.target\n\n\
             [Service]\n{environment}ExecStart={exec}\nRestart=on-failure\nRestartSec=30\n\n\
             [Install]\nWantedBy=default.target\n"
        )
    }

    /// `args` is the full command line, starting with the executable.
    pub fn install(args: &[String]) -> Result<PathBuf> {
        let path = unit_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let shell_path = std::env::var("PATH").ok();
        std::fs::write(&path, render(args, shell_path.as_deref()))?;
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", "--now", UNIT])?;
        systemctl(&["restart", UNIT])?;
        Ok(path)
    }

    pub fn uninstall() -> Result<PathBuf> {
        let path = unit_path()?;
        if path.exists() {
            let _ = systemctl(&["disable", "--now", UNIT]);
            std::fs::remove_file(&path)?;
            systemctl(&["daemon-reload"])?;
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageWindow;

    fn settings() -> Settings {
        Settings {
            threshold: 1.0,
            min_interval: Duration::from_secs(30),
            max_interval: Duration::from_secs(600),
            cooldown: Duration::from_secs(300),
            base_url: String::new(),
            codex_bin: "codex".into(),
            manage_app_server: false,
        }
    }

    fn row(id: &str, status: &'static str, windows: &[(&str, f64, i64)]) -> AccountUsage {
        AccountUsage {
            id: id.into(),
            name: id.into(),
            is_active: false,
            status,
            plan_type: None,
            windows: windows
                .iter()
                .map(|&(label, remaining, resets)| UsageWindow {
                    label: label.into(),
                    used_percent: 100.0 - remaining,
                    remaining_percent: remaining,
                    limit_window_seconds: None,
                    resets_at: None,
                    resets_in_seconds: Some(resets),
                })
                .collect(),
            credits: None,
            error: None,
        }
    }

    #[test]
    fn interval_tightens_as_5h_quota_drains() {
        let s = settings();
        let secs = |r| next_interval(r, &s).as_secs();
        assert_eq!(secs(None), 600);
        assert_eq!(secs(Some(80.0)), 600);
        assert_eq!(secs(Some(30.0)), 300);
        assert_eq!(secs(Some(10.0)), 120);
        assert_eq!(secs(Some(3.0)), 30);
        let tight = Settings {
            min_interval: Duration::from_secs(200),
            ..s
        };
        assert_eq!(next_interval(Some(10.0), &tight).as_secs(), 200);
    }

    #[test]
    fn picks_most_5h_remaining_among_usable_accounts() {
        let rows = vec![
            row("active", "ok", &[("5h", 0.5, 100), ("weekly", 90.0, 1000)]),
            row(
                "weekly-spent",
                "ok",
                &[("5h", 100.0, 0), ("weekly", 0.0, 5000)],
            ),
            row("errored", "error", &[]),
            row("api", "unsupported", &[]),
            row("some", "ok", &[("5h", 40.0, 100), ("weekly", 50.0, 1000)]),
            row("most", "ok", &[("5h", 70.0, 100), ("weekly", 20.0, 1000)]),
        ];
        assert_eq!(pick_next(&rows, Some("active"), 1.0).unwrap().id, "most");
        assert!(pick_next(&rows[..4], Some("active"), 1.0).is_none());
        assert_eq!(earliest_reset(&rows[..4], 1.0), Some(100));
        assert_eq!(earliest_reset(&rows[1..4], 1.0), Some(5000));
    }

    #[test]
    fn falls_back_to_the_managed_daemon_binary() {
        let home = tempfile::tempdir().unwrap();
        let missing = "codex-switcher-test-missing-binary";
        assert!(resolve_codex_bin(missing, home.path()).is_none());
        assert_eq!(
            resolve_codex_bin("/opt/x/codex", home.path()),
            Some(PathBuf::from("/opt/x/codex"))
        );
        let managed = home.path().join("packages/app-server-daemon/current/bin");
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::write(managed.join("codex"), "").unwrap();
        let resolved = resolve_codex_bin("codex", home.path()).unwrap();
        assert!(resolved.is_file());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unit_quotes_arguments() {
        let unit = systemd::render(
            &[
                "/usr/bin/codex-switcher".into(),
                "--store-dir".into(),
                "/home/a b/s".into(),
                "daemon".into(),
                "run".into(),
            ],
            Some("/home/a b/bin:/usr/bin"),
        );
        assert!(unit.contains(
            "Environment=\"PATH=/home/a b/bin:/usr/bin\"\nExecStart=/usr/bin/codex-switcher --store-dir \"/home/a b/s\" daemon run\n"
        ));
    }
}
