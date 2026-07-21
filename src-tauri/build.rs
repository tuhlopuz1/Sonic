fn main() {
    // cpal's Android backend (oboe/AAudio) pulls in C++ code that needs the NDK's
    // C++ runtime (libc++_shared) for symbols like __cxa_pure_virtual — without this,
    // the whole native library fails to dlopen at app startup with UnsatisfiedLinkError,
    // before any of our own Rust code ever runs.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-link-lib=c++_shared");
    }
    tauri_build::build()
}
