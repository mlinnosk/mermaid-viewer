# Blueprint: `view --watch` — live-reloading diagram window

## Global Design Brief

### A. Objective and Non-Goals

**Objective:** `mermaid-viewer view --watch file.mermaid` renders the diagram, opens the same GUI window as `view` today, and re-renders the window content every time the file changes on disk. When the user has zoomed or panned, the zoom multiplier and pan offset are retained across re-renders as well as the changed image bounds allow.

**Non-goals (intentionally unaltered):**
- `view` without `--watch` is byte-for-byte the same behavior.
- `WindowController` replace semantics (MCP `show_diagram` on a known ID resets zoom/pan) are **not** changed — watch mode does not go through the controller.
- `render` and `mcp` are untouched.
- Exit-code contract (0 / 1 / 2) is unchanged.
- No watching of `--code` or `--stdin` sources: `--watch` is a usage error without a `FILE` source (exit 2).

### B. Existing System Evidence

- `crates/mermaid-viewer/src/main.rs` — `run_view` (lines 166–175): render once → `run_blocking`. `read_source` reads the file once. Watch mode must re-read and re-render per change; the initial render must still fail before any window opens (typed `Error::Render`, exit 1).
- `crates/mermaid-viewer-core/src/view.rs`:
  - `run_blocking(title, image)` (lines 822–835): fake-backend short-circuit, then `Window::init_raylib` + `show_tab("default", …)` + `while window.step_cli(&mut rl, &thread) {}`.
  - `step_cli` (lines 740–767) runs every frame — the natural drain point for queued updates.
  - `TabState` (lines 498–512) holds per-tab `zoom`, `pan_x`, `pan_y`; `TabbedState::show` (567–594) resets them on replace — that reset is why a new update path is needed rather than reusing `show_tab`.
  - `Window::show_tab` (674–698) shows the texture-swap pattern: upload new texture, then swap `textures[index]`.
  - Test conventions: fake-backend unit tests live in `view.rs`'s `mod tests` (with the `test_png` helper); real-draw-path tests live in `crates/mermaid-viewer-core/tests/view_headless.rs` behind `#[cfg(feature = "view-headless")]`, driving `#[doc(hidden)] pub` seams.
- `crates/mermaid-viewer-core/src/lib.rs` (line 58): `pub use view::{run_blocking, WindowController};` — new public view API must be re-exported here.
- `crates/mermaid-viewer/src/cli.rs`: `ViewArgs` derives `Args`; the positional field is `file` (arg id `file`), so clap `requires = "file"` is expressible on `--watch`.
- `crates/mermaid-viewer/Cargo.toml`: binary deps (`clap`, `tokio`, `rmcp`); `notify` is absent from the workspace lockfile and must be added here only.

### C. Intended Design

Two pieces:

1. **Core — update-capable blocking window** (`view.rs`): a new public `run_blocking_updates(title, image, updates)` that is `run_blocking` plus a per-frame, non-blocking drain of an `std::sync::mpsc::Receiver<RenderedImage>`. Each received PNG replaces the active tab's content and texture **while retaining the tab's zoom/pan** — the fit scale is recomputed against the new image dimensions by the existing `compute_layout`, so relative zoom is preserved ("as best as possible"). `run_blocking` becomes a thin delegation (a receiver from a dropped sender behaves as "no updates"), keeping one code path. The retention itself is a pure operation on `TabbedState` (`update_content`), testable under every feature pass; the texture swap is a small `#[doc(hidden)] pub Window::apply_update` seam, pixel-tested in the `view-headless` pass. The fake backend validates the initial image, drains the currently queued updates (validating each), and returns immediately — consistent with `run_blocking`'s existing fake semantics and making the path CI-testable.

2. **Binary — `--watch` flag and watcher thread** (`cli.rs`, `main.rs`): `--watch`/`-w` (clap `requires = "file"`). The watch path renders the initial image (errors propagate before the window), opens an mpsc channel, spawns a watcher thread using `notify`'s recommended watcher on the file's **parent directory** (directory-level watching is required to observe atomic-save renames, which is how most editors save), filters events to the target path, debounces, re-reads + re-renders, and sends PNGs on the channel. Render failures during watching (transient invalid source while editing, unreadable mid-save file) print a diagnostic to stderr and keep the last good diagram — the window never closes because of a bad intermediate state. Main thread runs `run_blocking_updates`; when the user closes the window, the receiver drops, the watcher's next send fails, and the thread exits.

