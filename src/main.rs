use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use codex_switcher::{accounts, auth, status, storage::Storage, types::StoredAccount};

#[derive(Parser)]
#[command(
    version,
    about = "Manage and switch Codex accounts",
    after_help = "Account selectors accept an exact name or full ID. Use `list` to see saved accounts."
)]
struct Cli {
    /// Account storage directory (default: ~/.codex-switcher)
    #[arg(long, global = true)]
    store_dir: Option<PathBuf>,
    /// Codex directory (default: CODEX_HOME or ~/.codex)
    #[arg(long, global = true)]
    codex_home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
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
    Switch { account: String },
    /// Show local login status or details for a saved account (no network requests)
    Status {
        account: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show saved accounts without credentials or network requests
    List {
        #[arg(long)]
        json: bool,
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
        Command::Switch { account } => println!(
            "Switched to {}",
            accounts::switch(&storage, &account).await?
        ),
        Command::Status { account, json } => {
            let snapshot = status::snapshot(&storage, account.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot)?);
            } else {
                println!("Login:       {}", snapshot.login_status);
                println!("Saved accounts: {}", snapshot.saved_accounts);
                println!("Accounts file: {}", snapshot.accounts_file.display());
                println!("Auth file:     {}", snapshot.auth_file.display());
                println!(
                    "Live auth last refresh: {}",
                    status::format_time(snapshot.last_refresh)
                );
                if let Some(account) = &snapshot.account {
                    status::print_account(account);
                } else {
                    println!(
                        "No local login. Add an account and run `codex-switcher switch NAME`."
                    );
                }
                println!("Local snapshot only; API keys and server sessions are not verified. Plan information may be stale.");
            }
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
            } else if rows.is_empty() {
                println!("No saved accounts. Run `codex-switcher add NAME` or `codex-switcher add NAME --login`.");
            } else {
                println!("  ID                                    AUTH           CREDENTIALS       NAME / EMAIL / PLAN (local)");
                for account in &rows {
                    let marker = if account.is_active { '*' } else { ' ' };
                    println!(
                        "{marker} {}  {:<14} {:<17} {:?} / {:?} / {:?}",
                        account.id.as_deref().unwrap_or("-"),
                        account.auth_label(),
                        account.credential_status,
                        account.name.as_deref().unwrap_or("-"),
                        account.email.as_deref().unwrap_or("-"),
                        account.plan_type.as_deref().unwrap_or("unknown")
                    );
                }
                println!("* Current account. Credential status is based on local files, not server verification.");
            }
        }
    }
    Ok(())
}
