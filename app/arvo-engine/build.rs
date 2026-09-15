fn main() {
    // protoc is not installed on this machine; the plugin host vendors it the
    // same way.
    let protoc = protoc_bin_vendored::protoc_bin_path()
        .expect("vendored protoc binary not available for this host target");

    // SAFETY: a build script runs `main` on one thread and nothing here spawns
    // another before `tonic_prost_build` reads the variable.
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }

    tonic_prost_build::compile_protos("../../protos/arvo/engine/v1/engine.proto")
        .expect("failed to compile engine.proto");
}
