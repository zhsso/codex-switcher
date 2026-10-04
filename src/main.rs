use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use codex_switcher::{
    accounts, auth, daemon, processes, status, storage::Storage, types::StoredAccount, usage,
};

#[derive(Parser)]
#[command(
    version,
    about = "Manage and switch Codex accounts",
    after_help = "Account selectors accept an exact name or full ID. Use `list` (alias: `ls`) to see saved accounts."
)]
struct Cli {
    /// Terminal colors; auto respects NO_COLOR and disables colors in pipes
    #[arg(long, global = true, value_enum, default_value = "auto")]
    color: ColorMode,
    /// Account storage directory (default: ~/.codex-switcher)
    #[arg(long, global = true)]
    store_dir: Option<PathBuf>,
    /// Codex directory (default: CODEX_HOME or ~/.codex)
    #[arg(long, global = true)]
    codex_home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Subcommand)]
enum Command {
    /// Save an account; defaults to importing the current Codex auth.json
    Add {
        /// Display name; defaults to the email or account identity
        name: Option<String>,
        #[command(flatten)]
        source: Source,
        /// Sign in through the browser using ChatGPT OAuth
        #[arg(long, conflicts_with_all = ["file", "api_key_stdin"])]
        login: bool,
        /// Print the login URL without opening a browser
        #[arg(long, requires = "login")]
        no_browser: bool,
    },
    /// Forget a saved account without changing the current Codex login
    Remove { account: String },
    /// Rename an account and/or replace its credentials
    Edit {
        account: String,
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        source: Source,
    },
    /// Save the live session, refresh the target if needed, and write auth.json
    Switch {
        /// Exact name or full ID; multiple shell words are joined with spaces
        #[arg(required = true, num_args = 1..)]
        account: Vec<String>,
    },
    /// Fetch usage and reset times for an account, or all saved accounts
    Status {
        account: Option<String>,
        #[arg(long)]
        json: bool,
        /// Backend base URL receiving the account token (Codex path selection)
        #[arg(long, default_value = usage::DEFAULT_BASE_URL)]
        base_url: String,
    },
    /// Show saved accounts without credentials or network requests
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show running Codex CLI, Codex desktop, app-server, and ChatGPT processes
    Ps,
    /// Gracefully close the listed Codex, app-server, and ChatGPT processes
    Stop {
        /// Close without prompting for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Watch the active account's usage and auto-switch when it runs out
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Run the watcher in the foreground (used by the systemd unit)
    Run(DaemonArgs),
    /// Install and start a systemd user service running `daemon run` (Linux)
    Install(DaemonArgs),
    /// Stop and remove the systemd user service (Linux)
    Uninstall,
    /// Start the installed systemd user service (Linux)
    Start,
    /// Stop the installed systemd user service (Linux)
    Stop,
    /// Restart the installed systemd user service (Linux)
    Restart,
    /// Show the watcher's last check, next check, and last switch
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
struct DaemonArgs {
    /// Switch when the active account's remaining percent falls below this
    #[arg(long, default_value_t = 1.0)]
    threshold: f64,
    /// Shortest polling interval in seconds (used when the 5h quota is nearly spent)
    #[arg(long, default_value_t = 30)]
    min_interval: u64,
    /// Longest polling interval in seconds (used when over 50% of 5h quota is left)
    #[arg(long, default_value_t = 600)]
    max_interval: u64,
    /// Minimum seconds between automatic switches
    #[arg(long, default_value_t = 300)]
    cooldown: u64,
    /// Codex executable used for `codex app-server daemon restart`
    #[arg(long, default_value = "codex")]
    codex_bin: String,
    /// Backend base URL receiving the account token (Codex path selection)
    #[arg(long, default_value = usage::DEFAULT_BASE_URL)]
    base_url: String,
    /// Only switch accounts; never stop, start, or restart app-server processes
    #[arg(long)]
    no_restart: bool,
}

impl DaemonArgs {
    fn settings(&self) -> Result<daemon::Settings> {
        anyhow::ensure!(
            self.threshold.is_finite() && (0.0..100.0).contains(&self.threshold),
            "--threshold must be between 0 and 100"
        );
        anyhow::ensure!(
            0 < self.min_interval && self.min_interval <= self.max_interval,
            "--min-interval must be positive and not exceed --max-interval"
        );
        Ok(daemon::Settings {
            threshold: self.threshold,
            min_interval: std::time::Duration::from_secs(self.min_interval),
            max_interval: std::time::Duration::from_secs(self.max_interval),
            cooldown: std::time::Duration::from_secs(self.cooldown),
            base_url: self.base_url.clone(),
            codex_bin: self.codex_bin.clone(),
            manage_app_server: !self.no_restart,
        })
    }

    #[cfg(target_os = "linux")]
    fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            "--threshold".into(),
            self.threshold.to_string(),
            "--min-interval".into(),
            self.min_interval.to_string(),
            "--max-interval".into(),
            self.max_interval.to_string(),
            "--cooldown".into(),
            self.cooldown.to_string(),
            "--codex-bin".into(),
            self.codex_bin.clone(),
            "--base-url".into(),
            self.base_url.clone(),
        ];
        if self.no_restart {
            args.push("--no-restart".into());
        }
        args
    }
}