**Rejected alternative:** driving the CLI watch window through `WindowController::show_tab` — it already supports content replacement, but it (a) draws the tabbed frame the CLI `view` explicitly does not have, and (b) resets zoom/pan on replace, violating the retention requirement. Extending the blocking path is smaller and preserves both contracts.

**Invariants every milestone must preserve:**
- The real window loop's update drain is non-blocking per frame and treats a disconnected channel as "no updates" (never exits the loop).
- A non-PNG value arriving on the update channel is a typed `Error::View` ending the run (it is a programming error; the watcher only sends successfully rendered PNGs).
- An update never changes `zoom`, `pan_x`, `pan_y`, label, or active index — only content/texture.
- Fake-backend behavior stays immediate/non-blocking.

### D. Contract and Compatibility Map

**CC-1 — `run_blocking_updates`** (public, `mermaid_viewer_core::view`, re-exported from `mermaid_viewer_core`):
```rust
pub fn run_blocking_updates(
    title: &str,
    image: &RenderedImage,
    updates: &std::sync::mpsc::Receiver<RenderedImage>,
) -> Result<()>;
```
Real backend: identical window to `run_blocking`; each frame drains `updates` non-blocking, replacing the active tab's content per **CC-2**; `TryRecvError::Disconnected` is treated as empty; a non-PNG update is `Error::View`. Fake backend: validates `image` (PNG check), then drains currently queued updates with `try_recv` until empty, validating each PNG, returns `Ok(())`.

**CC-2 — View retention:** an update replaces only the tab's `image`/texture. `zoom`, `pan_x`, `pan_y`, `label`, and the active index are carried over unchanged; `compute_layout` recomputes the fit against the new image dimensions, so the retained `zoom` is a relative multiplier over the new fit.

**CC-3 — `TabbedState::update_content`** (internal, `view.rs`): `fn update_content(&mut self, id: &str, image: &RenderedImage) -> bool` — replaces the tab's content retaining zoom/pan/label/active; returns `false` (no-op) for an unknown ID.

**CC-4 — `Window::apply_update`** (`#[doc(hidden)] pub`, `view.rs`): `pub fn apply_update(&mut self, rl: &mut RaylibHandle, thread: &RaylibThread, image: &RenderedImage) -> Result<()>` — `ensure_png`, upload the texture, apply **CC-3** to the active tab, swap the texture at the active index. Exists for the real loop and the headless pixel pass.

**CC-5 — `ViewArgs.watch`** (public CLI): `#[arg(short = 'w', long, requires = "file")] pub watch: bool`. `--watch` without a `FILE` source is a clap usage error (exit 2 via the existing `main` mapping).

**CC-6 — Watch diagnostics:** a failed re-render during watching prints exactly `mermaid-viewer: watch: render failed: {err}; keeping the previous diagram` to stderr; the window keeps the last good image and watching continues. Missing/unreadable file at re-render time follows the same path.

**CC-7 — Dependency:** `notify = "8"` added to `crates/mermaid-viewer/Cargo.toml` `[dependencies]` only.

**Consumers affected:** `run_blocking`'s delegation to CC-1 (no signature change); `lib.rs` re-export line; `main.rs` `run_view` (gains a branch); `cli.rs` module doc usage block; `README.md` `view` section. MCP (`mcp.rs`) is unaffected — its replace-resets-view behavior is intentionally distinct from CC-2.

### E. Milestone Map

1. **[M-1] Core: update-capable blocking window with view retention** — lands the window mechanism first so the binary consumes a finished, tested core contract. `[M-1.a]` state machine + API + fake-backend tests (buildable/testable alone: `run_blocking` still works via delegation), `[M-1.b]` headless pixel proof of the texture swap.
2. **[M-2] Binary: `--watch` flag and watcher loop** — `[M-2.a]` CLI surface (parse-level, independently testable), `[M-2.b]` watcher wiring against CC-1.
3. **[M-3] Documentation** — README; last so it describes shipped behavior.

