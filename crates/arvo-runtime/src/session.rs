//! The workspace as you left it.
//!
//! Everything the window remembers between launches: the panel layout, which
//! sidebar was open, the theme, the strategy that was selected. Nothing about
//! *research* is in here — findings live in the evidence store, which is a
//! record and has to survive being wrong about the layout.
//!
//! # Why a file rather than the webview's storage
//!
//! `localStorage` would work and would be invisible. This app already owns a
//! data directory holding the bars, the evidence and the portfolios; a
//! workspace that hid in a browser profile would be the one piece of state
//! nobody could find, back up, or delete when it went wrong. A layout that
//! restores badly is a thing people need to be able to throw away.
//!
//! # What is deliberately not remembered
//!
//! The *contents* of a result tab. A study tab holds a backtest that lives in
//! memory, and re-running it on launch would spend a minute of work nobody
//! asked for. The tab comes back with its name and says it is not loaded —
//! which is honest, and better than either a blank panel or a silent re-run.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file, inside the app's data directory.
const FILE: &str = "session.json";

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("could not write the workspace to {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not encode the workspace: {0}")]
    Encode(#[from] serde_json::Error),
}

/// What the window puts back on launch.
///
/// Every field is optional or has a default, because this is a persisted
/// format that will gain fields: a session written by an older build must
/// still open, as a session that knows less rather than as a failure.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// dockview's own serialisation. Opaque here on purpose — this crate has
    /// no business knowing how a layout is encoded, and treating it as data
    /// means a dockview upgrade cannot break the Rust side.
    pub layout: Option<serde_json::Value>,
    /// Which activity-bar view was showing, or `None` for a collapsed sidebar.
    pub active_view: Option<String>,
    pub output_visible: bool,
    pub theme: Option<String>,
    /// The strategy the research sidebar had selected.
    pub strategy: Option<String>,
}

/// Reads the stored workspace.
///
/// A missing file is an empty session, not an error — that is the ordinary
/// state on first launch. So is a *corrupt* one: a workspace is a convenience,
/// and refusing to start the app because the layout file is malformed would
/// trade a small annoyance for a large one. It is logged and discarded.
#[must_use]
pub fn load(dir: &Path) -> Session {
    let path = dir.join(FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Session::default();
    };
    serde_json::from_str(&text).unwrap_or_else(|err| {
        tracing::warn!(
            error = %err,
            path = %path.display(),
            "the stored workspace could not be read; starting with a fresh layout"
        );
        Session::default()
    })
}

/// Writes the workspace.
///
/// Through a temporary file and a rename, because this is written on every
/// layout change — including while dragging a panel — and a process that dies
/// mid-write would otherwise leave a half-file that the next launch discards.
/// Losing a workspace to a crash is exactly the moment someone is least able
/// to afford a second annoyance.
///
/// # Errors
///
/// Returns [`SessionError`] if the directory cannot be written or the session
/// cannot be encoded.
pub fn save(dir: &Path, session: &Session) -> Result<(), SessionError> {
    std::fs::create_dir_all(dir).map_err(|source| SessionError::Write {
        path: dir.to_path_buf(),
        source,
    })?;

    let text = serde_json::to_string_pretty(session)?;
    let path = dir.join(FILE);
    let staging = dir.join(format!("{FILE}.tmp"));

    std::fs::write(&staging, text).map_err(|source| SessionError::Write {
        path: staging.clone(),
        source,
    })?;
    std::fs::rename(&staging, &path).map_err(|source| SessionError::Write {
        path: path.clone(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_an_empty_workspace_not_a_failure() {
        // First launch. The app has to start.
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(load(dir.path()), Session::default());
    }

    #[test]
    fn a_corrupt_file_is_discarded_rather_than_refusing_to_start() {
        // A workspace is a convenience. Refusing to open the application
        // because the layout file is malformed trades a small annoyance for a
        // large one.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(FILE), "{ not json").expect("write");
        assert_eq!(load(dir.path()), Session::default());
    }

    #[test]
    fn a_workspace_survives_a_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = Session {
            layout: Some(serde_json::json!({ "grid": { "root": "branch" } })),
            active_view: Some("research".to_owned()),
            output_visible: true,
            theme: Some("catppuccin-mocha".to_owned()),
            strategy: Some("opening_range".to_owned()),
        };

        save(dir.path(), &session).expect("saves");
        assert_eq!(load(dir.path()), session);
    }

    #[test]
    fn a_session_written_by_an_older_build_still_opens() {
        // The format will gain fields. One that knows less has to load as a
        // session that knows less, not as a failure that costs the layout.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(FILE), r#"{"theme":"dark"}"#).expect("write");

        let session = load(dir.path());
        assert_eq!(session.theme.as_deref(), Some("dark"));
        assert_eq!(session.layout, None);
        assert!(!session.output_visible);
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        // The rename is the point: a half-written file is a lost workspace,
        // and this is written on every layout change including mid-drag.
        let dir = tempfile::tempdir().expect("tempdir");
        save(dir.path(), &Session::default()).expect("saves");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readable")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn the_layout_is_carried_without_being_understood() {
        // dockview owns its own encoding. Treating it as opaque data is what
        // stops a dockview upgrade from breaking the Rust side.
        let dir = tempfile::tempdir().expect("tempdir");
        let odd = serde_json::json!({
            "activeGroup": "1",
            "grid": { "height": 900, "width": 1600, "orientation": "HORIZONTAL" },
            "panels": { "study:MSFT.NASDAQ": { "id": "study:MSFT.NASDAQ" } },
        });
        save(
            dir.path(),
            &Session {
                layout: Some(odd.clone()),
                ..Session::default()
            },
        )
        .expect("saves");
        assert_eq!(load(dir.path()).layout, Some(odd));
    }
}
