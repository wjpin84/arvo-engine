//! The contract's services, generated here.
//!
//! Only the services. Their request and response types are generated once, by
//! `arvo-api`, and `extern_path` points the stubs at that crate rather than
//! emitting a second copy of every message: prost generates one Rust module
//! per proto package, so two crates compiling `arvo.research.v1` would define
//! `RunRequest` twice and the window would spend its life converting one into
//! the other.
//!
//! The division is what lets the editor link `arvo-api` alone. It compiles to
//! WebAssembly, where the gRPC transport this crate brings does not build.

fn main() {
    // protoc is vendored rather than required on the machine, as in
    // arvo-plugin-host.
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc binary not available for this host target");
    // SAFETY: a build script runs `main` on one thread and nothing here spawns
    // another before this point; the variable is read moments later, in
    // process, by `tonic_prost_build`.
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }
    // The contract is its own repository, checked out here as a submodule: one
    // source of truth for the engine's API, which the desktop and the engine
    // each generate from rather than sharing a crate.
    println!("cargo:rerun-if-changed=../../contract/protos");
    let root = std::path::Path::new("../../contract/protos");
    let services = root.join("arvo/services/v1");
    let files: Vec<std::path::PathBuf> = std::fs::read_dir(&services)
        .expect("the contract is checked out; run `git submodule update --init`")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "proto"))
        .collect();
    assert!(!files.is_empty(), "no service protos under {}", services.display());
    let mut config = tonic_prost_build::configure();
    for domain in ["common", "market", "platform", "portfolio", "research", "session"] {
        config = config.extern_path(format!(".arvo.{domain}.v1"), format!("::arvo_api::{domain}"));
    }
    config
        .build_client(true)
        .build_server(true)
        .compile_protos(&files, &[root.to_path_buf()])
        .expect("failed to compile the engine API");
}
