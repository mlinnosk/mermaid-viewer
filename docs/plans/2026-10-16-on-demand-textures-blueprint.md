# Blueprint — On-demand texture management: SVG in the queue, per-tab texture slots, raster worker thread, re-raster on zoom

## Global Design Brief

### A. Goal and non-goals

**Goal.** The MCP show path stops rasterizing on the caller's critical path at a fixed scale. `show_diagram` renders the Mermaid source to **SVG** and sends the SVG document — plus an initial scale hint — across the queue to the GUI thread; each tab stores its SVG. A dedicated raster worker thread rasterizes on demand using the existing resvg pipeline (embedded Carlito font), the GUI thread uploads at most **one** live texture (the active tab's), inactive tabs' textures are freed, and the window re-rasterizes at display-appropriate scales as the user zooms, within an explicit texture-memory cap. While a tab has no live texture the content area shows a minimal placeholder. The MCP wire contract is unchanged — the same two tools with the same names, schemas, statuses, and bodies; only the documented meaning of `scale` shifts from "PNG pixel scale" to "initial raster scale hint" (default 2.0 unchanged).

User-visible outcomes:

- `show_diagram` no longer pays resvg rasterization plus PNG encode/decode on the tool call; the tool's ack still bounds the whole first-raster path (a first show can fail observably), but the GUI thread never stalls on rasterization and the window opens with a placeholder instead of blocking.
- Diagrams are rasterized near their displayed scale — sharp at fit and at zoom, no fixed-2.0 blur or oversampling — and texture memory stays bounded: one live texture, capped pixel count.

**Non-goals:**

- No change to the CLI `view` path: `run_blocking`, `step_cli`, `draw_frame`, and `load_window_texture` keep the eager single-diagram PNG behavior (one tab, no switching, no worker); `render` is untouched.
- No MCP wire changes: no new tools, parameters, statuses, or body shapes; `scale` keeps its name, type, optionality, and default.
- No new dependencies and no new unsafe code: resvg/png are already direct dependencies, and the crate stays `deny(unsafe_code)` with the single documented `image_from_rgba` block.
- No multiple OS windows, no Markdown extraction, no texture tiling/streaming, no raster-cache persistence across window close/reopen, no mid-session worker respawn.

### B. Existing-system evidence

