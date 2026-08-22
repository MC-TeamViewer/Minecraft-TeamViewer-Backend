use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let protocol_root = manifest.join("../third_party/TeamViewRelay-Protocol/proto");
    let protocol_file = protocol_root.join("teamviewer/v1/teamviewer.proto");
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");

    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc);
    config
        .compile_protos(std::slice::from_ref(&protocol_file), &[protocol_root])
        .expect("compile pinned TeamViewRelay protocol");

    println!("cargo:rerun-if-changed={}", protocol_file.display());
}