---

## [M-1] Core: update-capable blocking window with view retention

*Adds the core window mechanism that lets a long-lived CLI window receive new PNGs while the user keeps their zoom and pan — the foundation `view --watch` renders through.*

#### Contract Card
- **CC-1** `run_blocking_updates` — signature, real/fake semantics, and update-drain rules exactly as stated in the Global Design Brief section D.
- **CC-2** View retention — update touches content/texture only; zoom/pan/label/active carried over.
- **CC-3** `TabbedState::update_content(&mut self, id: &str, image: &RenderedImage) -> bool`.
- **CC-4** `Window::apply_update` (`#[doc(hidden)] pub`) — `ensure_png` → texture upload → CC-3 on the active tab → texture swap at the active index.
- Re-export: `lib.rs` line 58 becomes `pub use view::{run_blocking, run_blocking_updates, WindowController};`.

#### Verification Plan
- **Commands:**
  - `cargo test -p mermaid-viewer-core` (default-feature pass: fake backend + pure state)
  - `cargo test -p mermaid-viewer-core --no-default-features --features view-headless` (headless pixel pass)
  - `cargo clippy -p mermaid-viewer-core --all-targets -- -D warnings`
- **Named test cases** (the first two go in `view.rs` `mod tests`, using the existing `test_png` helper; the third in `tests/view_headless.rs`):
  - `update_content_retains_view` — given a tab shown with image A whose `zoom`/`pan_x`/`pan_y` are set to distinct non-default values, when `update_content(id, image_b)` is called, then the tab's image is B and zoom/pan/label/active are unchanged; and `update_content` on an unknown ID returns `false` and changes nothing.
  - `fake_run_blocking_updates_drains_and_validates` — given `MERMAID_VIEWER_FAKE_WINDOW=1`: (a) a channel carrying two queued PNGs → `run_blocking_updates` returns `Ok` and consumes both; (b) a channel carrying a queued SVG `RenderedImage` → returns `Err(Error::View)`; (c) a disconnected channel with the initial PNG → `Ok`.
  - `apply_update_swaps_drawn_content` — given a headless window shown with a solid-color PNG, when `apply_update` installs a differently colored PNG and the frame is rendered through the existing `render_frame`/`draw_tabbed_frame` harness, then the drawn pixels match the new image's color (the texture really was swapped).
- **Definition of done:** all three commands pass; `run_blocking` still behaves identically (existing `run_blocking_is_immediate_under_fake_env`, `view_rejects_svg_image` pass unchanged); no new `unsafe`.

#### [M-1.a] Update drain, retention state change, and public API
- **Intent:** Implement CC-1/2/3 in `view.rs`, make `run_blocking` delegate to the new function, and prove retention + fake-backend semantics with unit tests.
- **Target Files:**
  - `[MODIFY]` `crates/mermaid-viewer-core/src/view.rs`
  - `[MODIFY]` `crates/mermaid-viewer-core/src/lib.rs`
