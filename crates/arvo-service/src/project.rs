//! The project folder: where a person's scripts live (#121).
//!
//! One folder, chosen by the person so it can be a git repository, shown as
//! the Files view. Every command here is confined to it: a path from the
//! window is relative to the folder, checked segment by segment, and the
//! resolved location must still be inside the folder once links are followed.
//! That is what keeps the keychain, the evidence store and the rest of the
//! disk out of reach of anything the file tree does.
//!
//! Called the *project* folder because "workspace" already means a saved panel
//! arrangement in Settings.
//!
//! Deleting moves to the recycle bin rather than removing, because a tree view
//! is exactly where a mis-click happens and a script is exactly what someone
//! cannot get back.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use arvo_api::{FileContentView, FileEntryView, GitStatusView, ProjectFolderView, SearchHitView};

/// When `path` last changed, in milliseconds since the epoch, or 0 when the
/// file system cannot say. What a save compares against.
#[must_use]
pub fn modified(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

/// Where the chosen folder is remembered, in the app data directory. Not in
/// `session.json`, which is the window's layout; the folder is the engine's to
/// know (ADR-0018).
pub const FILE: &str = "project.json";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Stored {
    pub folder: Option<PathBuf>,
}

/// What a project's `.gitignore` must carry so a push never includes what
/// the person holds: statements, account snapshots, the agent's call log.
/// Market data is ignored for size, not privacy; delete those two lines to
/// track it.
const GITIGNORE: &str = "\
# Arvo — keep personal data out of git
portfolios/
snapshots/
exports/
agent-audit.jsonl
# Arvo — market data, ignored for size; remove to track it
data/
option-quotes/
";

/// The window's own app data directory, `%APPDATA%/com.arvo.desktop`: the
/// same folder Tauri's `app_data_dir` resolves to, for a process without an
/// app handle.
///
/// # Errors
///
/// When `APPDATA` is not set.
pub fn app_data_root() -> Result<PathBuf, String> {
    std::env::var_os("APPDATA")
        .map(|appdata| PathBuf::from(appdata).join("com.arvo.desktop"))
        .ok_or_else(|| "APPDATA is not set".to_owned())
}

/// The project folder that was chosen, from the file alone, so the headless
/// engine and the MCP server find the same data as the window.
#[must_use]
pub fn remembered() -> Option<PathBuf> {
    let stored: Stored = std::fs::read_to_string(app_data_root().ok()?.join(FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    stored.folder.filter(|folder| folder.is_dir())
}

/// Makes sure `root/.gitignore` carries the Arvo block: written whole when
/// there is none, appended when a repository already has its own.
///
/// # Errors
///
/// When the file cannot be read or written.
pub fn ensure_gitignore(root: &Path) -> std::io::Result<()> {
    let path = root.join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    if existing.contains("# Arvo") {
        return Ok(());
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(GITIGNORE);
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitignore_is_written_once_and_appended_to_a_repos_own() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".gitignore");

        ensure_gitignore(dir.path()).expect("writes");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), GITIGNORE);

        ensure_gitignore(dir.path()).expect("no-op");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), GITIGNORE, "not appended twice");

        std::fs::write(&path, "target/").expect("write");
        ensure_gitignore(dir.path()).expect("appends");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), format!("target/\n{GITIGNORE}"));
    }
}
