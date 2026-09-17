//! One supervisor for what Arvo spawns (ADR-0023).
//!
//! A [`Launch`] says what to run; the supervisor starts it, learns where it
//! is listening, tells the registry, restarts it if it dies as often as its
//! policy allows, and stops it when told or when Arvo exits. What it runs
//! today is a provider an extension built (ADR-0025) or a `command` entry in
//! `plugins.toml`; the language server and script runs keep their own
//! spawning for now and are the next callers.
//!
//! # The handshake is one line
//!
//! A child is given `ARVO_PLUGIN_ADDR=127.0.0.1:0`, binds it, and prints
//! `address=127.0.0.1:PORT` on stdout. No fixed ports: two plugins never
//! collide, and a plugin restarted lands wherever the OS puts it and says so.
//! The supervisor reads stdout until that line appears; everything after it,
//! and everything on stderr, goes to the log under the child's id.
//!
//! # A child answers only the Arvo that started it
//!
//! The port is loopback and chosen by the OS, and anything on the machine
//! could still connect to it. So every spawn is handed a fresh [`Token`] in
//! its environment, the registry sends it on every call, and the child
//! refuses a call without it. A restart is a new token: nothing that learned
//! the old one keeps a way in.
//!
//! # Restart is a policy, not a default
//!
//! [`Restart::UpTo`] restarts a child that exits, with a doubling pause, and
//! after that many exits reports it unreachable with the count rather than
//! spinning on a binary that will not stay up. [`Restart::Never`] reports the
//! first exit.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::registry::PluginRegistry;
use crate::source::{Token, TOKEN_ENV};

/// The variable a supervised child reads its address from.
pub const ADDRESS_ENV: &str = "ARVO_PLUGIN_ADDR";
/// The prefix of the line a child prints once it is listening.
pub const HANDSHAKE: &str = "address=";
/// How long a child has to say its address. A Rust binary says it at once;
/// a Python plugin may be creating an environment first.
const HANDSHAKE_WAIT: Duration = Duration::from_secs(60);
/// How long a child has to die after being told to.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// What to run, and what to do when it stops.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub restart: Restart,
}

/// What happens when a child exits on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restart {
    /// Report the exit and leave it.
    Never,
    /// Restart this many times, pausing longer each time, then report.
    UpTo(u32),
}

struct Running {
    launch: Launch,
    stop: Arc<Notify>,
    task: JoinHandle<()>,
}

/// Everything Arvo has started and not yet stopped.
pub struct Supervisor {
    registry: Arc<PluginRegistry>,
    children: Mutex<HashMap<String, Running>>,
}

impl Supervisor {
    #[must_use]
    pub fn new(registry: Arc<PluginRegistry>) -> Self {
        Self { registry, children: Mutex::new(HashMap::new()) }
    }

    /// Starts `launch` under `id`, or leaves it alone if it is already
    /// running with exactly that launch. A different launch under the same id
    /// stops the old one first.
    pub async fn start(&self, id: &str, launch: Launch) {
        let mut children = self.children.lock().await;
        if let Some(running) = children.get(id) {
            if running.launch == launch {
                return;
            }
        }
        if let Some(old) = children.remove(id) {
            halt(old).await;
        }
        let stop = Arc::new(Notify::new());
        let task = tokio::spawn(run(id.to_owned(), launch.clone(), self.registry.clone(), stop.clone()));
        children.insert(id.to_owned(), Running { launch, stop, task });
        tracing::info!(plugin = id, "started");
    }

    /// Stops `id` and forgets it. Nothing happens for an id not running.
    pub async fn stop(&self, id: &str) {
        let running = self.children.lock().await.remove(id);
        if let Some(running) = running {
            halt(running).await;
            tracing::info!(plugin = id, "stopped");
        }
    }

    /// Stops everything. What Arvo started does not outlive it (ADR-0023
    /// point 5).
    pub async fn stop_all(&self) {
        let all: Vec<(String, Running)> = self.children.lock().await.drain().collect();
        for (id, running) in all {
            halt(running).await;
            tracing::info!(plugin = %id, "stopped");
        }
    }

