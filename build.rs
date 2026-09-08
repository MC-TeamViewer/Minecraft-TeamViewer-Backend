use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let protocol_root = manifest.join("third_party/TeamViewRelay-Protocol/proto");
    // 应用层(teamviewer.v1)+ 传输层门控(teamviewer.door.v1,0.9.0-alpha.4
    // 分层)两份协议一并编译。
    let protocol_files = [
        protocol_root.join("teamviewer/v1/teamviewer.proto"),
        protocol_root.join("teamviewer/door/v1/door.proto"),
    ];
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");

    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc);
    config
        .compile_protos(&protocol_files, &[protocol_root])
        .expect("compile pinned TeamViewRelay protocol");

    for file in &protocol_files {
        println!("cargo:rerun-if-changed={}", file.display());
    }
}
