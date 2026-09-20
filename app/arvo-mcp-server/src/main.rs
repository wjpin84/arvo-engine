//! Arvo as an MCP server, over stdio (#24).
//!
//!     arvo-mcp-server [<app-data-dir>] [--agent NAME]
//!
//! An agent can read research memory and run studies against the same engine
//! the window uses. It cannot fetch, share, or reach a broker: no tool here
//! names a credential, a source or an order, and the token it holds reaches
//! only the engine's research service. See [`server`] for what is offered
//! and why.
//!
//! This is a client of the engine (#151, ADR-0018). It finds the engine
//! through `engine.json` in `<app-data-dir>`, which defaults to
//! `%APPDATA%/com.arvo.desktop`; when none is running it starts one, from
//! `ARVO_ENGINE` or from `arvo-engine` beside this executable, and that
//! engine keeps running after this server exits, as one engine per user
//! means.
//!
//! `--agent` names who is running, for attribution and deflation; without it
//! the client's own name from `initialize` is used.
//!
//! **Stdout is the protocol.** Nothing else may write to it — a stray line is a
//! malformed message to the client. Diagnostics go to stderr.

mod server;

use std::io::Write;
use std::path::PathBuf;

fn main() {
    if let Err(err) = run() {
        eprintln!("arvo-mcp-server: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let agent = match args.iter().position(|arg| arg == "--agent") {
        Some(at) => {
            let name = args.get(at + 1).cloned().ok_or("--agent needs a name")?;
            args.drain(at..=at + 1);
            Some(name)
        }
        None => None,
    };
    let (root, explicit) = match args.first() {
        Some(dir) => (PathBuf::from(dir), true),
        None => (arvo_client::discovery::default_root()?, false),
    };

    let mut server = server::Server::connect(&root, explicit, agent)?;
    for line in std::io::stdin().lines() {
        let line = line.map_err(|err| format!("reading stdin: {err}"))?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.handle_line(&line) {
            // Locked per reply, never across a call.
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{reply}").map_err(|err| format!("writing stdout: {err}"))?;
            stdout.flush().map_err(|err| format!("flushing stdout: {err}"))?;
        }
    }
    Ok(())
}