    /// Makes what is running match `wanted`: stops what is no longer wanted,
    /// starts what is missing, restarts what changed.
    pub async fn reconcile(&self, wanted: Vec<(String, Launch)>) {
        let unwanted: Vec<String> = {
            let children = self.children.lock().await;
            children.keys().filter(|id| !wanted.iter().any(|(w, _)| w == *id)).cloned().collect()
        };
        for id in unwanted {
            self.stop(&id).await;
        }
        for (id, launch) in wanted {
            self.start(&id, launch).await;
        }
    }

    /// The ids of everything running, sorted.
    pub async fn running(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.children.lock().await.keys().cloned().collect();
        ids.sort();
        ids
    }
}

/// Tells a child to stop and waits, briefly, for it to have done so.
async fn halt(running: Running) {
    running.stop.notify_one();
    if tokio::time::timeout(STOP_WAIT, running.task).await.is_err() {
        // Its loop did not come back in time; kill_on_drop finishes the job
        // when the handle goes.
    }
}

/// One child's life: spawn, handshake, register, wait, and restart or report.
async fn run(id: String, launch: Launch, registry: Arc<PluginRegistry>, stop: Arc<Notify>) {
    let mut exits = 0u32;
    loop {
        let token = Token::fresh();
        let mut command = Command::new(&launch.program);
        command
            .args(&launch.args)
            .current_dir(&launch.cwd)
            .env(ADDRESS_ENV, "127.0.0.1:0")
            .env(TOKEN_ENV, token.expose())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                let why = if err.kind() == std::io::ErrorKind::NotFound {
                    format!("{} is not installed, or not on the PATH", launch.program)
                } else {
                    format!("could not start {}: {err}", launch.program)
                };
                registry.set_unreachable(&id, why).await;
                return;
            }
        };
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(log(id.clone(), "stderr", stderr));
        }
        let stdout = child.stdout.take();

        let address = tokio::select! {
            found = handshake(id.clone(), stdout) => found,
            () = stop.notified() => {
                let _ = child.kill().await;
                registry.remove(&id).await;
                return;
            }
            () = tokio::time::sleep(HANDSHAKE_WAIT) => None,
        };
        match address {
            Some(address) => registry.add(&id, address, Some(token)).await,
            None => {
                registry.set_unreachable(&id, format!("never said its address ({HANDSHAKE}...) on stdout")).await;
                let _ = child.kill().await;
                return;
            }
        }

        let ended = tokio::select! {
            status = child.wait() => match status {
                Ok(status) => status.code().map_or_else(|| "ended".to_owned(), |code| format!("exited with code {code}")),
                Err(err) => format!("could not wait for it: {err}"),
            },
            () = stop.notified() => {
                let _ = child.kill().await;
                registry.remove(&id).await;
                return;
            }
        };
        exits += 1;
        tracing::warn!(plugin = %id, ended, exits, "a supervised plugin stopped on its own");
        let again = match launch.restart {
            Restart::Never => false,
            Restart::UpTo(limit) => exits <= limit,
        };
        if !again {
            registry.set_unreachable(&id, format!("{ended}; stopped {exits} time{} and not restarted", if exits == 1 { "" } else { "s" })).await;
            return;
        }
        registry.set_unreachable(&id, format!("{ended}; restarting")).await;
        let pause = Duration::from_secs(1 << exits.min(6));
        tokio::select! {
            () = tokio::time::sleep(pause) => {}
            () = stop.notified() => {
                registry.remove(&id).await;
                return;
            }
        }
    }
}

/// Reads stdout until the handshake line, and returns the address it names
/// as something a client can connect to. What follows keeps being logged, so
/// a chatty child never blocks on a full pipe.
async fn handshake(id: String, stdout: Option<tokio::process::ChildStdout>) -> Option<String> {
    let stdout = stdout?;
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(address) = line.trim().strip_prefix(HANDSHAKE) {
            let address = format!("http://{}", address.trim());
            tokio::spawn(async move {
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::info!(plugin = %id, "{line}");
                }
            });
            return Some(address);
        }
        tracing::info!(plugin = %id, "{line}");
    }
    None
}