#[derive(Args)]
struct Source {
    /// Import credentials from an auth.json file
    #[arg(long, conflicts_with = "api_key_stdin")]
    file: Option<PathBuf>,
    /// Read an API key from standard input (never from command-line arguments)
    #[arg(long)]
    api_key_stdin: bool,
}

fn import(storage: &Storage, source: &Source, name: String) -> Result<StoredAccount> {
    if source.api_key_stdin {
        let mut key = String::new();
        io::stdin().read_to_string(&mut key)?;
        let key = key.trim();
        anyhow::ensure!(!key.is_empty(), "API key must not be empty");
        return Ok(StoredAccount::new_api_key(name, key.to_string()));
    }
    let path = source.file.clone().unwrap_or_else(|| storage.auth_path());
    let contents = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "Failed to read {}; use --file, --api-key-stdin, or add --login",
            path.display()
        )
    })?;
    auth::credentials::import_from_auth_json_contents(&contents, name)
}

#[tokio::main]
async fn main() {
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let theme = codex_switcher::output::Theme {
        color: match cli.color {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => {
                io::stdout().is_terminal()
                    && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
                    && std::env::var("TERM").as_deref() != Ok("dumb")
            }
        },
    };
    #[cfg(target_os = "linux")]
    let explicit_dirs = (cli.store_dir.clone(), cli.codex_home.clone());
    let storage = Storage::new(cli.store_dir, cli.codex_home)?;
    match cli.command {
        Command::Add {
            name,
            source,
            login,
            no_browser,
        } => {
            if let Some(name) = &name {
                anyhow::ensure!(!name.trim().is_empty(), "Account name must not be blank");
            }
            let name = name.map(|name| name.trim().to_owned()).unwrap_or_default();
            if !name.is_empty() {
                accounts::validate_name(&storage.load()?, &name, None)?;
            }
            let account = if login {
                let (info, receiver, cancelled) = auth::oauth::start_oauth_login(name).await?;
                eprintln!(
                    "Open this URL to sign in (waiting up to 5 minutes):\n{}",
                    info.auth_url
                );
                if !no_browser {
                    if let Err(error) = webbrowser::open(&info.auth_url) {
                        eprintln!("Could not open browser: {error}. Open the URL above manually.");
                    }
                }
                tokio::select! {
                    account = auth::oauth::wait_for_oauth_login(receiver) => account?,
                    signal = tokio::signal::ctrl_c() => {
                        signal?;
                        cancelled.store(true, Ordering::Relaxed);
                        anyhow::bail!("Login cancelled");
                    }
                }
            } else {
                import(&storage, &source, name)?
            };
            let name = account.name.clone();
            let id = account.id.clone();
            accounts::add(&storage, account)?;
            println!("Added {name} ({id})");
        }
        Command::Remove { account } => {
            println!("Removed {}", accounts::remove(&storage, &account)?)
        }
        Command::Edit {
            account,
            name,
            source,
        } => {
            anyhow::ensure!(
                name.is_some() || source.file.is_some() || source.api_key_stdin,
                "Provide --name, --file, or --api-key-stdin to edit an account"
            );
            let replacement = if source.file.is_some() || source.api_key_stdin {
                Some(import(&storage, &source, String::new())?)
            } else {
                None
            };
            let name = name.map(|name| name.trim().to_owned());
            println!(
                "Updated {}",
                accounts::edit(&storage, &account, name, replacement)?
            );
        }
        Command::Switch { account } => {
            let selector = account.join(" ");
            println!(
                "Switched to {}",
                accounts::switch(&storage, &selector).await?
            )
        }
        Command::Status {
            account,
            json,
            base_url,
        } => {
            let report = usage::query(&storage, account.as_deref(), &base_url).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", theme.usage(&report));
            }
            anyhow::ensure!(
                !report.iter().any(|row| row.status == "error"),
                "Some usage queries failed; see per-account errors above"
            );
        }
        Command::List { json } => {
            let store = accounts::load_current(&storage)?;
            let now = chrono::Utc::now().timestamp();
            let rows: Vec<_> = store
                .accounts
                .iter()
                .map(|account| {
                    status::AccountStatus::from_stored(
                        account,
                        store.active_account_id.as_deref(),
                        now,
                    )
                })
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                print!("{}", theme.accounts(&rows));
            }
        }
        Command::Ps => {
            let running = processes::list_running()?;
            print!("{}", theme.processes(&running));
        }
        Command::Stop { yes } => {
            let running = processes::list_running()?;
            print!("{}", theme.processes(&running));
            if running.is_empty() {
                return Ok(());
            }
            if !yes {
                anyhow::ensure!(
                    io::stdin().is_terminal(),
                    "confirmation requires a terminal; review with `codex-switcher ps` and use `stop --yes` to proceed"
                );
                io::stdout().flush()?;
                eprint!("Close all listed processes? Unsaved work may be lost. [y/N] ");
                io::stderr().flush()?;
                let mut answer = String::new();
                io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                    println!("Cancelled.");
                    return Ok(());
                }
            }
            let stopped = processes::stop(&running)?;
            println!("Closed {} process(es).", stopped.len());
        }
        Command::Daemon { action } => match action {
            DaemonAction::Run(args) => daemon::run(&storage, args.settings()?).await?,
            #[cfg(target_os = "linux")]
            DaemonAction::Install(args) => {
                args.settings()?;
                let exe = std::env::current_exe()?;
                let mut command = vec![exe.to_string_lossy().into_owned()];
                let (store_dir, codex_home) = explicit_dirs;
                // Pin both directories so the service sees the same files as this shell.
                command.extend([
                    "--store-dir".into(),
                    std::path::absolute(store_dir.unwrap_or(storage.directory.clone()))?
                        .to_string_lossy()
                        .into_owned(),
                    "--codex-home".into(),
                    std::path::absolute(codex_home.unwrap_or(storage.codex_home.clone()))?
                        .to_string_lossy()
                        .into_owned(),
                    "daemon".into(),
                    "run".into(),
                ]);
                command.extend(args.to_args());
                if codex_switcher::config::ensure_daemon_auto_start(&storage.codex_home)? {
                    println!("Set [features] daemon_auto_start = true");
                }
                let path = daemon::systemd::install(&command)?;
                println!("Installed and started {}", path.display());
                println!("Logs: journalctl --user -u {} -f", daemon::systemd::UNIT);
            }
            #[cfg(target_os = "linux")]
            DaemonAction::Uninstall => {
                println!("Removed {}", daemon::systemd::uninstall()?.display());
            }
            #[cfg(target_os = "linux")]
            DaemonAction::Start | DaemonAction::Stop | DaemonAction::Restart => {
                let (action, done) = match action {
                    DaemonAction::Start => ("start", "Started"),
                    DaemonAction::Stop => ("stop", "Stopped"),
                    _ => ("restart", "Restarted"),
                };
                daemon::systemd::control(action)?;
                println!("{done} {}", daemon::systemd::UNIT);
            }
            #[cfg(not(target_os = "linux"))]
            DaemonAction::Install(_)
            | DaemonAction::Uninstall
            | DaemonAction::Start
            | DaemonAction::Stop
            | DaemonAction::Restart => {
                anyhow::bail!("Service installation is only supported on Linux; run `daemon run` under your own supervisor")
            }
            DaemonAction::Status { json } => {
                let state = daemon::read_state(&storage)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&state)?);
                } else {
                    print!("{}", theme.daemon(state.as_ref()));
                }
            }
        },
    }
    Ok(())
}
