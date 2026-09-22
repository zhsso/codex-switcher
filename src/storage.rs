use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::types::{AccountsStore, AuthData, AuthDotJson, StoredAccount, TokenData};

/// Explicit paths keep tests and alternate profiles separate from real credentials.
pub struct Storage {
    pub directory: PathBuf,
    pub codex_home: PathBuf,
}

impl Storage {
    pub fn new(directory: Option<PathBuf>, codex_home: Option<PathBuf>) -> Result<Self> {
        let home = || dirs::home_dir().context("Could not find home directory");
        Ok(Self {
            directory: match directory {
                Some(path) => path,
                None => home()?.join(".codex-switcher"),
            },
            codex_home: match codex_home
                .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
            {
                Some(path) => path,
                None => home()?.join(".codex"),
            },
        })
    }

    /// The OS releases this lock even if the CLI is interrupted or crashes.
    pub fn lock(&self) -> Result<File> {
        fs::create_dir_all(&self.directory)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.directory.join("accounts.lock"))?;
        file.try_lock()
            .context("Another codex-switcher command is using this account store")?;
        Ok(file)
    }

    pub fn load(&self) -> Result<AccountsStore> {
        let path = self.directory.join("accounts.json");
        let contents = match fs::read(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AccountsStore::default())
            }
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to read {}", path.display()))
            }
        };
        let store: AccountsStore = serde_json::from_slice(&contents)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        anyhow::ensure!(
            store.version == 1,
            "Unsupported account store version: {}",
            store.version
        );
        Ok(store)
    }

    pub fn save(&self, store: &AccountsStore) -> Result<()> {
        write_private_json(&self.directory.join("accounts.json"), store)
    }

    pub fn auth_path(&self) -> PathBuf {
        self.codex_home.join("auth.json")
    }

    pub fn read_auth(&self) -> Result<Option<AuthDotJson>> {
        let path = self.auth_path();
        match fs::read(&path) {
            Ok(contents) => serde_json::from_slice(&contents)
                .with_context(|| format!("Failed to parse {}", path.display()))
                .map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
        }
    }

    pub fn write_auth(&self, account: &StoredAccount) -> Result<()> {
        let auth = match &account.auth_data {
            AuthData::ApiKey { key } => AuthDotJson {
                openai_api_key: Some(key.clone()),
                tokens: None,
                last_refresh: None,
            },
            AuthData::ChatGPT {
                id_token,
                access_token,
                refresh_token,
                account_id,
            } => AuthDotJson {
                openai_api_key: None,
                tokens: Some(TokenData {
                    id_token: id_token.clone(),
                    access_token: access_token.clone(),
                    refresh_token: refresh_token.clone(),
                    account_id: account_id.clone(),
                }),
                last_refresh: Some(chrono::Utc::now()),
            },
        };
        write_private_json(&self.auth_path(), &auth)
    }
}

/// Write credentials atomically, with owner-only permissions from creation on Unix.
fn write_private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("File has no parent directory")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    serde_json::to_writer_pretty(&mut temporary, value)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}