async fn log(id: String, stream: &'static str, from: impl AsyncRead + Unpin) {
    let mut lines = BufReader::new(from).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::info!(plugin = %id, stream, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::PluginStatus;
    use arvo_core::config::PluginsConfig;

    /// A child that binds a port, says so the way a plugin does, and lives
    /// for as long as it is told. Python, because it is on the machine that
    /// runs these tests and nothing else spawns as a real process this cheaply.
    fn child(seconds: &str) -> Launch {
        Launch {
            program: "python".to_owned(),
            args: vec![
                "-c".to_owned(),
                "import socket, sys, time\ns = socket.socket()\ns.bind(('127.0.0.1', 0))\nprint(f'address=127.0.0.1:{s.getsockname()[1]}', flush=True)\ntime.sleep(float(sys.argv[1]))"
                    .to_owned(),
                seconds.to_owned(),
            ],
            cwd: std::env::temp_dir(),
            restart: Restart::Never,
        }
    }

    fn python_available() -> bool {
        std::process::Command::new("python").arg("--version").output().is_ok()
    }

    /// Bounded at fifteen seconds: two Python starts and a two-second restart
    /// pause fit with room, and a hang fails rather than waits forever.
    async fn wait_for(registry: &PluginRegistry, id: &str, holds: impl Fn(&crate::registry::PluginEntry) -> bool) -> bool {
        for _ in 0..600 {
            if registry.snapshot().await.iter().any(|entry| entry.id == id && holds(entry)) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_child_says_where_it_listens_and_is_registered_there_until_stopped() {
        if !python_available() {
            eprintln!("skipped: no python on the PATH");
            return;
        }
        let registry = Arc::new(PluginRegistry::connect(&PluginsConfig::default()).await);
        let supervisor = Supervisor::new(registry.clone());

        supervisor.start("lives", child("30")).await;
        assert!(
            wait_for(&registry, "lives", |entry| entry.address.starts_with("http://127.0.0.1:")).await,
            "the handshake line became a registry address: {:?}",
            registry.snapshot().await
        );
        // Nothing speaks gRPC at that port, so it is Unreachable: registered
        // is not reachable, and this is the one case that shows the difference.
        assert!(registry.snapshot().await.iter().any(|e| e.id == "lives" && matches!(e.status, PluginStatus::Unreachable(_))));
        assert_eq!(supervisor.running().await, ["lives"]);

        supervisor.stop("lives").await;
        assert!(supervisor.running().await.is_empty());
        assert!(!registry.snapshot().await.iter().any(|e| e.id == "lives"), "stopped, and gone from the registry");
    }

    #[tokio::test]
    async fn a_child_that_keeps_dying_is_restarted_as_often_as_its_policy_says_then_reported() {
        if !python_available() {
            eprintln!("skipped: no python on the PATH");
            return;
        }
        let registry = Arc::new(PluginRegistry::connect(&PluginsConfig::default()).await);
        let supervisor = Supervisor::new(registry.clone());

        let mut launch = child("0");
        launch.restart = Restart::UpTo(1);
        supervisor.start("dies", launch).await;
        // One exit, one restart (a two-second pause), one more exit, reported.
        assert!(
            wait_for(&registry, "dies", |entry| matches!(&entry.status, PluginStatus::Unreachable(why) if why.contains("stopped 2 times"))).await,
            "restarted once then given up on: {:?}",
            registry.snapshot().await
        );
        supervisor.stop_all().await;
    }

    #[tokio::test]
    async fn reconcile_stops_what_is_no_longer_wanted_and_leaves_the_rest_running() {
        if !python_available() {
            eprintln!("skipped: no python on the PATH");
            return;
        }
        let registry = Arc::new(PluginRegistry::connect(&PluginsConfig::default()).await);
        let supervisor = Supervisor::new(registry.clone());
        supervisor.reconcile(vec![("a".to_owned(), child("30")), ("b".to_owned(), child("30"))]).await;
        assert_eq!(supervisor.running().await, ["a", "b"]);
        supervisor.reconcile(vec![("b".to_owned(), child("30"))]).await;
        assert_eq!(supervisor.running().await, ["b"], "a stopped, b untouched");
        supervisor.stop_all().await;
        assert!(supervisor.running().await.is_empty());
    }
}
