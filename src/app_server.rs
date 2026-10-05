//! Restarting Codex app-servers so running clients pick up a new login.
//!
//! A running app-server refuses to reload an auth.json that belongs to a
//! different account, so an account change only takes effect after a restart.
//! The managed daemon is only ever controlled through `codex app-server daemon`:
//! killing it directly leaves clients unable to reconnect.
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::{config, processes};

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

/// Run `codex app-server daemon <action>` against `codex_home`.
pub fn daemon_command(codex_bin: &str, codex_home: &Path, action: &str) -> Result<()> {
    let codex = resolve_codex_bin(codex_bin, codex_home).with_context(|| {
        format!("Could not find `{codex_bin}` on PATH; pass --codex-bin with an absolute path")
    })?;
    let output = Command::new(&codex)
        .args(["app-server", "daemon", action])
        .env("CODEX_HOME", codex_home)
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

#[derive(Debug, Default)]
pub struct Refresh {
    /// `daemon_auto_start` had to be switched on in config.toml.
    pub config_changed: bool,
    /// Client-spawned app-servers that were stopped.
    pub stopped: Vec<u32>,
}

/// Route clients through the managed app-server after a manual account switch:
/// enable `daemon_auto_start`, stop app-servers that clients spawned
/// themselves (e.g. the desktop app), and run `codex app-server daemon restart`.
pub fn restart(codex_bin: &str, codex_home: &Path) -> Result<Refresh> {
    let config_changed = config::ensure_daemon_auto_start(codex_home)?;
    let stopped = processes::stop_standalone_app_servers()?;
    daemon_command(codex_bin, codex_home, "restart")?;
    Ok(Refresh {
        config_changed,
        stopped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[cfg(unix)]
    #[test]
    fn daemon_command_targets_the_given_codex_home() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let script = dir.path().join("fake-codex");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$CODEX_HOME $*\" > '{}'\n[ \"$3\" != fail ]\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let home = dir.path().join("home");
        daemon_command(script.to_str().unwrap(), &home, "restart").unwrap();
        assert_eq!(
            std::fs::read_to_string(&log).unwrap().trim(),
            format!("{} app-server daemon restart", home.display())
        );
        assert!(daemon_command(script.to_str().unwrap(), &home, "fail").is_err());
    }
}
