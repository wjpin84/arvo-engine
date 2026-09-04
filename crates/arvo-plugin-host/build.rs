fn main() {
    // ponytail: protoc isn't installed on this machine, so vendor the binary
    // instead of requiring a system install.
    let protoc = protoc_bin_vendored::protoc_bin_path()
        .expect("vendored protoc binary not available for this host target");

    // SAFETY: `set_var` is unsound only when another thread may concurrently
    // read or write the environment. This is a build script: Cargo runs
    // `main` on a single thread, and nothing here spawns one before this
    // point, so no other thread can observe the environment while it is
    // being mutated. The variable is read moments later, in-process, by
    // `tonic_prost_build`.
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }

    tonic_prost_build::compile_protos("../../protos/arvo/plugin/v1/plugin.proto")
        .expect("failed to compile plugin.proto");
}
