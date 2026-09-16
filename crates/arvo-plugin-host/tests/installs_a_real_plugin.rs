//! The whole path a person's install takes, against the real thing: clone the
//! Yahoo plugin from GitHub, build it with the recipe its manifest declares,
//! hand what it produced to the supervisor, and watch it register, serve, and
//! stop. Ignored by default because it clones and compiles; run it when the
//! boundary changes:
//!
//!     cargo test -p arvo-plugin-host --test installs_a_real_plugin -- --ignored --nocapture
//!
//! What it does not cover is the window: the Extensions view calling the
//! commands that do exactly this.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use arvo_core::config::PluginsConfig;
use arvo_data::source::Source;
use arvo_plugin_host::registry::{PluginRegistry, PluginStatus};
use arvo_plugin_host::source::SERVICE;
use arvo_plugin_host::supervisor::{Launch, Restart, Supervisor};

const REPO: &str = "https://github.com/wjpin84/arvo-plugin-yahoo.git";

fn run(program: &str, args: &[&str], cwd: &std::path::Path, envs: &[(&str, &str)]) {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .envs(envs.iter().copied())
        .output()
        .unwrap_or_else(|err| panic!("{program}: {err}"));
    assert!(
        output.status.success(),
        "{program} {}: {}\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test]
#[ignore = "clones a repository and builds it; minutes, and needs git, cargo and the network"]
async fn a_plugin_cloned_and_built_by_its_recipe_registers_serves_and_stops() {
    let temp = tempfile::tempdir().expect("tempdir");
    let checkout = temp.path().join("yahoo");
    let build_dir = temp.path().join("build");

    // Install: what the installer does, minus the manifest read that the
    // recipe below is copied from.
    run("git", &["clone", "--depth", "1", REPO, checkout.to_str().expect("utf-8")], temp.path(), &[]);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(checkout.join("arvo-extension.json")).expect("manifest")).expect("json");
    let provider = &manifest["contributes"]["providers"][0];
    let build = provider["build"].as_str().expect("build");
    let run_path = provider["run"].as_str().expect("run");
    assert_eq!(build, "cargo build --release");

    // Build: the recipe, argv, in the checkout, scratch under the cache.
    let mut words = build.split_whitespace();
    let program = words.next().expect("a program");
    let args: Vec<&str> = words.collect();
    run(program, &args, &checkout, &[("CARGO_TARGET_DIR", build_dir.to_str().expect("utf-8"))]);

    // Verify: what `run` names exists, .exe or not, where CARGO_TARGET_DIR put it.
    let named = PathBuf::from(run_path);
    let file_name = named.file_name().expect("a file name").to_string_lossy().into_owned();
    let produced = build_dir.join("release").join(if cfg!(windows) { format!("{file_name}.exe") } else { file_name });
    assert!(produced.is_file(), "the build produced nothing at {}", produced.display());

    // Supervise: the artifact, from where Arvo would keep it.
    let registry = Arc::new(PluginRegistry::connect(&PluginsConfig::default()).await);
    let supervisor = Supervisor::new(registry.clone());
    supervisor
        .start(
            "yahoo/yahoo",
            Launch { program: produced.display().to_string(), args: Vec::new(), cwd: checkout.clone(), restart: Restart::Never },
        )
        .await;

    let mut reachable = None;
    for _ in 0..600 {
        if let Some(entry) = registry.snapshot().await.into_iter().find(|entry| entry.id == "yahoo/yahoo") {
            if matches!(entry.status, PluginStatus::Reachable(_)) {
                reachable = Some(entry);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let entry = reachable.expect("the supervised plugin said its address and answered its manifest");
    let PluginStatus::Reachable(manifest) = &entry.status else { unreachable!() };
    assert!(manifest.capabilities.iter().any(|c| c.name == SERVICE), "{manifest:?}");
    assert!(entry.address.starts_with("http://127.0.0.1:"), "a port of the OS's choosing: {}", entry.address);
    assert_eq!(entry.sources.len(), 2, "both Yahoo venues discovered");
    assert_eq!(entry.sources[0].id(), "yahoo");
    assert!(entry.sources[0].connected().await.expect("asked"), "Yahoo needs no credential");
    eprintln!("reachable at {} serving {:?}", entry.address, entry.sources.iter().map(|s| s.id()).collect::<Vec<_>>());

    // Stop: gone from the registry, and the process with it.
    supervisor.stop("yahoo/yahoo").await;
    assert!(supervisor.running().await.is_empty());
    assert!(!registry.snapshot().await.iter().any(|entry| entry.id == "yahoo/yahoo"));
}
