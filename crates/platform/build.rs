use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=native/macos.swift");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let arch = if arch == "aarch64" { "arm64" } else { "x86_64" };
    let status = Command::new("xcrun")
        .args([
            "swiftc",
            "-swift-version",
            "5",
            "-O",
            "-emit-library",
            "-module-name",
            "OpenCapNative",
            "-target",
        ])
        .arg(format!("{arch}-apple-macosx13.0"))
        .args([
            "-Xlinker",
            "-install_name",
            "-Xlinker",
            "@rpath/libopencap_native.dylib",
            "native/macos.swift",
            "-o",
        ])
        .arg(out.join("libopencap_native.dylib"))
        .status()
        .expect("macOS builds require Xcode Command Line Tools (xcrun swiftc)");
    assert!(
        status.success(),
        "Building the macOS native API bridge failed"
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=dylib=opencap_native");
    println!("cargo:lib_dir={}", out.display());
    // Used by cargo run/test. The .app packager also supplies a Frameworks rpath.
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", out.display());
}
