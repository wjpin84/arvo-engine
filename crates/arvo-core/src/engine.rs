//! How a front end finds the engine: `engine.json` in the app data directory.
//!
//! The file holds the address, the token and the engine's process id. It sits
//! in the user's own app data directory, so reading it is what being a local
//! front end of this user's engine means (ADR-0018). It is not protection from
//! another program running as the same user, which could read it — and the
//! keychain — alike.
//!
//! Here rather than in `arvo-engine` because the window reads it too, to know
//! whether to start an engine, and the engine depends on the window's crate
//! for its research entry points. One definition of the file for both.

use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

/// The file, in the app data directory.
pub const FILE: &str = "engine.json";

/// Where a running engine is, and what it will accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub address: SocketAddr,
    pub token: String,
    pub pid: u32,
}

/// Writes the file whole or not at all, through a temporary file and a rename,
/// so a front end never reads half a token.
///
/// # Errors
///
/// When the directory or the file cannot be written.
pub fn write(root: &Path, found: &Discovery) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    let path = root.join(FILE);
    let partial = root.join(format!("{FILE}.partial"));
    let body = json!({
        "address": found.address.to_string(),
        "token": found.token,
        "pid": found.pid,
    });
    std::fs::write(&partial, body.to_string())?;
    std::fs::rename(&partial, &path)
}

/// The file's contents, or `None` when it is absent or unreadable.
#[must_use]
pub fn read(root: &Path) -> Option<Discovery> {
    let text = std::fs::read_to_string(root.join(FILE)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    Some(Discovery {
        address: value.get("address")?.as_str()?.parse().ok()?,
        token: value.get("token")?.as_str()?.to_owned(),
        pid: u32::try_from(value.get("pid")?.as_u64()?).ok()?,
    })
}

/// Removes the file if it still describes the engine with `pid`, so a newer
/// engine's file is never taken away by an older one shutting down.
pub fn remove_if_ours(root: &Path, pid: u32) {
    if read(root).is_some_and(|found| found.pid == pid) {
        let _ = std::fs::remove_file(root.join(FILE));
    }
}

/// The engine already answering at the recorded address, if one is.
///
/// A file left by an engine that crashed points at nothing, and is treated as
/// no engine. Blocks for at most half a second.
#[must_use]
pub fn running(root: &Path) -> Option<Discovery> {
    let found = read(root)?;
    TcpStream::connect_timeout(&found.address, Duration::from_millis(500))
        .is_ok()
        .then_some(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_file_reads_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let found = Discovery {
            address: "127.0.0.1:50999".parse().expect("address"),
            token: "t".repeat(64),
            pid: 42,
        };
        write(dir.path(), &found).expect("written");
        assert_eq!(read(dir.path()), Some(found));
    }

    #[test]
    fn only_the_engine_that_wrote_the_file_removes_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let found = Discovery { address: "127.0.0.1:1".parse().expect("address"), token: "t".into(), pid: 7 };
        write(dir.path(), &found).expect("written");
        remove_if_ours(dir.path(), 8);
        assert!(read(dir.path()).is_some(), "another engine's file stays");
        remove_if_ours(dir.path(), 7);
        assert!(read(dir.path()).is_none());
    }

    #[test]
    fn a_file_pointing_at_nothing_is_no_engine_and_a_listener_is_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(running(dir.path()).is_none(), "no file");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let dead = listener.local_addr().expect("address");
        drop(listener);
        write(dir.path(), &Discovery { address: dead, token: "t".into(), pid: 1 }).expect("written");
        assert!(running(dir.path()).is_none(), "a crashed engine's file");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        write(dir.path(), &Discovery { address, token: "t".into(), pid: 1 }).expect("written");
        assert!(running(dir.path()).is_some());
    }
}
