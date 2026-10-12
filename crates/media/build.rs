fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if let Ok(dir) = std::env::var("DEP_OPENCAP_NATIVE_LIB_DIR") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
    }
}
