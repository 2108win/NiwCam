// The same export list as the C++ DLL (src/dshow/dshow.def, PRIVATE = no import library entries).
fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    println!("cargo:rerun-if-changed=dshow.def");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-cdylib-link-arg=/DEF:{dir}/dshow.def");
    }
}
