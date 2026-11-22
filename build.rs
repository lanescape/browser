fn main() {
    println!("cargo:rerun-if-changed=proto/lanescape/browser/control.proto");
    println!("cargo:rerun-if-env-changed=LANESCAPE_BROWSER_BUILD");
    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc_bin_vendored::protoc_bin_path().unwrap());
    config
        .compile_protos(&["proto/lanescape/browser/control.proto"], &["proto"])
        .unwrap();
}
