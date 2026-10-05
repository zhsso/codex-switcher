use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use codex_switcher::{
    accounts, app_server, auth, processes, status, storage::Storage, types::StoredAccount, usage,
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
    /// Codex executable used to control the app-server daemon
    #[arg(long, global = true, default_value = "codex")]
    codex_bin: String,
    /// Skip restarting app-server processes after a manual switch
    #[arg(long, global = true)]
    no_restart: bool,
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

/// Make running Codex clients pick up the account now in auth.json.
fn restart_app_servers(storage: &Storage, codex_bin: &str) -> Result<()> {
    let refresh = app_server::restart(codex_bin, &storage.codex_home).context(
        "Restarting the app-server failed; running Codex sessions keep the previous account until it restarts",
    )?;
    if refresh.config_changed {
        println!("Set [features] daemon_auto_start = true");
    }
    if !refresh.stopped.is_empty() {
        println!(
            "Stopped {} standalone app-server process(es)",
            refresh.stopped.len()
        );
    }
    println!("Restarted app-server");
    Ok(())
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
            let (name, _) = accounts::edit(&storage, &account, name, replacement)?;
            println!("Updated {name}");
        }
        Command::Switch { account } => {
            let selector = account.join(" ");
            println!(
                "Switched to {}",
                accounts::switch(&storage, &selector).await?
            );
            if !cli.no_restart {
                restart_app_servers(&storage, &cli.codex_bin)?;
            }
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
            if running.iter().any(|process| !process.kind.is_closable()) {
                println!("The app-server updater is left running.");
            }
            if !running.iter().any(|process| process.kind.is_closable()) {
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
            let mut closed = 0;
            if running
                .iter()
                .any(|process| process.managed && process.kind.is_closable())
            {
                // Killing the managed daemon directly leaves clients unable to reconnect.
                app_server::daemon_command(&cli.codex_bin, &storage.codex_home, "stop")?;
                println!("Stopped the managed app-server daemon.");
                closed += running
                    .iter()
                    .filter(|process| process.managed && process.kind.is_closable())
                    .count();
            }
            closed += processes::stop(&running)?.len();
            println!("Closed {closed} process(es).");
        }
    }
    Ok(())
}
