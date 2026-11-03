use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use codex_switcher::{
    accounts, app_server, auth, processes, status, storage::Storage, types::StoredAccount, usage,
    warmup, workspace,
};

#[derive(Parser)]
#[command(
    version,
    about = "Manage and switch Codex accounts",
    after_help = "Account selectors accept an exact name or full ID. Use `list` (alias: `ls`) to see saved accounts.\n\nRun `codex-switcher <COMMAND> --help` or `codex-switcher help <COMMAND>` for details and examples."
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
    /// Workspace to operate on instead of the current Codex directory
    #[arg(short, long, global = true, conflicts_with = "codex_home")]
    workspace: Option<String>,
    /// Codex executable used to control the app-server daemon
    #[arg(long, global = true, default_value = "codex")]
    codex_bin: String,
    /// Skip restarting app-server processes after a manual switch or relogin
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
    #[command(
        after_help = r#"With no source option, imports auth.json from --codex-home, CODEX_HOME, or ~/.codex.
Choose one source: --file, --api-key-stdin, or --login. --no-browser requires --login.
Omit NAME to generate a name from the email or account identity; names must be unique.
Adding an account does not switch to it. Run `codex-switcher switch NAME` to activate it.
Browser login waits up to 5 minutes; press Ctrl+C to cancel.

Examples:
  codex-switcher add personal
  codex-switcher add work --login
  codex-switcher add --login
  codex-switcher add work --login --no-browser
  codex-switcher add work --file /path/to/auth.json
  codex-switcher add api --api-key-stdin < /path/to/api-key.txt
  codex-switcher add "So Zhang" --login"#
    )]
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
    #[command(
        after_help = r#"Accepts an exact name or full ID from `codex-switcher list`.
Only removes the saved profile; Codex stays logged in and no other account is selected.
Quote names containing spaces.

Examples:
  codex-switcher remove work
  codex-switcher remove "So Zhang""#
    )]
    Remove {
        /// Exact account name or full ID
        account: String,
    },
    /// Rename an account and/or replace its credentials
    #[command(
        after_help = r#"Provide --name, --file, or --api-key-stdin; renaming and replacing credentials can be combined.
Keeps the account ID, creation time, and usage history. Replacing the current account's
credentials also updates auth.json. Does not restart app-server.
Use `codex-switcher relogin ACCOUNT` to replace credentials through browser login.
Quote names containing spaces.

Examples:
  codex-switcher edit work --name company
  codex-switcher edit company --file /path/to/new-auth.json
  codex-switcher edit work --name company --file /path/to/new-auth.json
  codex-switcher edit api --api-key-stdin < /path/to/api-key.txt
  codex-switcher edit "So Zhang" --name personal"#
    )]
    Edit {
        /// Exact account name or full ID
        account: String,
        /// New display name; must be nonblank and unique
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        source: Source,
    },
    /// Sign in again through the browser and replace a saved account's credentials
    #[command(
        after_help = r#"Keeps the account name, ID, creation time, and usage history.
For the current account, also updates auth.json and restarts app-server; ongoing tasks
may be interrupted. Use --no-restart to skip the restart.
Login waits up to 5 minutes; press Ctrl+C to cancel. Names may contain multiple words.

Examples:
  codex-switcher relogin work
  codex-switcher relogin work --no-browser
  codex-switcher relogin work --no-restart
  codex-switcher relogin So Zhang"#
    )]
    Relogin {
        /// Exact name or full ID; multiple shell words are joined with spaces
        #[arg(required = true, num_args = 1..)]
        account: Vec<String>,
        /// Print the login URL without opening a browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Switch to an account, or cycle to the next saved account when omitted
    #[command(
        after_help = r#"Accepts an exact name or full ID from `codex-switcher list`.
Without ACCOUNT, cycles in saved order and wraps from the last account to the first.
If no account is active, selects the first saved account.
Writes the selected credentials to auth.json and restarts app-server so connected
clients pick up the account. Ongoing tasks may be interrupted; --no-restart skips this restart.
Names may contain multiple words.

Examples:
  codex-switcher switch
  codex-switcher switch work
  codex-switcher switch So Zhang
  codex-switcher switch work --no-restart"#
    )]
    Switch {
        /// Exact name or full ID; omit to cycle; multiple words are joined with spaces
        #[arg(num_args = 0..)]
        account: Vec<String>,
    },
    /// Fetch usage and reset times for an account, or all saved accounts
    #[command(
        after_help = r#"Omit ACCOUNT to query all saved accounts. Accepts an exact name or full ID;
