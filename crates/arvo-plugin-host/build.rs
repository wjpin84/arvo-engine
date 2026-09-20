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

    // The provider contract is its own repository, checked out as the
    // `extension` submodule: this side implements what a provider calls and
    // must not carry a copy that can drift from it.
    println!("cargo:rerun-if-changed=../../extension/proto");
    let root = "../../extension/proto";
    assert!(
        std::path::Path::new(root).is_dir(),
        "no provider protos under {root}; run `git submodule update --init`"
    );
    for file in ["arvo/plugin/v1/plugin.proto", "arvo/source/v1/source.proto", "arvo/signal/v1/signal.proto"] {
        tonic_prost_build::configure()
            .compile_protos(&[format!("{root}/{file}")], &[root.to_owned()])
            .unwrap_or_else(|err| panic!("failed to compile {file}: {err}"));
    }
}
