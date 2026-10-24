/*
 * Link stubs for the raylib models/audio C API, Windows/MSVC only.
 *
 * This project builds raylib with `default-features = false` and only the
 * modules the application uses (see `Cargo.toml`), so raylib-sys's build
 * script compiles the C library with `SUPPORT_MODULE_RMODELS` /
 * `SUPPORT_MODULE_RAUDIO` / `USE_AUDIO` OFF — i.e. no `models.c` /
 * `raudio.c` objects. The safe `raylib` crate, however, compiles its
 * `core::models` / `core::audio` FFI stubs unconditionally, so any link that
 * pulls in the `raylib` rlib references ~21 C symbols the library does not
 * define.
 *
 * Linux's `ld` (and macOS `ld`) silently drop those references, but MSVC's
 * `link.exe` resolves every `extern` in the linked objects and fails with
 * LNK1120. This is the exact scenario raylib-sys 6.0.0's build script
 * documents ("NOTE(WINDOWS ONLY, linux is fine): Custom builds that turn off
 * modules might not link because the linker tries to find the function def
 * which windows sometimes does not optimize out").
 *
 * Rather than the `/FORCE:UNRESOLVED` link-arg workaround that raylib-sys
 * prescribes (which suppresses *all* unresolved-symbol errors and would mask
 * real link regressions), these stubs define the 21 missing symbols so every
 * link target — the binary, the integration-test executables, and the
 * `view-headless` pass — resolves them cleanly.
 *
 * The application never calls the models/audio functions, so the stubs are
 * never executed; each `abort()`s if it is ever reached, which is the honest
 * behaviour for a function the linked C library does not implement. The
 * signatures use `void` returns and no parameters: at link time only the
 * (unmangled) symbol name matters, and the `abort()` runs before any
 * argument is read or return value produced.
 */

#include <stdlib.h>

/* models module (SUPPORT_MODULE_RMODELS) */
void LoadModel(void) { abort(); }
void LoadModelFromMesh(void) { abort(); }
void LoadModelAnimations(void) { abort(); }
void UnloadModel(void) { abort(); }
void UnloadModelAnimations(void) { abort(); }
void UploadMesh(void) { abort(); }
void UnloadMesh(void) { abort(); }
void LoadMaterials(void) { abort(); }
void UnloadMaterial(void) { abort(); }

/* audio module (SUPPORT_MODULE_RAUDIO / USE_AUDIO) */
void LoadSoundAlias(void) { abort(); }
void UnloadSoundAlias(void) { abort(); }
void UnloadWaveSamples(void) { abort(); }
void UnloadWave(void) { abort(); }
void UnloadSound(void) { abort(); }
void UnloadMusicStream(void) { abort(); }
void UnloadAudioStream(void) { abort(); }
void SetAudioStreamCallback(void) { abort(); }
void AttachAudioMixedProcessor(void) { abort(); }
void DetachAudioMixedProcessor(void) { abort(); }
void AttachAudioStreamProcessor(void) { abort(); }
void DetachAudioStreamProcessor(void) { abort(); }