quote names containing spaces. API key accounts are reported as unsupported.
Shows live usage windows, remaining percentages, and reset times without switching accounts.
JSON output is always an array, including for a single account. Failed queries are
reported per account; other accounts are still queried and the command exits with code 1.

Examples:
  codex-switcher status
  codex-switcher status work
  codex-switcher status "So Zhang" --json
  codex-switcher status --json"#
    )]
    Status {
        /// Exact account name or full ID; omit to query all accounts
        account: Option<String>,
        /// Output a JSON array instead of formatted text
        #[arg(long)]
        json: bool,
        /// Backend base URL, not a usage endpoint; receives the account token and ID
        #[arg(long, default_value = usage::DEFAULT_BASE_URL)]
        base_url: String,
    },
    /// Send hi with gpt-6-luna when the account's 5h usage is zero
    #[command(
        after_help = r#"Omit ACCOUNT to check all saved accounts. Accepts an exact name or full ID;
quote names containing spaces. Queries live usage, then sends hi with gpt-6-luna only
when the 5-hour window reports exactly 0% used. This produces real model usage.
Skips API key accounts, accounts with existing usage, and accounts without a 5-hour window.
Does not switch accounts or restart app-server. Results are warmed, skipped, or error;
errors do not stop other accounts from being checked, and result in exit code 1.

Examples:
  codex-switcher warmup
  codex-switcher warmup work
  codex-switcher warmup "So Zhang" --json
  codex-switcher warmup --json"#
    )]
    Warmup {
        /// Exact account name or full ID; omit to check all accounts
        account: Option<String>,
        /// Output a JSON array of per-account results instead of formatted text
        #[arg(long)]
        json: bool,
        /// Backend base URL, not a responses endpoint; receives the account token and ID
        #[arg(long, default_value = usage::DEFAULT_BASE_URL)]
        base_url: String,
    },
    /// Show saved accounts without credentials or network requests
    #[command(
        alias = "ls",
        after_help = r#"Lists local account names, full IDs, and credential status. * marks the account
matching the current Codex auth.json. Does not query live usage; use `status` for that.
Credentials are never included in text or JSON output. Alias: ls.

Examples:
  codex-switcher list
  codex-switcher ls
  codex-switcher list --json"#
    )]
    List {
        /// Output a JSON array instead of formatted text
        #[arg(long)]
        json: bool,
    },
    /// Show running Codex CLI, Codex desktop, app-server, and ChatGPT processes
    #[command(
        after_help = r#"Displays detected processes for review before running `codex-switcher stop`.
Listing processes does not close them.

Examples:
  codex-switcher ps"#
    )]
    Ps,
    /// Gracefully close the listed Codex, app-server, and ChatGPT processes
    #[command(
        after_help = r#"Lists detected processes and asks for confirmation before requesting a graceful shutdown.
Use --yes to skip confirmation, including when standard input is not a terminal.
Stops managed app-server daemons through Codex; leaves the app-server updater running.
Unsaved work may be lost. Processes that do not exit are not forcibly killed.

Examples:
  codex-switcher ps
  codex-switcher stop
  codex-switcher stop --yes"#
    )]
    Stop {
        /// Close without prompting for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Manage workspaces, so several accounts can run at the same time
    #[command(
        after_help = r#"A workspace is a Codex directory with its own login, sessions, state, and
app-server daemon. ~/.codex is the workspace named default. New workspaces copy
config.toml from the current Codex directory and share its skills and plugins.
Add -w NAME to any command to run it against a workspace. An OAuth account can
only be used by one workspace at a time. Linux only.

Examples:
  codex-switcher workspace create work
  codex-switcher switch so -w work
  codex-switcher workspace run work
  codex-switcher workspace run work codex resume --last
  codex-switcher workspace shell work
  codex-switcher -w work status
  codex-switcher workspace list
  codex-switcher workspace disable work
  codex-switcher workspace enable work
  codex-switcher workspace remove work"#
    )]
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    /// Create a workspace from the current Codex directory
    Create {
        /// Workspace name: letters, digits, '.', '_' or '-'
        name: String,
        /// Also switch it to this saved account (exact name or full ID)
        #[arg(long)]
        account: Option<String>,
    },
    /// List workspaces and the account each one is using
    #[command(visible_alias = "ls")]
    List {
        /// Print machine-readable JSON instead of formatted text
        #[arg(long)]
        json: bool,
    },
    /// Stop a workspace's daemon and release its account for other workspaces
    #[command(
        after_help = r#"Tokens are saved and the workspace's auth.json is removed, so the account can