- **Requirements:**
  - `view.rs`: implement CC-3 on `TabbedState`, CC-1 as a public function, and restructure the CLI loop so `run_blocking` delegates to `run_blocking_updates` with a receiver from a dropped sender (one code path; per-frame drain is non-blocking and frame-loop integrated so updates appear on the next frame — i.e., drained in `step_cli`'s per-frame position, before drawing).
  - CC-4 (`apply_update`) belongs to this sub-milestone's compile unit too (the real loop calls it); its pixel proof is [M-1.b]. Mark it `#[doc(hidden)] pub` mirroring `load_window_texture`.
  - `lib.rs`: re-export per the Contract Card.
  - Module doc of `view.rs`: extend the two-mode overview with the update-drain mode (blocking window + update channel).
  - Fake-backend `run_blocking_updates` must not block: validate initial, drain queue, return.
  - Tests `update_content_retains_view` and `fake_run_blocking_updates_drains_and_validates` as specified in the Verification Plan.
  - `builder's choice`: internal placement of the drain call within the loop and the exact drain helper shape — subject to the invariant that a disconnected channel never ends the run.

#### [M-1.b] Headless pixel proof of the update path
- **Intent:** Prove with the real draw path (software renderer) that `apply_update` actually swaps the displayed texture — the unit tests in [M-1.a] cannot see textures.
- **Target Files:**
  - `[MODIFY]` `crates/mermaid-viewer-core/tests/view_headless.rs`
- **Requirements:**
  - Add the `apply_update_swaps_drawn_content` test from the Verification Plan, following the file's existing conventions (global init lock, `solid_png` helper, `with_headless`/`render_frame` harness, doc comment stating the shared verification plan).
  - The test builds a `Window`, `show_tab`s image A, calls CC-4 `apply_update` with image B (different color, different dimensions), renders the frame, and pixel-asserts the new color.
- **Completion Checklist**
  - [ ] `run_blocking_updates` exported from `mermaid_viewer_core` and behaviorally per CC-1 (real + fake)
  - [ ] Updates retain zoom/pan (CC-2) — pure test green under both feature passes
  - [ ] `apply_update` pixel-proven in the headless pass
  - [ ] `cargo test -p mermaid-viewer-core`, headless pass, and clippy all green; existing view tests untouched and passing

---

## [M-2] Binary: `--watch` flag and watcher loop

*Wires the CLI surface and the file-watching loop so `view --watch FILE` re-renders the window on every file change.*

#### Contract Card
- **CC-5** `ViewArgs.watch`: `#[arg(short = 'w', long, requires = "file")] pub watch: bool` — usage error (exit 2) when given without a `FILE` source.
- **CC-6** Watch diagnostics: stderr line `mermaid-viewer: watch: render failed: {err}; keeping the previous diagram` on any failed re-render (bad source, unreadable/missing file mid-save); the window keeps the last good image; watching continues.
- **CC-7** Dependency `notify = "8"` in `crates/mermaid-viewer/Cargo.toml`.
- **Watch behavior contract:** the watcher uses `notify`'s recommended watcher registered on the watched file's **parent directory**, filtering events to the target file path (directory-level watching is required to observe atomic-save renames). Changes are debounced: a burst of events within a 200 ms quiet window (`builder's choice: debounce mechanics`) produces exactly one re-render of the file's current content. The initial render happens before the window opens and its failure propagates as today (typed `Error::Render`, exit 1). Watch mode exits when the user closes the window (ESC / OS close, exit 0); a send failure on the update channel (window gone) ends the watcher thread.

#### Verification Plan
- **Commands:**
  - `cargo test -p mermaid-viewer`
  - `cargo clippy -p mermaid-viewer --all-targets -- -D warnings`
  - `cargo build -p mermaid-viewer` (proves CC-7 resolves)
- **Named test cases:**
  - `view_parses_watch_flag` (in `cli.rs` `mod tests`) — given `["view", "--watch", "d.mmd"]` when parsed, then `Command::View` with `watch == true`, `file` set; and `["view", "d.mmd"]` parses with `watch == false`.
  - `watch_requires_file_source` (in `cli.rs` `mod tests`) — given `["view", "--watch", "--code", "graph TD"]` and `["view", "--watch", "--stdin"]` when parsed, then a clap error (not `Ok`).
  - `run_view_watch_bad_initial_source_is_render_error` (in `main.rs` `mod tests`) — given a temp file with parser-rejected content (`gitGraph\n  frobnicate`) and a `ViewArgs { watch: true, file: Some(…) }` when `run` is invoked, then `Err(Error::Render)` (failure before any window, existing contract preserved).
  - `view_watch_fake_env_smoke` (in `tests/cli.rs`) — given the binary spawned with `MERMAID_VIEWER_FAKE_WINDOW=1` and args `view --watch <tempfile>` when the process runs, then it exits 0 (fake `run_blocking_updates` per CC-1 returns after draining; the watcher thread does not keep the process alive).
- **Definition of done:** all commands pass; `view` without `--watch` behaves exactly as before (existing tests green); `--watch` without `file` is exit 2.

#### [M-2.a] CLI surface: `--watch`
- **Intent:** Land the parse-level contract (CC-5) so the flag exists and its usage rules are enforced before any behavior wiring.
- **Target Files:**
  - `[MODIFY]` `crates/mermaid-viewer/src/cli.rs`
- **Requirements:**
  - Add the `watch` field per CC-5 to `ViewArgs` and update the module doc's usage line to `view [FILE] [-c|--code <SRC>] [--stdin] [-w|--watch] [-t|--title <T>] [-s|--scale <F>]`.
  - Doc comment on the field states: re-render the window whenever the file changes; requires a `FILE` source.
  - Both named CLI tests from the Verification Plan in the existing `mod tests`.
  - Note for the builder: existing tests construct `ViewArgs` struct-literals (e.g. `main.rs` tests) — adding a field breaks them; that repair belongs to [M-2.b], where those files are already targeted.

#### [M-2.b] Watch loop: re-render on change
- **Intent:** Implement the watch behavior contract end-to-end: initial render → window with update channel → watcher thread re-rendering on change with diagnostics (CC-6, CC-7).
- **Target Files:**
  - `[MODIFY]` `crates/mermaid-viewer/src/main.rs`
  - `[MODIFY]` `crates/mermaid-viewer/Cargo.toml`
- **Requirements:**
  - `Cargo.toml`: add CC-7.
  - `main.rs`: `run_view` branches on `args.watch` into a watch handler implementing the Watch behavior contract: render initial (existing error-before-window behavior), `mpsc::channel::<RenderedImage>()`, spawn the watcher thread, then `run_blocking_updates(&args.title, &initial, &rx)` (the CC-1 call). `--code`/`--stdin` never reach the watch handler (CC-5 rejects them at parse).
  - Watcher thread per the Watch behavior contract: directory-watcher + event filter + debounce + re-read/re-render/send; render failures per CC-6; thread ends when the channel send fails or the watcher errors fatally (`notify` watcher construction failure is a typed `Error` → exit 1 before the window opens).
  - `run`'s match arms and `read_source` still handle `View` uniformly; the watch handler receives the resolved file path (it re-reads the file itself; it must not reuse `read_source`'s single-shot result beyond the initial render).
  - Non-watch `ViewArgs` struct-literal construction sites in `main.rs`'s tests gain `watch: false`.
  - Tests `run_view_watch_bad_initial_source_is_render_error` (main.rs) and `view_watch_fake_env_smoke` (tests/cli.rs — `[MODIFY]` implied by its existing suite; adding one test) from the Verification Plan.
  - `builder's choice`: internal watcher-thread structure, event-filter implementation, and debounce mechanics — subject to the Watch behavior contract's observable rules (200 ms quiet window, one render per burst, CC-6 text).
