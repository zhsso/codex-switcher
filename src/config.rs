//! Minimal edits to Codex's config.toml that preserve the user's formatting.
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use toml_edit::{value, DocumentMut, Item, Table};

/// Ensure `[features] daemon_auto_start = true`, so clients reconnect to the
/// managed app-server daemon after a restart. Returns whether the file changed.
pub fn ensure_daemon_auto_start(codex_home: &Path) -> Result<bool> {
    let path = codex_home.join("config.toml");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path.display()))
        }
    };
    let mut document: DocumentMut = contents
        .parse()
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let features = document
        .entry("features")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .with_context(|| format!("`features` in {} is not a table", path.display()))?;
    if features
        .get("daemon_auto_start")
        .and_then(Item::as_bool)
        .unwrap_or(false)
    {
        return Ok(false);
    }
    features.insert("daemon_auto_start", value(true));
    fs::create_dir_all(codex_home)?;
    let parent = path.parent().context("File has no parent directory")?;
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    if let Ok(metadata) = fs::metadata(&path) {
        temporary
            .as_file()
            .set_permissions(metadata.permissions())?;
    }
    fs::write(temporary.path(), document.to_string())?;
    temporary
        .persist(&path)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(initial: Option<&str>) -> (bool, String) {
        let dir = tempfile::tempdir().unwrap();
        if let Some(initial) = initial {
            fs::write(dir.path().join("config.toml"), initial).unwrap();
        }
        let changed = ensure_daemon_auto_start(dir.path()).unwrap();
        (
            changed,
            fs::read_to_string(dir.path().join("config.toml")).unwrap(),
        )
    }

    #[test]
    fn flips_false_and_preserves_comments() {
        let (changed, text) = run(Some(
            "# keep me\nmodel = \"o3\"\n\n[features]\ndaemon_auto_start = false\nother = 1\n",
        ));
        assert!(changed);
        assert_eq!(
            text,
            "# keep me\nmodel = \"o3\"\n\n[features]\ndaemon_auto_start = true\nother = 1\n"
        );
    }

    #[test]
    fn adds_missing_table_or_key_and_leaves_true_alone() {
        let (changed, text) = run(None);
        assert!(changed);
        assert!(text.contains("[features]\ndaemon_auto_start = true"));
        let (changed, text) = run(Some("[features]\nx = 1\n"));
        assert!(changed);
        assert!(text.contains("daemon_auto_start = true"));
        let original = "[features]\ndaemon_auto_start = true # on\n";
        assert_eq!(run(Some(original)), (false, original.to_owned()));
    }

    #[test]
    fn invalid_toml_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[features\n").unwrap();
        assert!(ensure_daemon_auto_start(dir.path()).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "[features\n");
    }
}
