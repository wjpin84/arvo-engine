//! The Arvo engine (ADR-0018).
//!
//!     arvo-engine [<app-data-dir>]
//!
//! Serves the research tier on a free loopback port and writes the address
//! and a fresh token to `engine.json` in the app data directory, which
//! defaults to the window's own. A second engine for the same directory finds
//! the first answering and leaves it running. Ctrl-C stops it and removes the
//! file.

use std::path::PathBuf;

use arvo_engine::{discovery, grpc, research};

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("arvo-engine: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let root = match std::env::args().nth(1) {
        Some(dir) => PathBuf::from(dir),
        None => research::default_root()?,
    };

    if let Some(found) = discovery::running(&root).await {
        eprintln!(
            "arvo-engine: already running at {} (pid {}); leaving it",
            found.address, found.pid
        );
        return Ok(());
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|err| format!("binding a loopback port: {err}"))?;
    let address = listener
        .local_addr()
        .map_err(|err| format!("reading the bound address: {err}"))?;
    let token = discovery::new_token();
    let pid = std::process::id();
    discovery::write(&root, &discovery::Discovery { address, token: token.clone(), pid })
        .map_err(|err| format!("writing {}: {err}", discovery::FILE))?;
    eprintln!("arvo-engine: serving research on {address} for {}", root.display());

    let served = grpc::serve(listener, research::Research::new(&root), &token, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await;
    discovery::remove_if_ours(&root, pid);
    served.map_err(|err| format!("serving: {err}"))
}