- **Completion Checklist**
  - [ ] `--watch`/`-w` parses per CC-5; missing `FILE` is exit 2
  - [ ] `view --watch FILE` re-renders on change with zoom/pan retained (via CC-1/CC-2)
  - [ ] Bad intermediate source keeps the window with the last good diagram and prints CC-6 to stderr
  - [ ] `view` without `--watch` unchanged; `cargo test -p mermaid-viewer` and clippy green

---

## [M-3] Documentation

*Update the user-facing docs to describe `--watch`.*

#### Contract Card
- No code contracts. Doc-only: README `view` section documents the `-w|--watch` flag with the exact usage line from `cli.rs`, one example (`mermaid-viewer view --watch diagram.mmd`), and the retention + diagnostic behavior in one or two sentences.

#### Verification Plan
- **Commands:** `cargo build --workspace` (docs change cannot break the build; run as sanity).
- **Named test cases:** none — verification is that the documented usage line matches `cli.rs`'s module doc exactly.
- **Definition of done:** README's `view` section shows `--watch`, matching the implemented flag and its `FILE`-only restriction.

#### [M-3.a] README update
- **Intent:** Keep the README (the project's CLI reference) in sync with the shipped flag.
- **Target Files:**
  - `[MODIFY]` `README.md`
- **Requirements:**
  - Update the `view` usage block (around line 101) to include `-w|--watch` and add a short paragraph: watch mode re-renders on every file change, retains zoom/pan as well as the new image bounds allow, keeps the last good diagram when a saved state fails to render, and requires a `FILE` source.

#### Completion Checklist
- [ ] README documents `--watch` consistently with `cli.rs`

---

[BLUEPRINT COMPLETE - READY FOR IMPLEMENTATION]