- `crates/mermaid-viewer-core/src/render.rs` — the pipeline is `mermaid source → [mermaid-render] → SVG (+w,h) → [resvg @ scale] → PNG`. `RenderedImage` already has both `ImageFormat::Svg` and `ImageFormat::Png` variants; SVG output is a complete `<?xml…?><svg width height viewBox>…</svg>` document with integer logical pixel dimensions, and `NativeRenderer::render(source, ImageFormat::Svg, opts)` never touches resvg. `rasterize_png(svg, scale)` parses via `resvg::usvg::Tree::from_data`, renders into a `tiny_skia::Pixmap`, and returns demultiplied straight RGBA8 before PNG encoding; non-finite/non-positive scales are clamped to 1.0. `raster_options()` preloads exactly one embedded font (`include_bytes!("../assets/Carlito-Regular.ttf")`, family `Carlito`) and maps `sans-serif`/`serif` onto it — usvg converts `<text>` to glyph paths at parse time, so any rasterization site using these options rasterizes identically. `Error::Render` (parse) and `Error::Rasterize` (raster) are distinct typed variants.
- `crates/mermaid-viewer-core/src/mcp.rs` — `DiagramServer::show` is where PNG is forced today: `tokio::task::spawn_blocking` → `NativeRenderer::render(&code, ImageFormat::Png, &RenderOptions { png_scale: scale })` with `DEFAULT_PNG_SCALE = 2.0`, then `spawn_blocking` → `state.commit_show(id, label, &image)`. `State::commit_show` reconciles, decides the status from the registry (the sole basis for `opened`/`added`/`updated`), lazily spawns the controller, calls `show_tab`, and inserts the ID into the registry **only after the ack succeeds** — a failed show never corrupts the registry. Exactly two tools exist; `scale` is documented as "PNG pixel scale for the window image (default 2.0)".
- `crates/mermaid-viewer-core/src/view.rs` — `TabState { id, label, image, zoom, pan_x, pan_y }` and the pure `TabbedState` add/replace/close rules with `WindowController::MAX_TABS = 16`. `Window { state, textures: Vec<Texture2D> }` keeps one eagerly-uploaded texture per tab, all alive; replacement drops the old texture (`drop(old)`) — the existing texture-freeing mechanism. `Window::show_tab` orders capacity check → `load_window_texture` → `state.show`, so a failed upload leaves the window untouched. `load_window_texture` decodes any PNG encoding to straight RGBA8 and uploads raw RGBA8 through `image_from_rgba` (the crate's single documented `#[allow(unsafe_code)]` FFI block) — the texture path already bypasses raylib's file loaders entirely, so any CPU rasterizer output is acceptable. `draw_tabbed_frame` draws a raygui tab bar (text labels) and **only the active tab's** content (`window.textures[active]`); inactive tabs draw nothing, so freeing their textures is safe. `apply_input` applies wheel zoom (`ZOOM_STEP`, clamped `MIN_ZOOM`..`MAX_ZOOM`) and drag pan to the active tab each frame. `Cmd` is plain data; `RaylibThread` never crosses channels. `Inner::Real(Mutex<RealView>)` / `Inner::Fake { tabs }`: the fake backend has no textures and no UI events (`drain_tab_closes` → empty). `RealView::new` spawns the `"mermaid-viewer-gui"` OS thread running `gui_loop`; `WindowController::show_tab` waits for a one-shot ack bounded by `GUI_ACK_TIMEOUT` (10 s) with `recover_dead_gui` on timeout; `Drop` sets `stopped` and `bounded_join`s with `GUI_SHUTDOWN_GRACE` (5 s). `gui_loop` waits CLOSED until a `ShowTab` (which initializes raylib and opens the window), then per frame drains queued commands non-blockingly (`run_drain`, stopping exactly at a latched close so a queued `ShowTab` survives for re-open) and steps the window; raylib's event pump runs inside `EndDrawing`.
- `crates/mermaid-viewer-core/src/view.rs` tests — `view_rejects_svg_image` currently codifies "the view rejects SVG" for `show_tab` and `run_blocking` alike; this blueprint deliberately flips that contract for the controller path. `drain_stops_at_latched_close_and_leaves_queued_show_tab` models the drain with local closures that must be updated with the new `Cmd::ShowTab` payload. The fake-backend cases (`fake_tab_lifecycle`, `fake_tab_replace`, `fake_tab_cap`, `fake_close_unknown_tab_is_noop`, `tab_close_index_updates_active`) build PNG fixtures through a local `test_png` helper.
- Build/entry — `crates/mermaid-viewer-core/Cargo.toml` already depends on `resvg 0.48` and `png 0.18` plus the two raylib configurations (both with `raygui`; the software-renderer build enables `SUPPORT_MODULE_RTEXT`, `SUPPORT_MODULE_RTEXTURES`, `SUPPORT_IMAGE_GENERATION`); there is no `image` crate. `crates/mermaid-viewer/src/main.rs`: `run_mcp` builds a multi-threaded tokio runtime; `serve_mcp` serves `Arc<DiagramServer>` over stdio and closes the window before exit; the CLI `view` renders PNG at `--scale` then `run_blocking` — this path is not to be changed.
- Tests — `crates/mermaid-viewer-core/tests/mcp.rs` (9 named cases, fake backend, the env var set once per process), `crates/mermaid-viewer-core/tests/view_headless.rs` (pixel tests serialized on a global raylib-init lock; drives `draw_frame`, `draw_tabbed_frame`, `load_window_texture`, and `Window::show_tab` under `view-headless`), `crates/mermaid-viewer/tests/mcp_e2e.rs` (spawns the real binary with the fake env on the child only; `e2e_initialize_lists_two_tools` asserts exact parameter names; `e2e_show_and_close_roundtrip` already sends `scale: 1.0`), plus `tests/cli.rs` and `tests/exit_codes.rs`.

### C. Intended design

#### Show path: SVG across the queue

`DiagramServer::show` renders `ImageFormat::Svg` only (mermaid-render parse + layout; no resvg on the caller's path) and passes the SVG `RenderedImage` plus the resolved initial scale hint through `commit_show` to `WindowController::show_tab(id, label, image, initial_scale)`. `Cmd::ShowTab` carries the SVG document (kilobytes of plain-data text) and the hint; `TabState.image` stores the SVG, whose logical `width`/`height` drive the fit math. The `scale` MCP parameter becomes the initial raster scale hint — a floor on the first rasterization's sharpness; the window re-rasterizes on demand as the user zooms. Rendering and the window/registry commit stay in `spawn_blocking` exactly as today.

#### Per-tab texture slots and the one-live-texture rule

`Window` replaces `textures: Vec<Texture2D>` with one texture slot per tab:

```rust
enum TextureSlot {
    /// No live texture: the active tab draws the placeholder.
    Empty,
    /// A live texture rasterized at `scale` (logical SVG px → raster px).
    Ready { texture: Texture2D, scale: f64 },
}
```

In-flight and pending raster requests are tracked per tab separately from the slot, so a re-raster keeps drawing the previous texture until the new one lands (last-known content); the placeholder shows only while there is no completed raster for the current content. Only the active tab is ever drawn, so only it holds a texture: switching the active tab frees the previous tab's texture (slot → `Empty`), and an `Empty` active tab gets an on-demand raster. **Invariant: at most one `Ready` slot per window at any instant.** The placeholder is minimal: the background clear plus the tab's label drawn centered with raylib's text drawing (rtext is enabled on both backend builds). The fit/zoom/pan layout always runs against the live texture's pixel dimensions (the PNG path's existing rule), which makes the on-screen rect invariant under the raster scale — a re-raster never moves or resizes the drawn diagram.

#### Raster worker thread

A dedicated `"mermaid-viewer-raster"` OS thread, spawned as a sibling of the GUI thread in `RealView::new` (real backend only; the fake backend is unaffected). Jobs and results cross `std::sync::mpsc` channels as plain data:

```rust
struct RasterJob    { id: String, generation: u64, svg: String, scale: f64 }
struct RasterOutcome { id: String, generation: u64,
                       result: std::result::Result<(Vec<u8>, u32, u32), Error> }
```

The worker runs pure-Rust resvg through the extracted raw-RGBA primitive using the existing `raster_options()` — no raylib types, no unsafe, never panics. The GUI thread is the only job submitter and the only result consumer; it performs every GL upload (raylib thread affinity) and drains the result channel non-blockingly every frame alongside `run_drain`. The worker's lifecycle mirrors the GUI thread: it stops on the shared `stopped` flag or when the job channel disconnects (the GUI thread's death drops the only job sender), `WindowController::drop` bounded-joins both threads within the existing grace, and the worker outlives window open/close cycles. A dead worker — pathological, since the rasterization never panics by contract — surfaces as typed `Error::View` acks for staged shows plus a stderr log; there is no mid-session respawn (the existing GUI-thread recovery path provisions a fresh worker with a fresh `RealView`).

#### Acknowledgement and failure semantics

`Cmd::ShowTab`'s one-shot ack channel carries typed `crate::error::Error` (not a bare `String`), so a rasterization failure reaches the MCP caller as `Error::Rasterize` through the existing ack path. The initial raster is part of the show's commit, and every `ShowTab` receives **exactly one ack, eventually**:

- `Ok` — the tab exists with the new content committed (label, SVG, zoom/pan reset); if the tab is active, a live `Ready` texture at the initial scale is uploaded (or will be within a frame if the tab was switched away mid-raster and back).
- `Err` — the show had no effect on the window: an added tab is removed, a replaced tab keeps its previous content; the message is typed (`Error::View` for capacity, format validation, window init, a wedged GUI thread, or a gone worker; `Error::Rasterize` for a failed SVG rasterization).

A staged show superseded by a newer show of the same tab, or cancelled by a tab close or window close while in flight, acks `Err` naming the cause. Zoom-driven re-rasters carry no ack: they are best-effort — a failure keeps the previous texture (or the placeholder) and logs once to stderr; no automatic retry within the same content generation (the next activation or content change retries).

#### Raster scale policy

The effective display scale `e` (logical SVG px → window px) is the fit of the tab's logical dimensions into the content rect (window minus the tab strip) × the tab's zoom, recomputed every frame. Every raster target is `e × RASTER_HEADROOM`, clamped so the output pixel count (`ceil(w·s) × ceil(h·s)`) stays ≤ `MAX_RASTER_PIXELS` — one live texture, so the bound is on a single texture. The show's initial target is `max(initial_scale hint, e × RASTER_HEADROOM)`; degenerate hints (non-finite, ≤ 0) are ignored — never an error. A `Ready` slot re-rasters when the slot deviates from `e` by more than `RASTER_HYSTERESIS` in either direction **and** `e` has been unchanged for `ZOOM_SETTLE` (wheel bursts and resize drags debounce identically; pan never touches the clock) **and** the clamped target differs from the slot's scale — a target that clamps to the slot's own scale is a no-op, the guard against re-raster loops at the cap. An `Empty` active tab rasterizes immediately on activation (no debounce), unless an in-flight job for the same generation already satisfies it. Coalescing: at most one in-flight job plus one pending target per tab; while a job is in flight a new crossing replaces the pending target, and on landing the pending fires at most once if it still deviates beyond hysteresis, else is cleared.

Constants: `RASTER_HEADROOM = 1.25`, `RASTER_HYSTERESIS = 1.5`, `ZOOM_SETTLE = 200 ms`, `MAX_RASTER_PIXELS = 16_777_216` (4096×4096 ≈ 64 MiB RGBA8).

#### What stays untouched

The CLI path (`run_blocking`, `step_cli`, `draw_frame`, `load_window_texture`) keeps its eager single-diagram PNG behavior. `NativeRenderer::render`'s PNG pipeline and `rasterize_png`'s observable behavior are unchanged (`rasterize_png` becomes a composition of the extracted raw-RGBA primitive plus the unchanged encode step). The MCP wire contract, the tab-ID registry rules, the tab cap, and the MCP layer's thread model are unchanged.

### D. Contract and compatibility map

**View contract (`mermaid-viewer-core::view`):**

- `WindowController::show_tab(&self, id: &str, label: &str, image: &RenderedImage, initial_scale: f64) -> Result<()>` — `image` must be `ImageFormat::Svg`; PNG (or any other format) is rejected with a typed `Error::View` before any backend work, in both CLOSED and OPEN states, leaving the window unchanged. The ack is bounded by `GUI_ACK_TIMEOUT` as today, with the outcome set defined above. (Signature change; the only consumer is `mcp.rs`, updated in the same milestone.)
- `Cmd::ShowTab { id, label, image (SVG), scale: f64, ack: Sender<std::result::Result<(), Error>> }` — plain data, internal to view.rs; its in-file test models are updated with it.
- `close_tab`, `close`, `is_open`, `drain_tab_closes`, `MAX_TABS`, `GUI_ACK_TIMEOUT` — unchanged. `run_blocking(title, image)` — unchanged, PNG-only. `load_window_texture(rl, thread, image)` — unchanged (the CLI path). `draw_frame` — unchanged.
- `draw_tabbed_frame(d, window, sw, sh, ui_closes) -> bool` — same signature; additionally draws the placeholder when the active tab's slot is `Empty`.
- New `#[doc(hidden)]` pub test seams on `Window` for the headless pixel pass: the staging show (`show_tab`, evolving from the current eager seam), the result-application seam (`apply_raster_result`: decides discard/commit/rollback via the generation rules, uploads on success, returns the ack outcome for the GUI loop to send), and `ready_slot_count() -> usize` (asserts the one-live-texture invariant).
- New crate-internal raster-scheduling state (per-tab generations, staging, in-flight/pending requests, the target policy) as pure functions, unit-tested under every feature pass. The worker thread and its channels are real-backend-only.

**Render contract (`mermaid-viewer-core::render`):**

- New crate-internal primitive `rasterize_rgba(svg: &str, scale: f64) -> Result<(Vec<u8>, u32, u32)>`: straight RGBA8, row-major, `len == w*h*4`; dimensions follow the PNG path's rule (`round(svg_size × scale)`, minimum 1 px per side); malformed SVG → `Error::Rasterize`; non-finite/non-positive scale clamped to 1.0. `rasterize_png` composes it with the unchanged PNG encode, so its observable behavior and tests are unchanged. The public `NativeRenderer`/`RenderOptions`/`RenderedImage` contracts are unchanged.

**MCP contract (`mermaid-viewer-core::mcp`):**

- Wire-identical: exactly the two existing tools with the same names, parameter names/types/optionality, statuses, and bodies. `ShowDiagramParams.scale`'s schema description changes to document the initial raster hint semantics; no other schema change. `DiagramServer::show` renders `ImageFormat::Svg` and passes the resolved hint (`DEFAULT_PNG_SCALE` renamed to `DEFAULT_INITIAL_RASTER_SCALE`, value 2.0 unchanged) through `commit_show` to `show_tab`. Render failures remain `Error::Render`; rasterization failures surface from the view ack as `Error::Rasterize` — both tool-level errors, never protocol errors, never panics. The registry rules are unchanged: an ID is gained only after the ack succeeds.

**CLI contract:** unchanged — `view`, `render`, and `mcp` syntax and semantics; the `view` path's PNG render + `run_blocking` are untouched.

**Build contract:** unchanged — no dependency, feature, or lint-policy changes; the headless pass compiles and tests the same window code.

### E. Milestone map

1. **M-1 — SVG show path with an eager, fit-aware raster on the GUI thread:** the queue carries SVG, the slot types and target-scale policy land, the controller's format contract flips (SVG required, PNG rejected), and the GUI thread rasterizes synchronously — the same threading model as today, so the intermediate state is fully buildable and testable.
2. **M-2 — Raster worker thread and on-demand texture slots:** the rasterization first moves off the GUI thread (worker, deferred acks, generations, coalescing), then the slots become on-demand (free on switch, activation raster, placeholder, one live texture).
3. **M-3 — Zoom-driven re-raster:** the per-frame trigger (hysteresis, settle, cap guard) on top of M-2's request machinery.
4. **M-4 — E2E, documentation, and final verification:** the `scale`-hint edge cases, the e2e schema-documentation check, README, and the full default + headless matrix.

Each milestone keeps the repository green: M-1 flips the whole show chain in one sub-milestone (the controller flip and its only consumer, `mcp.rs`, cannot be split without an unbuilt intermediate state); M-2's sub-milestones each leave the window fully functional; M-3 adds policy to existing machinery; M-4 is coverage and docs.

**Invariants:** no new dependencies and no new C FFI — the crate stays `deny(unsafe_code)` with the single documented `image_from_rgba` block, and the binary's `deny(unsafe_code)` is unchanged; the `view-headless` build compiles and tests the same window code paths; the MCP wire contract is unchanged (the same two tools with the same names, schemas, statuses, and bodies — only the documented meaning of `scale` shifts); channels carry plain data only — `RaylibThread` and every other raylib handle stays on the GUI thread, and the raster worker never sees a raylib type; one raylib initialization per process; the fake and real backends follow the same lifecycle and capacity rules; bad input never panics (typed errors everywhere); MCP stdout carries protocol frames only (worker and GUI diagnostics go to stderr); at most one live GPU texture per window; the CLI single-diagram path (`run_blocking`, `step_cli`, `draw_frame`, `load_window_texture`) keeps its eager PNG behavior.

---

## [M-1] SVG show path with an eager, fit-aware raster on the GUI thread

*The MCP show path sends SVG across the queue and the GUI thread rasterizes it synchronously at a display-aware scale (no worker yet). This flips the controller's format contract — SVG required, PNG rejected — while the CLI keeps its PNG-only path.*

### Contract card

- `render.rs`: `rasterize_rgba(svg, scale) -> Result<(Vec<u8>, u32, u32)>` (crate-internal, documented) is the resvg core of today's `rasterize_png` — usvg parse with `raster_options()`, `tiny_skia::Pixmap` at the clamped scale, `take_demultiplied()` straight RGBA8 — returning the raw buffer and dimensions. `rasterize_png` becomes `rasterize_rgba` plus the unchanged PNG encode, so PNG behavior and its existing tests are byte-identical.
- `view.rs`: `WindowController::show_tab(id, label, image, initial_scale)` requires `ImageFormat::Svg` (an `ensure_svg` check mirroring today's `ensure_png`): PNG is rejected with a typed `Error::View` naming the new rule, in CLOSED and OPEN states, leaving the window unchanged. `run_blocking` keeps `ensure_png` (PNG-only, unchanged). The fake backend applies the same format, add/replace, capacity, and close rules (it ignores the hint). The module docs' "window input is raster PNG only" wording is rewritten for the split (controller = SVG on demand; CLI = PNG eager).
- `Cmd::ShowTab` carries `{ id, label, image: RenderedImage (SVG), scale: f64, ack: Sender<std::result::Result<(), Error>> }`; the ack channel type changes from `String` to `Error` so typed failures propagate end-to-end. The ack is still sent synchronously, as today — the eager raster runs inside the acked call on the GUI thread (the known per-show frame stall that M-2 removes).
- `Window` gains per-tab `TextureSlot { Empty | Ready { texture, scale } }` replacing `textures: Vec<Texture2D>`. The raylib-side `show_tab` seam rasters eagerly on the GUI thread at the target scale and uploads raw RGBA8 through the existing `image_from_rgba` + `load_texture_from_image` path; the ordering stays capacity check → raster+upload → state show, preserving "a failed show leaves the window untouched".
- Raster-scale policy (pure, in view.rs): every target is `e × RASTER_HEADROOM` clamped to `MAX_RASTER_PIXELS`; the show's initial target is `max(initial_scale, e × headroom)` at show time (content rect = window minus `TAB_STRIP_HEIGHT`), with degenerate hints ignored. Constants `RASTER_HEADROOM = 1.25` and `MAX_RASTER_PIXELS = 16_777_216` are introduced here and reused by M-3.
- The fit/zoom/pan layout runs against the live texture's pixel dimensions (the PNG path's existing rule), making the on-screen rect invariant under raster scale; `draw_tabbed_frame` draws the active tab through its slot (no `Empty` tab can persist in this milestone — the raster is synchronous — so no placeholder yet).
- `mcp.rs`: `DiagramServer::show` renders `ImageFormat::Svg` in `spawn_blocking` (the `RenderOptions` value is immaterial for SVG — the hint travels as the `show_tab` argument); the resolved hint (`DEFAULT_INITIAL_RASTER_SCALE = 2.0`, the renamed `DEFAULT_PNG_SCALE`) is passed through `commit_show` to `show_tab`. `ShowDiagramParams.scale`'s field doc (the JSON schema description) and the module docs are updated to the initial-hint wording. The `show`/`close` doc comments, statuses, registry rules, and tool descriptions otherwise stay as they are.

### Verification plan

- Commands: `cargo test -p mermaid-viewer-core --lib render::`; `cargo test -p mermaid-viewer-core`; `cargo test -p mermaid-viewer-core --no-default-features --features view-headless`; `cargo build --workspace`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --check`.
- Named cases:
  - `rasterize_rgba_dimensions_and_stride` — a 20×20 SVG at scale 2 rasterizes to (40, 40) with a `w*h*4` straight-RGBA8 buffer, the same geometry `rasterize_png` reports.
  - `rasterize_rgba_straight_alpha` — a 50%-opacity red rect yields full-strength red with ~50% alpha in the raw buffer (no premultiplication, no PNG round-trip).
  - `controller_rejects_png_image` — supersedes `view_rejects_svg_image`: `show_tab` rejects PNG in CLOSED and OPEN states with a typed `Error::View`, the window state is unchanged by the rejection, and `run_blocking` still rejects SVG.
  - `raster_target_respects_hint_floor_and_cap` — the pure target: a hint below the fit-derived target loses to it, a higher hint wins, a huge diagram clamps to `MAX_RASTER_PIXELS`, and non-finite/negative hints are ignored (never an error).
  - `fake_tab_lifecycle`, `fake_tab_replace`, `fake_tab_cap`, `fake_close_unknown_tab_is_noop`, `tab_close_index_updates_active` — the existing cases updated to SVG fixtures (`test_svg` replacing `test_png`); semantics otherwise unchanged.
  - `drain_stops_at_latched_close_and_leaves_queued_show_tab` — updated for the new `Cmd::ShowTab` payload (latch semantics unchanged).
  - `headless_pixel_center`, `headless_pixel_corners_clear` — unchanged and passing: the CLI full-window frame still draws a PNG texture through `load_window_texture`.
  - `headless_tabbed_frame_active_content`, `headless_tab_bar_renders` — updated: tabs are shown as SVG and rasterized eagerly; the same pixel probes hold (the fit rect is raster-scale-invariant).
  - The nine `mcp_*` cases in `tests/mcp.rs` pass unchanged (fake backend, statuses unchanged, wire bodies identical).

### Target files

#### [M-1.a] Extract the raw-RGBA rasterization primitive

- [MODIFY] `crates/mermaid-viewer-core/src/render.rs`

#### [M-1.b] SVG across the show path (queue, controller, MCP, tests)

- [MODIFY] `crates/mermaid-viewer-core/src/view.rs`
- [MODIFY] `crates/mermaid-viewer-core/src/mcp.rs`
- [MODIFY] `crates/mermaid-viewer-core/tests/view_headless.rs`

### Completion checklist

- [ ] `show_diagram` sends SVG end-to-end; PNG input to the controller is a typed error; the CLI path is untouched and its tests are green.
- [ ] The eager raster uses the fit-aware, capped target; the shown diagram is sharp at fit with no PNG encode/decode round-trip.
- [ ] Default and headless passes, clippy, and fmt are green; the `scale` schema description documents the hint semantics.
- [ ] Manual desktop smoke: two MCP shows with different `scale` hints render sharp; `view` behaves exactly as before.

---

## [M-2] Raster worker thread and on-demand texture slots

*The rasterization moves off the GUI thread onto a dedicated worker (deferred acks, generations, coalescing, discard rules), then texture slots become on-demand: free on switch, activation raster, placeholder, one live texture.*

### Contract card

- `RasterJob { id, generation, svg, scale }` and `RasterOutcome { id, generation, result: Result<(Vec<u8>, u32, u32), Error> }` cross `std::sync::mpsc` channels as plain data; `worker_loop(jobs, results, stopped)` runs on the `"mermaid-viewer-raster"` thread: `recv_timeout` while idle (observing `stopped`), `crate::render::rasterize_rgba` per job with the outcome (failures as `Error::Rasterize`), never panics, and returns on disconnect or `stopped`.
- `RealView::new` spawns the worker as a sibling of the GUI thread and hands the GUI thread the job sender and result receiver; `RealView` keeps the worker `JoinHandle`. `WindowController::drop` sets `stopped` once and bounded-joins the GUI thread, then the worker (existing `bounded_join`/`GUI_SHUTDOWN_GRACE`); a wedged worker detaches exactly like a wedged GUI thread. The worker outlives window close/reopen cycles and dies with the job-channel disconnect when the GUI thread is re-spawned by `recover_dead_gui` (the fresh `RealView` provisions a fresh worker).
- The show path through the worker: the `Window::show_tab` seam stages the show (capacity check first; a provisional add for a new ID; a replace keeps the old content live until commit) and produces the `RasterJob` at the initial target — no rasterization or upload on the GUI thread. `Window::apply_raster_result(rl, thread, id, generation, result)` decides via the generation rules and either commits (content, label, zoom/pan reset; texture upload when the tab is active → `Ready`) or rolls back (add removed / replace keeps old), returning the ack outcome for the GUI loop to send.
- Staleness rules: a content commit requires the tab to still exist and the generation to match; the texture upload additionally requires the tab to be active; a zoom/activation raster for an inactive tab is discarded. Every `ShowTab` gets exactly one ack: `Ok` as defined globally; `Err` (typed, naming the cause) for a failed raster, a superseded staged show, or a staged show cancelled by a tab/window close; a worker-gone condition (job send failure or result-channel disconnect) acks staged shows `Err(Error::View(…))` and logs to stderr.
- `gui_loop`: a `ShowTab` in the CLOSED state initializes raylib and enters the OPEN frame loop with the staged tab — placeholder — without acking; in the OPEN state `run_drain`'s apply stages the show and defers the ack. A new non-blocking result drain runs every frame beside `run_drain`: outcomes are applied or discarded and deferred acks are sent there.
- Placeholder: `draw_tabbed_frame` draws, for an `Empty` active slot, the background clear plus the tab's label centered in the content area (rtext, both backends).
- On-demand slots: when the active tab changes (tab-bar selection or a show activating another tab), the previously active tab's `Ready` texture is dropped (slot → `Empty`) and its pending request state cleared (an in-flight job is left to land and be discarded by the inactive rule). An `Empty` active tab gets a raster job at the current target immediately (no debounce), unless an in-flight job for the same generation already satisfies it. **At most one `Ready` slot per window at any instant**, observable through the `ready_slot_count()` seam. In this milestone the only jobs are initial-show rasters and activation rasters (zoom re-rasters are M-3).

### Verification plan

- Commands: `cargo test -p mermaid-viewer-core`; `cargo test -p mermaid-viewer-core --no-default-features --features view-headless`; `cargo build --workspace`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --check`.
- Named cases:
  - `worker_loop_rasterizes_jobs` — a valid tiny-SVG job yields an `Ok` outcome with the expected dimensions; a malformed-SVG job yields an `Err(Error::Rasterize)` outcome; the loop never panics on bad input.
  - `worker_loop_stops_on_disconnect_and_flag` — dropping the job sender ends the loop; the `stopped` flag ends an idle loop without jobs.
  - `show_raster_failure_acks_typed_error` — a staged show whose raster fails acks `Err(Error::Rasterize)`: an added tab is removed, a replaced tab keeps its old content, and the window is otherwise unchanged.
  - `raster_result_discards_stale_outcomes` — a result for a removed tab, a superseded generation, or a raster of an inactive tab is discarded without side effects; a superseded or closed staged show acks `Err` naming the cause.
  - `drain_stops_at_latched_close_and_leaves_queued_show_tab` — updated to the deferred-ack model (a `ShowTab` no longer acks inside the drain; a latching close acks an in-flight staged show `Err`).
  - `coalescing_bounds_one_inflight_plus_pending` — at most one in-flight job plus one pending target per tab; the pending target replaces freely and fires at most once on landing.
  - `real_controller_spawn_and_drop_joins_threads` — under the headless pass with no display and no raylib init (no show sent): a real-backend controller spawns the GUI and raster threads and both join on drop within the grace period.
  - `headless_placeholder_while_empty` — an active tab with no live texture draws the placeholder: content-area background with the label's glyphs and no content pixels.
  - `headless_apply_raster_result_draws_content` — landing an `Ok` outcome through the seam uploads and draws the tab's content at the expected fit rect.
  - `headless_one_live_texture_across_switches` — across a show/switch/apply sequence, `ready_slot_count()` is always ≤ 1, the switched-away tab's slot is `Empty`, and the activated tab is rasterized on demand.
  - The existing fake-backend and MCP suites stay green (the fake path never touches the worker).

### Target files

#### [M-2.a] The raster worker thread (machinery, dormant)

- [MODIFY] `crates/mermaid-viewer-core/src/view.rs`

#### [M-2.b] The show path through the worker (deferred acks, placeholder)

- [MODIFY] `crates/mermaid-viewer-core/src/view.rs`

#### [M-2.c] On-demand slots (free on switch, activation raster)

- [MODIFY] `crates/mermaid-viewer-core/src/view.rs`
- [MODIFY] `crates/mermaid-viewer-core/tests/view_headless.rs`

### Completion checklist

- [ ] The first raster runs on the worker; the GUI thread never rasterizes and never blocks on the worker; every `ShowTab` is acked exactly once.
- [ ] A rasterization failure reaches the MCP caller as a tool-level `Error::Rasterize` message with no window or registry corruption; a rollback leaves the previous content in place.
- [ ] Switching tabs frees the inactive texture, the placeholder shows while empty, and one live texture holds at any instant.
- [ ] `WindowController::drop` joins both threads within the grace period; a dead GUI thread's recovery provisions a fresh worker.
- [ ] Default and headless passes, clippy, and fmt are green; manual desktop smoke: show, switch during a slow first raster, close a tab mid-raster.

---

## [M-3] Zoom-driven re-raster

*The window re-rasterizes the active tab as the user zooms (or resizes), at the displayed scale, debounced and capped, reusing M-2's request machinery.*

### Contract card

- Per-frame evaluation on the GUI thread (pure decision core, unit-tested): for the active tab with a `Ready` slot, compute the effective scale `e` (fit of the tab's logical dims into the content rect × the tab's zoom); deviation `d = max(e/slot_scale, slot_scale/e)`; submit a job at `target = clamp(e × RASTER_HEADROOM, …, MAX_RASTER_PIXELS)` iff `d > RASTER_HYSTERESIS (1.5)` **and** `e` has been unchanged for `ZOOM_SETTLE (200 ms)` **and** the clamped target differs from the slot's scale and any in-flight target (the no-duplicate guard, including at the cap). The settle clock resets whenever `e` changes (wheel or resize); pan never touches it.
- Coalescing from M-2 extends to these jobs: while one is in flight, a new crossing stores/replaces the tab's pending target; on landing the pending fires at most once if it still deviates beyond hysteresis, else is cleared. A generation bump from a new show discards any in-flight or pending re-raster.
- A zoom re-raster failure keeps the previous texture (slot stays `Ready` at the old scale) or the placeholder, logs once per generation to stderr, and clears the request state; no retry within the same content generation.
- On-screen rect invariance: the layout runs against the live raster's pixel dimensions, so a landing re-raster never moves or resizes the drawn diagram (headlessly asserted at two raster scales).

### Verification plan

- Commands: `cargo test -p mermaid-viewer-core`; `cargo test -p mermaid-viewer-core --no-default-features --features view-headless`; `cargo build --workspace`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --check`.
- Named cases:
  - `reraster_hysteresis_threshold` — within 1.5× slot deviation no job is submitted; beyond it exactly one job at the clamped target is.
  - `reraster_settles_after_scale_pause` — a scale change younger than 200 ms suppresses submission; aged past it the job fires; pan never suppresses or triggers.
  - `reraster_target_respects_pixel_cap` — a huge diagram's target clamps to `MAX_RASTER_PIXELS`; a target that clamps to the slot's own scale is not resubmitted (no re-raster loop at the cap).
  - `reraster_pending_coalesces_and_fires_once` — a crossing while a job is in flight stores one pending target (latest wins); the landing applies or clears it exactly once.
  - `headless_reraster_keeps_layout_rect` — the same SVG rasterized at two scales through the seams draws to the identical on-screen rect (same center probe, background corners).
  - The existing M-1/M-2 cases and the fake/MCP suites keep passing.

### Target files

#### [M-3.a] The per-frame re-raster trigger

- [MODIFY] `crates/mermaid-viewer-core/src/view.rs`
- [MODIFY] `crates/mermaid-viewer-core/tests/view_headless.rs`

### Completion checklist

- [ ] Zooming in and out re-rasterizes at the displayed scale without thrash (hysteresis + settle observable in the unit cases).
- [ ] The drawn diagram never moves or resizes when a re-raster lands; a huge zoom-in stays within the pixel cap.
- [ ] Default and headless passes, clippy, and fmt are green; manual desktop smoke: wheel-zoom a large diagram and watch sharpness catch up after the pause.

---

## [M-4] E2E coverage, documentation, and final verification

*Lock in the `scale`-hint semantics at the MCP surfaces and document the on-demand behavior; run the full matrix.*

### Contract card

- `tests/mcp.rs` gains the hint-edge case; `mcp_e2e.rs` gains the schema-documentation case; no production code changes. The README's MCP section documents: `scale` as the initial raster scale hint (floor on the first rasterization's sharpness; the window re-rasterizes on demand as you zoom), on-demand rasterization with the placeholder, the zoom-driven re-raster behavior, the single-live-texture rule with the 16,777,216-pixel (~64 MiB RGBA8) cap, and the raster worker — plus a refreshed non-goals note (wire contract and CLI unchanged).

### Verification plan

- Commands: `cargo build --workspace`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --check`; `cargo test --workspace`; `cargo test -p mermaid-viewer-core --no-default-features --features view-headless`.
- Named cases:
  - `mcp_show_diagram_scale_hint_edges` — explicit `scale` values 0.25 and 8.0 succeed with the normal statuses; negative and huge values are ignored (success, never an error, never a panic).
  - `e2e_scale_describes_initial_raster_hint` — the tools/list schema's `scale` description documents the initial raster hint semantics.
  - `e2e_initialize_lists_two_tools`, `e2e_show_and_close_roundtrip` (already sends `scale: 1.0`), `e2e_two_tabs_close_one`, `e2e_stdin_eof_exits_zero` — unchanged and passing: the wire contract is untouched.

### Target files

#### [M-4.a] Hint-edge and e2e coverage

- [MODIFY] `crates/mermaid-viewer-core/tests/mcp.rs`
- [MODIFY] `crates/mermaid-viewer/tests/mcp_e2e.rs`

#### [M-4.b] Documentation and final verification

- [MODIFY] `README.md`

### Completion checklist

- [ ] The `scale` documentation matches the implementation at every surface (schema description, code docs, README).
- [ ] The full default and headless matrix (build, clippy `-D warnings`, fmt, workspace tests, headless pass) is green.
- [ ] Manual desktop smoke across the whole feature: multi-tab show, placeholder on first raster and tab switch, zoom re-raster, cap behavior on a huge diagram, clean shutdown.

---

## Key decisions

1. **The controller takes SVG only; the CLI keeps PNG only.** `WindowController::show_tab` rejects PNG with a typed `Error::View` (the `view_rejects_svg_image` contract is deliberately flipped for the controller path — superseded by `controller_rejects_png_image`), because the MCP server is its only consumer and will only ever send SVG; keeping a PNG slot path would double the slot machinery for zero consumers. The fake backend mirrors the format rules (accepts SVG, rejects PNG) and ignores the hint; byte-level rasterizability is enforced by the raster step on the real path only — the same fake/real split the PNG-decode validity has today.
2. **The ack carries typed `Error`, and the initial raster is part of the show's commit.** `Cmd::ShowTab`'s one-shot ack channel changes from `String` to `Error` so a rasterization failure reaches the MCP caller as `Error::Rasterize` through the existing ack path. `Ok` means committed (with a live `Ready` texture if active); `Err` means no effect on the window or registry (an added tab removed, a replaced tab keeping its old content) — preserving today's "a failed show never corrupts the registry" rule verbatim. Every `ShowTab` is acked exactly once, including superseded and cancelled staged shows.
3. **`scale` is an initial-hint floor, superseded by zoom math.** The first raster targets `max(hint, e × RASTER_HEADROOM)`: the hint guarantees initial sharpness while the fit-derived target avoids a blurry first frame and a double raster; afterwards the per-frame zoom math governs. Degenerate hints (non-finite, ≤ 0) are ignored via `f64::max` semantics — never an error. Default 2.0 and the wire shape are unchanged.
4. **Concrete policy numbers.** `RASTER_HEADROOM = 1.25` (sharp for ~4 wheel notches in, ~2 out), `RASTER_HYSTERESIS = 1.5` (re-raster only beyond 1.5× deviation either direction), `ZOOM_SETTLE = 200 ms` (one debounce rule covers wheel bursts and resize drags; pan is exempt), `MAX_RASTER_PIXELS = 16_777_216` (4096×4096 ≈ 64 MiB RGBA8 — defensible for a single live texture), plus the no-duplicate-target guard that prevents re-raster loops when the cap clamps.
5. **Two-variant slot; no `Loading` variant.** `TextureSlot` is `Empty | Ready { texture, scale }`; in-flight/pending requests are tracked per tab separately. This gives last-known-content during re-rasters for free (the old texture stays `Ready` until the new one lands) and answers the placeholder question: blank-with-label only when no raster has completed for the current content; a re-raster never blanks the tab.
6. **Worker owned by `RealView` as a GUI-thread sibling; no mid-session respawn.** The worker stops on the shared `stopped` flag or the job-channel disconnect (the GUI thread's death drops the only sender), is bounded-joined in `WindowController::drop` like the GUI thread, and outlives window cycles. A dead worker surfaces as typed `Error::View` acks plus a stderr log; the existing `recover_dead_gui` path provisions a fresh worker whenever it re-creates the `RealView` — no extra respawn machinery for a pathological case.
7. **Layout always fits the live raster's pixel dimensions.** This is the PNG path's existing rule, generalized: the fit rect is provably invariant under the raster scale, so re-rasters never move the drawn diagram and the cap only softens pixels, never geometry.
8. **`rasterize_png` stays; `rasterize_rgba` is the shared primitive.** The worker and the eager M-1 path consume raw straight RGBA8 (no PNG encode/decode round-trip), while `rasterize_png` composes the primitive with the unchanged encode step — the `render` subcommand and `NativeRenderer::render(Png)` stay byte-identical.
9. **One live GPU texture at a time.** Freeing on switch is safe because only the active tab is ever drawn; it bounds GPU memory at one capped texture regardless of tab count (`MAX_TABS = 16` unchanged).
10. **The placeholder is minimal.** Background clear plus the tab's label centered, via rtext on both backends — enough to identify the tab and to pixel-test, nothing more.
