//! Explicit inspection and graceful shutdown of local Codex and ChatGPT processes.

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

#[cfg(windows)]
use std::collections::HashSet;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessKind {
    CodexCli,
    CodexDesktop,
    CodexAppServer,
    /// The managed daemon's auto-updater; never stopped by `stop`.
    CodexAppServerUpdater,
    /// Helper spawned by an app-server or the CLI to run code-mode tools.
    CodexCodeModeHost,
    ChatGpt,
}

impl ProcessKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::CodexCli => "Codex CLI",
            Self::CodexDesktop => "Codex desktop",
            Self::CodexAppServer => "Codex app-server",
            Self::CodexAppServerUpdater => "Codex app-server updater",
            Self::CodexCodeModeHost => "Codex code-mode host",
            Self::ChatGpt => "ChatGPT",
        }
    }

    /// Whether `stop` closes this kind of process.
    pub fn is_closable(&self) -> bool {
        *self != Self::CodexAppServerUpdater
    }

    fn app_server(name: &str, command: &str) -> Self {
        // Linux truncates process names to 15 bytes, so also check the executable.
        let executable = command
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches('"')
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let name = name.to_ascii_lowercase();
        if [name, executable]
            .iter()
            .any(|value| value == "codex-code-mode-host" || value == "codex-code-mode-host.exe")
        {
            Self::CodexCodeModeHost
        } else if command
            .split_whitespace()
            .any(|token| token.trim_matches('"') == "pid-update-loop")
        {
            Self::CodexAppServerUpdater
        } else {
            Self::CodexAppServer
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunningProcess {
    pub pid: u32,
    pub kind: ProcessKind,
    bundle_id: Option<String>,
}

impl RunningProcess {
    fn new(pid: u32, kind: ProcessKind) -> Self {
        Self {
            pid,
            kind,
            bundle_id: None,
        }
    }
}

/// Return active Codex CLI, Codex desktop, app-server, and ChatGPT processes.
pub fn list_running() -> Result<Vec<RunningProcess>> {
    #[cfg(unix)]
    {
        return list_unix_processes();
    }

    #[cfg(windows)]
    {
        return list_windows_processes();
    }

    #[allow(unreachable_code)]
    Ok(Vec::new())
}

/// Gracefully close the reviewed process list. New processes are left alone and
/// cause the operation to stop so the user can review them first. Processes
/// that are not closable (the daemon updater) are always left running.
pub fn stop(targets: &[RunningProcess]) -> Result<Vec<u32>> {
    let targets: Vec<_> = targets
        .iter()
        .filter(|process| process.kind.is_closable())
        .collect();
    if targets.is_empty() {
        return Ok(Vec::new());
    }

    let current: Vec<_> = list_running()?
        .into_iter()
        .filter(|process| process.kind.is_closable())
        .collect();
    let reviewed: std::collections::HashMap<_, _> = targets
        .iter()
        .map(|process| (process.pid, *process))
        .collect();
    let mut live = Vec::new();

    for process in current {
        match reviewed.get(&process.pid) {
            Some(previous) if **previous == process => live.push(process),
            Some(_) => anyhow::bail!(
                "Process identity changed for PID {}; inspect again with `codex-switcher ps`",
                process.pid
            ),
            None => anyhow::bail!(
                "A new {} process (PID {}) appeared; inspect again with `codex-switcher ps`",
                process.kind.label(),
                process.pid
            ),
        }
    }

    for process in &live {
        request_close(process);
    }

    wait_for_exit(&live, Duration::from_secs(8));
    let remaining: Vec<_> = live
        .iter()
        .filter(|process| process_exists(process.pid))
        .map(|process| process.pid)
        .collect();
    anyhow::ensure!(
        remaining.is_empty(),
        "Could not close process{} {}; still running after a graceful shutdown request",
        if remaining.len() == 1 { "" } else { "es" },
        remaining
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );

    Ok(live.iter().map(|process| process.pid).collect())
}

/// Terminate app-server server processes that clients spawned themselves,
/// leaving clients (`app-server proxy`), other subcommands, code-mode helpers,
/// and the managed daemon (under `app-server-daemon/`) alone.
pub fn stop_standalone_app_servers() -> Result<Vec<u32>> {
    #[cfg(unix)]
    {
        let output = Command::new("ps")
            .args(["-eo", "pid=,tty=,ucomm=,command="])
            .output()
            .context("failed to query the local process list with ps")?;
        anyhow::ensure!(
            output.status.success(),
            "ps could not read the process list"
        );
        let targets: Vec<_> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(parse_unix_process_line)
            .filter(|process| process.pid != std::process::id())
            .filter(|process| {
                is_app_server_daemon_process(&process.command)
                    && !process.command.contains("app-server-daemon")
            })
            .map(|process| RunningProcess::new(process.pid, ProcessKind::CodexAppServer))
            .collect();
        for process in &targets {
            request_close(process);
        }
        wait_for_exit(&targets, Duration::from_secs(8));
        let remaining: Vec<_> = targets
            .iter()
            .filter(|process| process_exists(process.pid))
            .map(|process| process.pid.to_string())
            .collect();
        anyhow::ensure!(
            remaining.is_empty(),
            "app-server process(es) {} still running after a shutdown request",
            remaining.join(", ")
        );
        Ok(targets.iter().map(|process| process.pid).collect())
    }

    #[cfg(not(unix))]
    {
        anyhow::bail!("Stopping app-server processes is only supported on Unix")
    }
}

/// `... app-server [--flags]` is a server; `app-server <subcommand>` is not.
#[cfg(unix)]
fn is_app_server_daemon_process(command: &str) -> bool {
    if command.contains("codex-switcher") {
        return false;
    }
    let mut tokens = command
        .split_whitespace()
        .map(|token| token.trim_matches('"'));
    if !tokens.any(|token| token == "app-server") {
        return false;
    }
    tokens.next().is_none_or(|next| next.starts_with('-'))
}

#[cfg(unix)]
#[derive(Debug, Eq, PartialEq)]
struct UnixProcess {
    pid: u32,
    name: String,
    command: String,
}

#[cfg(unix)]
fn list_unix_processes() -> Result<Vec<RunningProcess>> {
    #[cfg(target_os = "macos")]
    let ps_args = ["-axo", "pid=,tty=,ucomm=,command="];
    #[cfg(not(target_os = "macos"))]
    let ps_args = ["-eo", "pid=,tty=,ucomm=,command="];

    let output = Command::new("ps")
        .args(ps_args)
        .output()
        .context("failed to query the local process list with ps")?;
    anyhow::ensure!(
        output.status.success(),
        "ps could not read the process list"
    );

    let this_pid = std::process::id();
    let mut processes = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some(process) = parse_unix_process_line(line) else {
            continue;
        };
        if process.pid == this_pid {
            continue;
        }
        if let Some(process) = classify_unix_process(process) {
            processes.push(process);
        }
    }
    processes.sort_by_key(|process| process.pid);
    processes.dedup_by_key(|process| process.pid);
    Ok(processes)
}

#[cfg(unix)]
fn parse_unix_process_line(line: &str) -> Option<UnixProcess> {
    let (pid, remaining) = take_field(line)?;
    let (_, remaining) = take_field(remaining)?;
    let (name, command) = take_field(remaining)?;
    Some(UnixProcess {
        pid: pid.parse().ok()?,
        name: name.to_owned(),
        command: command.trim().to_owned(),
    })
}

#[cfg(unix)]
fn take_field(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    (end > 0).then(|| (&input[..end], &input[end..]))
}

#[cfg(unix)]
fn classify_unix_process(process: UnixProcess) -> Option<RunningProcess> {
    let lower_name = process.name.to_ascii_lowercase();
    let lower_command = process.command.to_ascii_lowercase();
    if lower_command.contains("codex-switcher") || lower_command.contains("--type=") {
        return None;
    }

    #[cfg(target_os = "macos")]
    {
        if is_app_server_process(&process.name, &process.command) {
            return Some(RunningProcess::new(
                process.pid,
                ProcessKind::app_server(&process.name, &process.command),
            ));
        }
        let is_codex_cli = process.name == "codex"
            || process.name != "Codex" && executable_name(&process.command) == Some("codex");
        if is_codex_cli {
            return Some(RunningProcess::new(process.pid, ProcessKind::CodexCli));
        }
        if let Some((kind, bundle_id)) = macos_desktop_identity(&process.name, &process.command) {
            return Some(RunningProcess {
                pid: process.pid,
                kind,
                bundle_id: Some(bundle_id),
            });
        }
        return None;
    }

    #[cfg(not(target_os = "macos"))]
    {
        if is_app_server_process(&process.name, &process.command) {
            return Some(RunningProcess::new(
                process.pid,
                ProcessKind::app_server(&process.name, &process.command),
            ));
        }
        if process.name == "Codex" {
            return Some(RunningProcess::new(process.pid, ProcessKind::CodexDesktop));
        }
        let is_codex_cli = process.name == "codex"
            || process.name != "Codex" && executable_name(&process.command) == Some("codex");
        if is_codex_cli {
            return Some(RunningProcess::new(process.pid, ProcessKind::CodexCli));
        }
        if lower_name == "chatgpt" {
            return Some(RunningProcess::new(process.pid, ProcessKind::ChatGpt));
        }
        None
    }
}

#[cfg(unix)]
fn is_app_server_process(name: &str, command: &str) -> bool {
    let lower_name = name.to_ascii_lowercase();
    let lower_command = command.to_ascii_lowercase();
    lower_name == "codex-code-mode-host"
        || lower_name == "codex-code-mode-host.exe"
        || executable_name(command) == Some("codex-code-mode-host")
        || lower_command.contains("app-server-daemon")
        || lower_command
            .split_whitespace()
            .any(|token| token.trim_matches('"') == "app-server")
}

#[cfg(unix)]
fn executable_name(command: &str) -> Option<&str> {
    let command = command.trim_start();
    let executable = if let Some(quoted) = command.strip_prefix('"') {
        quoted.split_once('"')?.0
    } else {
        command.split_whitespace().next()?
    };
    executable.rsplit('/').next()
}

#[cfg(target_os = "macos")]
fn macos_desktop_identity(name: &str, command: &str) -> Option<(ProcessKind, String)> {
    let (suffix, kind) = match name {
        "Codex" => ("/Codex.app/Contents/MacOS/Codex", ProcessKind::CodexDesktop),
        "ChatGPT" => ("/ChatGPT.app/Contents/MacOS/ChatGPT", ProcessKind::ChatGpt),
        _ => return None,
    };
    let start = command.find(suffix)?;
    let end = start + suffix.len();
    if command[end..]
        .chars()
        .next()
        .is_some_and(|character| !character.is_whitespace() && character != '"')
    {
        return None;
    }
    let bundle_suffix = if name == "Codex" {
        "/Codex.app"
    } else {
        "/ChatGPT.app"
    };
    let bundle_end = start + bundle_suffix.len();
    let app_path = command[..bundle_end].trim_start_matches('"');
    let info_plist = std::path::Path::new(app_path).join("Contents/Info.plist");
    let output = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
        .arg(info_plist)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let bundle_id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if (name == "Codex" && bundle_id != "com.openai.codex")
        || (name == "ChatGPT" && bundle_id != "com.openai.chat" && bundle_id != "com.openai.codex")
    {
        return None;
    }
    let kind = if bundle_id == "com.openai.codex" {
        ProcessKind::CodexDesktop
    } else {
        kind
    };
    Some((kind, bundle_id))
}

#[cfg(windows)]
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WindowsProcess {
    name: String,
    process_id: u32,
    parent_process_id: u32,
    #[serde(default)]
    command_line: String,
    #[serde(default)]
    executable_path: String,
    #[serde(default)]
    main_window_title: String,
}

#[cfg(windows)]
fn list_windows_processes() -> Result<Vec<RunningProcess>> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const SCRIPT: &str = r#"
$titles = @{}
Get-Process -Name Codex,ChatGPT -ErrorAction SilentlyContinue | ForEach-Object {
  $titles[[uint32]$_.Id] = [string]$_.MainWindowTitle
}
$items = @(
  Get-CimInstance Win32_Process |
Where-Object {
  $_.Name -ieq 'Codex.exe' -or
  $_.Name -ieq 'ChatGPT.exe' -or
  $_.Name -ieq 'codex-code-mode-host.exe'
} |
    ForEach-Object {
      [PSCustomObject]@{
        Name = $_.Name
        ProcessId = [uint32]$_.ProcessId
        ParentProcessId = [uint32]$_.ParentProcessId
        CommandLine = if ($_.CommandLine) { [string]$_.CommandLine } else { '' }
        ExecutablePath = if ($_.ExecutablePath) { [string]$_.ExecutablePath } else { '' }
        MainWindowTitle = if ($titles.ContainsKey([uint32]$_.ProcessId)) { $titles[[uint32]$_.ProcessId] } else { '' }
      }
    }
)
ConvertTo-Json -InputObject $items -Compress
"#;

    let output = Command::new("powershell.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .output()
        .context("failed to query Windows processes with PowerShell")?;
    anyhow::ensure!(
        output.status.success(),
        "PowerShell process query failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text = String::from_utf8_lossy(&output.stdout);
    if text.trim().is_empty() || text.trim() == "null" {
        return Ok(Vec::new());
    }
    let value: serde_json::Value =
        serde_json::from_str(&text).context("failed to parse Windows process list")?;
    let entries: Vec<WindowsProcess> = match value {
        serde_json::Value::Array(items) => items
            .into_iter()
            .map(serde_json::from_value)
            .collect::<std::result::Result<_, _>>()?,
        item => vec![serde_json::from_value(item)?],
    };
    Ok(classify_windows_processes(&entries))
}

#[cfg(windows)]
fn classify_windows_processes(entries: &[WindowsProcess]) -> Vec<RunningProcess> {
    let mut processes = Vec::new();
    for process in entries {
        let name = process.name.to_ascii_lowercase();
        let command = process.command_line.to_ascii_lowercase();
        let executable = process
            .executable_path
            .replace('/', "\\")
            .to_ascii_lowercase();
        if command.contains("--type=") || command.contains("codex-switcher") {
            continue;
        }
        if name == "codex-code-mode-host.exe"
            || command.contains("app-server-daemon")
            || command
                .split_whitespace()
                .any(|token| token.trim_matches('"') == "app-server")
        {
            processes.push(RunningProcess::new(
                process.process_id,
                ProcessKind::app_server(&name, &command),
            ));
        } else if name == "codex.exe" {
            if command.contains("\\resources\\codex.exe")
                || executable.contains("\\resources\\codex.exe")
            {
                continue;
            }
            let has_renderer = windows_has_renderer_descendant(process.process_id, entries);
            let kind = if !process.main_window_title.trim().is_empty() || has_renderer {
                ProcessKind::CodexDesktop
            } else {
                ProcessKind::CodexCli
            };
            processes.push(RunningProcess::new(process.process_id, kind));
        } else if name == "chatgpt.exe"
            && (!process.main_window_title.trim().is_empty()
                || windows_has_renderer_descendant(process.process_id, entries))
        {
            let executable_path = if process.executable_path.is_empty() {
                windows_command_executable_path(&process.command_line).unwrap_or_default()
            } else {
                &process.executable_path
            };
            let kind = if is_windows_codex_desktop_chatgpt_path(executable_path) {
                ProcessKind::CodexDesktop
            } else {
                ProcessKind::ChatGpt
            };
            processes.push(RunningProcess::new(process.process_id, kind));
        }
    }
    processes.sort_by_key(|process| process.pid);
    processes.dedup_by_key(|process| process.pid);
    processes
}

#[cfg(windows)]
fn windows_command_executable_path(command_line: &str) -> Option<&str> {
    let command_line = command_line.trim_start();
    if let Some(quoted) = command_line.strip_prefix('"') {
        return quoted
            .split_once('"')
            .map(|(path, _)| path)
            .filter(|path| !path.is_empty());
    }
    command_line.split_whitespace().next()
}

#[cfg(windows)]
fn is_windows_codex_desktop_chatgpt_path(path: &str) -> bool {
    let normalized = path.replace('/', "\\").to_ascii_lowercase();
    let Some(package_path) = normalized.strip_suffix("\\app\\chatgpt.exe") else {
        return false;
    };
    let mut components = package_path.rsplit('\\');
    let Some(package_name) = components.next() else {
        return false;
    };
    let Some(package_parent) = components.next() else {
        return false;
    };
    package_parent == "windowsapps"
        && package_name.starts_with("openai.codex_")
        && package_name.ends_with("__2p2nqsd0c76g0")
}

#[cfg(windows)]
fn windows_has_renderer_descendant(root: u32, entries: &[WindowsProcess]) -> bool {
    let mut pending = vec![root];
    let mut visited = HashSet::new();
    while let Some(parent) = pending.pop() {
        if !visited.insert(parent) {
            continue;
        }
        for child in entries
            .iter()
            .filter(|process| process.parent_process_id == parent && process.process_id != root)
        {
            if child
                .command_line
                .to_ascii_lowercase()
                .contains("--type=renderer")
            {
                return true;
            }
            pending.push(child.process_id);
        }
    }
    false
}

fn request_close(process: &RunningProcess) {
    #[cfg(target_os = "macos")]
    if let Some(bundle_id) = &process.bundle_id {
        let script = format!("tell application id \"{bundle_id}\" to quit");
        if Command::new("/usr/bin/osascript")
            .args(["-e", &script])
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }
    }

    #[cfg(unix)]
    {
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &process.pid.to_string()])
            .status();
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let _ = Command::new("taskkill")
            .creation_flags(CREATE_NO_WINDOW)
            .args(["/T", "/PID", &process.pid.to_string()])
            .status();
    }
}

fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        return Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "pid="])
            .output()
            .map(|output| {
                if !output.status.success() {
                    return true;
                }
                String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .any(|value| value == pid.to_string())
            })
            .unwrap_or(true);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        return Command::new("tasklist")
            .creation_flags(CREATE_NO_WINDOW)
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .map(|output| {
                if !output.status.success() {
                    return true;
                }
                String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                    line.split(',')
                        .nth(1)
                        .is_some_and(|field| field.trim().trim_matches('"') == pid.to_string())
                })
            })
            .unwrap_or(true);
    }

    #[allow(unreachable_code)]
    false
}

fn wait_for_exit(processes: &[RunningProcess], timeout: Duration) {
    let started = Instant::now();
    while processes.iter().any(|process| process_exists(process.pid)) && started.elapsed() < timeout
    {
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{classify_unix_process, parse_unix_process_line, ProcessKind, UnixProcess};

    #[cfg(unix)]
    #[test]
    fn parses_unix_process_fields_without_splitting_paths_with_spaces() {
        let process = parse_unix_process_line(
            " 4321 ?? ChatGPT /Applications/ChatGPT Desktop.app/Contents/MacOS/ChatGPT --flag",
        )
        .unwrap();
        assert_eq!(process.pid, 4321);
        assert_eq!(process.name, "ChatGPT");
        assert_eq!(
            process.command,
            "/Applications/ChatGPT Desktop.app/Contents/MacOS/ChatGPT --flag"
        );
    }

    #[cfg(unix)]
    #[test]
    fn parses_unix_process_fields_with_variable_spacing() {
        let process = parse_unix_process_line("  77     pts/4   codex    codex login").unwrap();
        assert_eq!(
            process,
            UnixProcess {
                pid: 77,
                name: "codex".to_owned(),
                command: "codex login".to_owned(),
            }
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn recognizes_only_app_server_servers() {
        use super::is_app_server_daemon_process as server;
        assert!(server("/usr/lib/chatgpt/resources/codex -c features.x=true app-server --analytics-default-enabled -c a=b"));
        assert!(server("/home/u/.codex/packages/app-server-daemon/releases/0.1/bin/codex app-server --listen unix://"));
        assert!(server("codex app-server"));
        assert!(!server("codex app-server proxy"));
        assert!(!server("codex app-server daemon restart"));
        assert!(!server(
            "/home/u/.npm/codex-linux-x64/bin/codex-code-mode-host"
        ));
        assert!(!server("codex --yolo resume --last"));
        assert!(!server("codex-switcher daemon run app-server"));
    }

    #[test]
    fn classifies_cli_chatgpt_and_app_server_processes() {
        let cli = classify_unix_process(UnixProcess {
            pid: 10,
            name: "codex".to_owned(),
            command: "/home/user/.local/bin/codex login".to_owned(),
        })
        .unwrap();
        assert_eq!(cli.kind, ProcessKind::CodexCli);

        let chatgpt = classify_unix_process(UnixProcess {
            pid: 11,
            name: "ChatGPT".to_owned(),
            command: "/opt/ChatGPT/ChatGPT --no-sandbox".to_owned(),
        })
        .unwrap();
        assert_eq!(chatgpt.kind, ProcessKind::ChatGpt);

        let app_server = classify_unix_process(UnixProcess {
            pid: 12,
            name: "codex".to_owned(),
            command: "/home/user/.codex/packages/app-server-daemon/releases/0.1/bin/codex app-server --listen unix://".to_owned(),
        });
        assert_eq!(app_server.unwrap().kind, ProcessKind::CodexAppServer);

        let code_mode_host = classify_unix_process(UnixProcess {
            pid: 13,
            name: "codex-code-mode".to_owned(),
            command:
                "/home/user/.codex/packages/app-server-daemon/releases/0.1/bin/codex-code-mode-host"
                    .to_owned(),
        });
        assert_eq!(code_mode_host.unwrap().kind, ProcessKind::CodexCodeModeHost);

        let cli_code_mode_host = classify_unix_process(UnixProcess {
            pid: 15,
            name: "codex-code-mode".to_owned(),
            command: "/home/user/.npm/codex-linux-x64/bin/codex-code-mode-host".to_owned(),
        });
        assert_eq!(
            cli_code_mode_host.unwrap().kind,
            ProcessKind::CodexCodeModeHost
        );

        let updater = classify_unix_process(UnixProcess {
            pid: 14,
            name: "codex".to_owned(),
            command: "/home/user/.codex/packages/app-server-daemon/releases/0.1/bin/codex app-server daemon pid-update-loop".to_owned(),
        })
        .unwrap();
        assert_eq!(updater.kind, ProcessKind::CodexAppServerUpdater);
        assert!(!updater.kind.is_closable());
    }
}
