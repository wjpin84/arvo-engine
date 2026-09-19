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
    let files: Vec<std::path::PathBuf> = walk(root).filter(|p| p.extension().is_some_and(|e| e == "proto")).collect();
    assert!(!files.is_empty(), "no protos under {}; run `git submodule update --init`", root.display());
    tonic_prost_build::configure()
        // One file with the whole module tree: a package that names a type in
        // another is generated as a path through it, so the modules have to
        // mirror the package names rather than be flattened by hand.
        .include_file("arvo.rs")
        // The views reach the editor over Tauri's bridge, which is JSON, so the
        // generated types carry serde as well as prost.
        .type_attribute(".arvo.views.v1", "#[derive(serde::Serialize, serde::Deserialize)]")
        .compile_protos(&files, &[root.to_path_buf()])
        .expect("failed to compile the engine API");
}

/// Every file under `dir`, depth first.
fn walk(dir: &std::path::Path) -> Box<dyn Iterator<Item = std::path::PathBuf>> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Box::new(std::iter::empty()) };
    Box::new(entries.flatten().flat_map(|entry| {
        let path = entry.path();
        if path.is_dir() { walk(&path) } else { Box::new(std::iter::once(path)) }
    }))
}
