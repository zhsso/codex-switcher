//! Isolated Codex homes, so several accounts can run at the same time.
//!
//! A fork lives in `<store>/homes/<name>` and keeps its own credentials, state,
//! and managed daemon. Nothing records which account a fork uses: its auth.json
//! is the source of truth, including after `switch` or `codex login` inside it.
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::storage::{same_path, Storage};
use crate::types::AuthDotJson;
use crate::{accounts, app_server, config, processes};

const MARKER: &str = ".codex-switcher-fork.json";

/// Read-mostly customizations that should stay the same in every home. Packages
/// are not shared: each daemon installs and updates its own copy.
const SHARED: &[&str] = &[
    "skills",
    "plugins",
    "vendor_imports",
    "rules",
    "prompts",
    "agents",
    "AGENTS.md",
    "AGENTS.override.md",
];

#[derive(Serialize, Deserialize)]
struct Marker {
    source: PathBuf,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ForkInfo {
    pub name: String,
    pub account: Option<String>,
    pub path: PathBuf,
    pub source: PathBuf,
}

fn root(storage: &Storage) -> PathBuf {
    storage.directory.join("homes")
}

fn read_marker(directory: &Path) -> Option<Marker> {
    serde_json::from_slice(&fs::read(directory.join(MARKER)).ok()?).ok()
}

/// Every fork, sorted by name. A directory without a marker is not a fork.
fn forks(storage: &Storage) -> Vec<(String, PathBuf, Marker)> {
    let Ok(entries) = fs::read_dir(root(storage)) else {
        return Vec::new();
    };
    let mut forks: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let marker = read_marker(&entry.path())?;
            Some((
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
                marker,
            ))
        })
        .collect();
    forks.sort_by(|left, right| left.0.cmp(&right.0));
    forks
}

/// The login of every known home other than the current one: all forks and the
/// homes they came from. Unreadable files are skipped, because a broken fork
/// must not stop accounts from working everywhere else.
pub fn peer_auths(storage: &Storage) -> Vec<(PathBuf, AuthDotJson)> {
    let mut homes: Vec<PathBuf> = Vec::new();
    for (_, path, marker) in forks(storage) {
        for home in [path, marker.source] {
            if !same_path(&home, &storage.codex_home)
                && !homes.iter().any(|known| same_path(known, &home))
            {
                homes.push(home);
            }
        }
    }
    homes
        .into_iter()
        .filter_map(|home| {
            let auth = storage.with_home(home.clone()).read_auth().ok()??;
            Some((home, auth))
        })
        .collect()
}

fn validate_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && !name.starts_with('.')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
        "Fork name '{name}' must use letters, digits, '.', '_' or '-' and not start with '.'"
    );
    Ok(())
}

/// The directory of an existing fork.
pub fn path(storage: &Storage, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    let path = std::path::absolute(root(storage).join(name))?;
    anyhow::ensure!(
        read_marker(&path).is_some(),
        "Fork '{name}' not found; use `codex-switcher fork list` to see forks"
    );
    Ok(path)
}

/// Replace this process with `program`, running against the fork at `path`, so
/// the terminal, signals, and exit code all belong to that program.
pub fn exec(path: &Path, program: &OsStr, args: &[String]) -> Result<()> {
    let mut command = Command::new(program);
    command.args(args).env("CODEX_HOME", path);
    let run = || format!("Could not run {}", program.to_string_lossy());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).with_context(run)
    }
    #[cfg(not(unix))]
    {
        let status = command.status().with_context(run)?;
        std::process::exit(status.code().unwrap_or(1))
    }
}

pub fn list(storage: &Storage) -> Result<Vec<ForkInfo>> {
    let store = storage.load()?;
    Ok(forks(storage)
        .into_iter()
        .map(|(name, path, marker)| {
            let auth = storage.with_home(path.clone()).read_auth().ok().flatten();
            ForkInfo {
                name,
                account: auth.as_ref().and_then(|auth| {
                    accounts::account_for_auth(&store, auth).map(|account| account.name.clone())
                }),
                path,
                source: marker.source,
            }
        })
        .collect())
}

fn populate(source: &Path, target: &Path) -> Result<()> {
    let marker = Marker {
        source: source.to_path_buf(),
        created_at: Utc::now(),
    };
    fs::write(target.join(MARKER), serde_json::to_vec_pretty(&marker)?)?;
    match fs::copy(source.join("config.toml"), target.join("config.toml")) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Failed to copy config.toml"),
    }
    // Clients reach the fork's own daemon, which can then be restarted on a switch.
    config::ensure_daemon_auto_start(target)?;
    #[cfg(unix)]
    for entry in SHARED {
        if let Ok(shared) = fs::canonicalize(source.join(entry)) {
            std::os::unix::fs::symlink(shared, target.join(entry))
                .with_context(|| format!("Failed to link {entry}"))?;
        }
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

/// Fork the current Codex home. Returns the new directory and the account name.
pub async fn create(storage: &Storage, name: &str, account: &str) -> Result<(PathBuf, String)> {
    anyhow::ensure!(
        cfg!(target_os = "linux"),
        "Forks are only supported on Linux, where processes can be told apart by Codex home"
    );
    validate_name(name)?;
    // Report an unknown account before touching the disk.
    let store = accounts::load_current(storage)?;
    accounts::resolve(&store, account)?;

    let root = std::path::absolute(root(storage))?;
    fs::create_dir_all(&root)?;
    let target = root.join(name);
    create_private_dir(&target).map_err(|error| match error.kind() {
        std::io::ErrorKind::AlreadyExists => anyhow::anyhow!("Fork '{name}' already exists"),
        _ => anyhow::Error::new(error).context(format!("Failed to create {}", target.display())),
    })?;

    let source = std::path::absolute(&storage.codex_home)?;
    let fork_storage = storage.with_home(target.clone());
    let result = match populate(&source, &target) {
        Ok(()) => accounts::switch(&fork_storage, account).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(account) => Ok((target, account)),
        Err(error) => {
            // This directory was created above and has never been used.
            let _ = fs::remove_dir_all(&target);
            Err(error)
        }
    }
}

/// Stop the fork's daemon, save its rotated tokens, and delete it.
pub fn remove(storage: &Storage, name: &str, codex_bin: &str) -> Result<()> {
    let path = path(storage, name)?;
    let running = processes::list_running(&path)?;
    let clients = running
        .iter()
        .filter(|process| process.kind.is_closable() && !process.managed)
        .count();
    anyhow::ensure!(
        clients == 0,
        "{clients} process(es) are still using fork '{name}'; close them or run `CODEX_HOME={} codex-switcher stop`",
        path.display()
    );
    if running
        .iter()
        .any(|process| process.managed && process.kind.is_closable())
    {
        app_server::daemon_command(codex_bin, &path, "stop")?;
    }
    processes::stop_updaters(&path)?;
    let remaining = processes::list_running(&path)?.len();
    anyhow::ensure!(
        remaining == 0,
        "{remaining} process(es) are still using fork '{name}'; try again shortly"
    );

    // Deleting the fork's auth.json must not lose the only valid refresh token.
    let _lock = storage.lock()?;
    let store = accounts::load_current(storage)?;
    storage.save(&store)?;
    fs::remove_dir_all(&path).with_context(|| format!("Failed to delete {}", path.display()))
}
