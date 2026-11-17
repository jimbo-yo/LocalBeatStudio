// libopus (from opus-static-sys) is built with CMake's interprocedural
// optimization; on MSVC its objects are LTCG objects, which Rust's bundling of
// static libraries into the rlib loses the symbols of (unresolved opus_*
// errors from libopusenc). Hand opus.lib straight to the linker instead.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") && let Ok(dir) = std::env::var("DEP_OPUS_LIB_PATH") {
        println!("cargo:rustc-link-search=native={dir}");
        println!("cargo:rustc-link-lib=dylib=opus");
    }
}
