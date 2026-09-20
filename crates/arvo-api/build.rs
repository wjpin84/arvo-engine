//! The contract's types, generated here and nowhere else.
//!
//! Every message package: the models a domain is made of and the views a
//! front end renders of it, which share a package because they share a
//! subject. The services are not here — `arvo-client` generates those, and
//! points their signatures back at these types with `extern_path`.
//!
//! The split is by kind rather than by package, and it has to be: prost
//! generates one Rust module per proto package, so two crates cannot each
//! compile `arvo.research.v1` without defining the same type twice. Messages
//! here, services there.
//!
//! Here rather than there because the editor links this crate and compiles to
//! WebAssembly, where a gRPC transport does not build. Messages only: prost,
//! serde, and no tonic.

fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc binary not available for this host");
    // SAFETY: a build script runs `main` on one thread and nothing here spawns
    // another before this point; the variable is read moments later, in
    // process, by `prost_build`.
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }
    println!("cargo:rerun-if-changed=../../contract/protos");
    let root = std::path::Path::new("../../contract/protos");
    let files: Vec<std::path::PathBuf> = walk(root)
        .filter(|path| path.extension().is_some_and(|e| e == "proto"))
        // The services are `arvo-client`'s to generate.
        .filter(|path| !path.components().any(|c| c.as_os_str() == "services"))
        .collect();
    assert!(!files.is_empty(), "no protos under {}; run `git submodule update --init`", root.display());
    prost_build::Config::new()
        // One file with the whole module tree: a package that names a type in
        // another is generated as a path through it, so the modules have to
        // mirror the package names rather than be flattened by hand.
        .include_file("arvo.rs")
        // These reach the editor over Tauri's bridge, which is JSON.
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .compile_protos(&files, &[root.to_path_buf()])
        .expect("failed to compile the contract's types");
}

/// Every file under `dir`, depth first.
fn walk(dir: &std::path::Path) -> Box<dyn Iterator<Item = std::path::PathBuf>> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Box::new(std::iter::empty()) };
    Box::new(entries.flatten().flat_map(|entry| {
        let path = entry.path();
        if path.is_dir() { walk(&path) } else { Box::new(std::iter::once(path)) }
    }))
}
