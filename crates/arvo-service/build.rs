//! Stamps the build with the git commit it was made from (#189).
//!
//! A finding records which engine produced it, so two findings from different
//! builds of the same ruleset can be told apart. `ARVO_COMMIT` is the short
//! hash, with `-dirty` when the tree had uncommitted changes, or `unknown`
//! when the build did not happen inside a git checkout.

use std::process::Command;

fn main() {
    // Re-run when HEAD moves or the index changes, which is when the answer
    // can change; a rebuild for any other reason keeps the cached value.
    for path in ["../../.git/HEAD", "../../.git/index"] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rustc-env=ARVO_COMMIT={}", commit());
}

fn commit() -> String {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let Some(sha) = git(&["rev-parse", "--short=12", "HEAD"]) else {
        return "unknown".to_owned();
    };
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|out| !out.is_empty());
    if dirty { format!("{sha}-dirty") } else { sha }
}
