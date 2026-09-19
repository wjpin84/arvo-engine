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
    tonic_prost_build::compile_protos("../../contract/protos/arvo/engine/v1/engine.proto")
        .expect("failed to compile engine.proto");
}