be used elsewhere. Sessions and settings stay. Refuses while clients are running.
`enable` restores the same account; `switch ACCOUNT -w NAME` picks any free one."#
    )]
    Disable {
        /// Workspace name
        name: String,
    },
    /// Log a disabled workspace back in to the account it released
    Enable {
        /// Workspace name
        name: String,
    },
    /// Run a command with CODEX_HOME set to a workspace (default: codex)
    Run {
        /// Workspace name
        name: String,
        /// Command and arguments to run instead of codex
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Start a shell with CODEX_HOME set to a workspace; `exit` leaves it
    Shell {
        /// Workspace name
        name: String,
    },
    /// Print a workspace's directory, for use as CODEX_HOME
    Path {
        /// Workspace name
        name: String,
    },
    /// Stop a workspace's app-server daemon and delete it, including its sessions
    Remove {
        /// Workspace name
        name: String,
        /// Delete without prompting for confirmation
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
    restart_home_app_servers(&storage.codex_home, codex_bin)
}

fn restart_home_app_servers(codex_home: &Path, codex_bin: &str) -> Result<()> {
    let refresh = app_server::restart(codex_bin, codex_home).context(
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

/// Run the browser OAuth flow until it completes, fails, or Ctrl-C cancels it.
async fn oauth_login(name: String, no_browser: bool) -> Result<StoredAccount> {
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
        account = auth::oauth::wait_for_oauth_login(receiver) => account,
        signal = tokio::signal::ctrl_c() => {
            signal?;
            cancelled.store(true, Ordering::Relaxed);
            anyhow::bail!("Login cancelled");
        }
    }
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

fn open_storage(
    store_dir: Option<PathBuf>,
    codex_home: Option<PathBuf>,
    workspace: Option<&str>,
) -> Result<Storage> {
    let storage = Storage::new(store_dir, codex_home)?;
    Ok(match workspace {
        Some(name) => {
            let home = workspace::resolve(&storage.directory, name)?;
            storage.with_home(home)
        }
        None => storage,
    })
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
    let storage = open_storage(cli.store_dir, cli.codex_home, cli.workspace.as_deref())?;
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
                oauth_login(name, no_browser).await?
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
        Command::Relogin {
            account,
            no_browser,
        } => {
            let selector = account.join(" ");
            // Fail on an unknown account before opening the browser.
            let store = accounts::load_current(&storage)?;
            let previous = &store.accounts[accounts::resolve(&store, &selector)?];
            let previous_email = previous.email.clone();
            let replacement = oauth_login(previous.name.clone(), no_browser).await?;
            if let (Some(old), Some(new)) = (&previous_email, &replacement.email) {
                if !old.eq_ignore_ascii_case(new) {
                    eprintln!("warning: signed in as {new}, previously {old}");
                }
            }
            let (name, live_homes) = accounts::edit(&storage, &selector, None, Some(replacement))?;
            println!("Signed in again as {name}");
            if !cli.no_restart {
                for home in &live_homes {
                    restart_home_app_servers(home, &cli.codex_bin)?;
                }
            }
        }
        Command::Switch { account } => {
            let selector = account.join(" ");
            let name = if account.is_empty() {
                accounts::switch_next(&storage).await?
            } else {
                accounts::switch(&storage, &selector).await?
            };
            println!("Switched to {name}");
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
        Command::Warmup {
            account,
            json,
            base_url,
        } => {
            let report = warmup::run(&storage, account.as_deref(), &base_url).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", theme.warmup(&report));
            }
            anyhow::ensure!(
                !report.iter().any(|row| row.status == "error"),
                "Some warmups failed; see per-account errors above"
            );
        }
        Command::List { json } => {
            let store = accounts::load_current(&storage)?;
            let now = chrono::Utc::now().timestamp();
            let logins = workspace::logins(&storage);
            let rows: Vec<_> = store
                .accounts
                .iter()
                .map(|account| status::AccountStatus {
                    workspaces: accounts::workspaces_using(account, &logins),
                    ..status::AccountStatus::from_stored(
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
            let running = processes::list_running(&storage.codex_home)?;
            print!("{}", theme.processes(&running));
        }
        Command::Stop { yes } => {
            let running = processes::list_running(&storage.codex_home)?;
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
            closed += processes::stop(&running, &storage.codex_home)?.len();
            println!("Closed {closed} process(es).");
        }
        Command::Workspace { command } => match command {
            WorkspaceCommand::Create { name, account } => {
                let (_, account) = workspace::create(&storage, &name, account.as_deref()).await?;
                match account {
                    Some(account) => println!("Created workspace {name} using {account}"),
                    None => {
                        println!("Created workspace {name}");
                        println!("Choose an account: codex-switcher switch ACCOUNT -w {name}");
                    }
                }
                println!("Run: codex-switcher workspace run {name}");
            }
            WorkspaceCommand::List { json } => {
                let workspaces = workspace::list(&storage)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&workspaces)?);
                } else {
                    for workspace in workspaces {
                        let account = if workspace.disabled {
                            match &workspace.disabled_account {
                                Some(account) => format!("disabled ({account})"),
                                None => "disabled".to_owned(),
                            }
                        } else {
                            workspace
                                .account
                                .clone()
                                .unwrap_or_else(|| "(no saved account)".to_owned())
                        };
                        println!(
                            "{} {}  {}  {}",
                            if workspace.current { "*" } else { " " },
                            workspace.name,
                            account,
                            workspace.path.display()
                        );
                    }
                }
            }
            WorkspaceCommand::Disable { name } => {
                let account = workspace::disable(&storage, &name, &cli.codex_bin)?;
                println!("Disabled workspace {name}; {account} is free to use elsewhere");
            }
            WorkspaceCommand::Enable { name } => {
                let (path, account) = workspace::enable(&storage, &name).await?;
                println!("Enabled workspace {name} using {account}");
                if !cli.no_restart {
                    restart_home_app_servers(&path, &cli.codex_bin)?;
                }
            }
            WorkspaceCommand::Run { name, command } => {
                let path = workspace::path(&storage, &name)?;
                match command.split_first() {
                    Some((program, args)) => workspace::exec(&path, program.as_ref(), args)?,
                    None => {
                        anyhow::ensure!(
                            !workspace::is_disabled(&path),
                            "Workspace '{name}' is disabled; run `codex-switcher workspace enable {name}` or `codex-switcher switch ACCOUNT -w {name}`"
                        );
                        let codex = app_server::resolve_codex_bin(&cli.codex_bin, &path)
                            .with_context(|| {
                                format!(
                                    "Could not find `{}` on PATH; pass --codex-bin with an absolute path",
                                    cli.codex_bin
                                )
                            })?;
                        workspace::exec(&path, codex.as_os_str(), &[])?
                    }
                }
            }
            WorkspaceCommand::Shell { name } => {
                let path = workspace::path(&storage, &name)?;
                let shell = std::env::var_os("SHELL")
                    .filter(|shell| !shell.is_empty())
                    .unwrap_or_else(|| "/bin/sh".into());
                eprintln!("Using workspace {name}; type `exit` to leave");
                workspace::exec(&path, &shell, &[])?
            }
            WorkspaceCommand::Path { name } => {
                println!("{}", workspace::path(&storage, &name)?.display());
            }
            WorkspaceCommand::Remove { name, yes } => {
                let path = workspace::path(&storage, &name)?;
                if !yes {
                    anyhow::ensure!(
                        io::stdin().is_terminal(),
                        "confirmation requires a terminal; use `workspace remove --yes` to proceed"
                    );
                    eprint!(
                        "Delete {} including its sessions and history? [y/N] ",
                        path.display()
                    );
                    io::stderr().flush()?;
                    let mut answer = String::new();
                    io::stdin().read_line(&mut answer)?;
                    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                        println!("Cancelled.");
                        return Ok(());
                    }
                }
                workspace::remove(&storage, &name, &cli.codex_bin)?;
                println!("Removed workspace {name}");
            }
        },
    }
    Ok(())
}
