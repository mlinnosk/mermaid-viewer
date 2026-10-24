/// Link stubs for the raylib models/audio C API, Windows/MSVC only.
///
/// This project builds raylib with `default-features = false` and only the
/// modules the application uses (see `Cargo.toml`), so raylib-sys's build
/// script compiles the C library with `SUPPORT_MODULE_RMODELS` /
/// `SUPPORT_MODULE_RAUDIO` / `USE_AUDIO` OFF — i.e. no `models.c` /
/// `raudio.c` objects. The safe `raylib` crate, however, compiles its
/// `core::models` / `core::audio` FFI stubs unconditionally, so any link
/// that pulls in the `raylib` rlib references 21 C symbols the library
/// does not define.
///
/// Linux's `ld` (and macOS's `ld`) silently drop those references, but
/// MSVC's `link.exe` resolves every `extern` in the linked objects and
/// fails with LNK1120. This is the exact scenario raylib-sys 6.0.0's
/// build script documents ("NOTE(WINDOWS ONLY, linux is fine): Custom
/// builds that turn off modules might not link because the linker tries
/// to find the function def which windows sometimes does not optimize
/// out").
///
/// Rather than the `/FORCE:UNRESOLVED` link-arg workaround that raylib-sys
/// prescribes (which suppresses *all* unresolved-symbol errors and would
/// mask real link regressions), this build script compiles
/// `src/link_stubs.c` — which defines the 21 missing symbols, each
/// `abort()`ing — into a static library and links it via
/// `cargo:rustc-link-lib` / `cargo:rustc-link-search`. Unlike
/// `cargo:rustc-link-arg`, those directives propagate from a dependency's
/// build script to *every* link target that depends on it (this package's
/// own integration-test executables and the `mermaid-viewer` binary), so
/// one build script covers all links on Windows.
///
/// The application never calls the models/audio functions, so the stubs
/// are never executed; each `abort()`s if it is ever reached, which is the
/// honest behaviour for a function the linked C library does not
/// implement.
fn main() {
    // Guard on the *target* triple, not `cfg!(windows)`: build scripts are
    // compiled and run for the host, so `cfg!` would be wrong for
    // cross-compilation.
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    if target.contains("windows") {
        cc::Build::new()
            .file("src/link_stubs.c")
            .compile("raylib_link_stubs");
        println!(
            "cargo:rustc-link-search=native={}",
            std::env::var("OUT_DIR").expect("cargo sets OUT_DIR for build scripts")
        );
        println!("cargo:rustc-link-lib=static=raylib_link_stubs");
    }
    println!("cargo:rerun-if-changed=src/link_stubs.c");
}
