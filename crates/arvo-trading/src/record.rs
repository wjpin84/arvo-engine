//! A session's own record: one JSON line per event, appended.
//!
//! Append-only and never rewritten, because this is what answers "what did the
//! system do" after the fact, and a file that is rewritten can be wrong about
//! its own past.

use std::path::{Path, PathBuf};

use crate::promotion::SUBDIR;
use crate::status::Event;

/// Appends a session's events to its file.
pub(crate) struct Recorder {
    pub(crate) path: PathBuf,
}

/// `<data>/sessions/<id>.jsonl`, with anything but a letter, digit, `-` or
/// `_` in the id made `_`.
#[must_use]
pub fn record_path(data: &Path, id: &str) -> PathBuf {
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    data.join(SUBDIR).join(format!("{safe}.jsonl"))
}

impl Recorder {
    pub(crate) fn open(data: &Path, id: &str) -> Result<Self, String> {
        let path = record_path(data, id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        }
        Ok(Self { path })
    }

    pub(crate) fn write(&self, event: &str, detail: Option<serde_json::Value>) {
        use std::io::Write as _;
        let line = Event {
            at: chrono::Utc::now().to_rfc3339(),
            event,
            detail,
        };
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            return;
        };
        if let Ok(text) = serde_json::to_string(&line) {
            let _ = writeln!(file, "{text}");
        }
    }
}
