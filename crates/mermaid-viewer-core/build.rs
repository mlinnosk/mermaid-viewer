/// Link-argument shim for the raylib C library on Windows/MSVC.
///
/// We build raylib with `default-features = false` and only the modules the
/// application uses (see `Cargo.toml`), so raylib-sys's build script compiles
/// the C library with `SUPPORT_MODULE_RMODELS`/`SUPPORT_MODULE_RAUDIO`/
/// `USE_AUDIO` OFF — i.e. no `models.c`/`raudio.c` objects. The safe `raylib`
/// crate, however, compiles its `core::models`/`core::audio` modules
/// unconditionally, so the final link references ~21 C symbols the library
/// does not define.
///
/// Linux's `ld` (and macOS `ld`) silently drop those references, but MSVC's
/// `link.exe` resolves every `extern` in the linked objects and fails with
/// LNK1120. This is the exact scenario raylib-sys 6.0.0's build script
/// documents ("NOTE(WINDOWS ONLY, linux is fine): Custom builds that turn
/// off modules might not link because the linker tries to find the function
/// def which windows sometimes does not optimize out"), and the workaround
/// it prescribes is `cargo:rustc-link-arg=/FORCE:UNRESOLVED` from the
/// consumer's build script. The application never calls the models/audio
/// functions, so the unresolved symbols are never bound at runtime.
fn main() {
    // Guard on the *target* triple, not `cfg!(windows)`: build scripts are
    // compiled and run for the host, so `cfg!` would be wrong for
    // cross-compilation.
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    if target.contains("windows") {
        println!("cargo:rustc-link-arg=/FORCE:UNRESOLVED");
    }
}
