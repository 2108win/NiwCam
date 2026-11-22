// vcam.def marks every export PRIVATE (kept out of the import library); do the same here.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        for name in [
            "DllCanUnloadNow",
            "DllGetClassObject",
            "DllRegisterServer",
            "DllUnregisterServer",
        ] {
            println!("cargo:rustc-cdylib-link-arg=/EXPORT:{name},PRIVATE");
        }
    }
}
