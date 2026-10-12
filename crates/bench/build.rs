fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if let Ok(dir) = std::env::var("DEP_OPENCAP_NATIVE_LIB_DIR") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
        let plist = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("../app/resources/Info.plist");
        println!("cargo:rerun-if-changed={}", plist.display());
        println!(
            "cargo:rustc-link-arg=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
