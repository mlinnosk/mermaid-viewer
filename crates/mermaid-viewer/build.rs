/// Link-argument shim for the raylib C library on Windows/MSVC.
///
/// The `mermaid-viewer-core` library builds raylib with
/// `default-features = false` and only the modules the application uses
/// (see `mermaid-viewer-core/Cargo.toml`), so raylib-sys's build script
/// compiles the C library with `SUPPORT_MODULE_RMODELS`/
/// `SUPPORT_MODULE_RAUDIO`/`USE_AUDIO` OFF — i.e. no `models.c`/`raudio.c`
/// objects. The safe `raylib` crate, however, compiles its
/// `core::models`/`core::audio` modules unconditionally, so the final link
/// references ~21 C symbols the library does not define.
///
/// Linux's `ld` (and macOS `ld`) silently drop those references, but MSVC's
/// `link.exe` resolves every `extern` in the linked objects and fails with
/// LNK1120. This is the exact scenario raylib-sys 6.0.0's build script
/// documents ("NOTE(WINDOWS ONLY, linux is fine): Custom builds that turn
/// off modules might not link because the linker tries to find the function
/// def which windows sometimes does not optimize out"), and the workaround
/// it prescribes is `cargo:rustc-link-arg=/FORCE:UNRESOLVED` from the
/// *top-level project's* build script. The application never calls the
/// models/audio functions, so the unresolved symbols are never bound at
/// runtime.
///
/// The flag must be emitted here, from the binary crate's own build
/// script: Cargo applies `rustc-link-arg` (and the named `*-bin`
/// variants) only from the build script of the crate currently being
/// compiled — link-args from a dependency's build script never reach the
/// final binary link (the sole exception is `rustc-link-arg-cdylib`,
/// which propagates for historical reasons).
fn main() {
    // Guard on the *target* triple, not `cfg!(windows)`: build scripts are
    // compiled and run for the host, so `cfg!` would be wrong for
    // cross-compilation.
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    if target.contains("windows") {
        println!("cargo:rustc-link-arg=/FORCE:UNRESOLVED");
    }
}
