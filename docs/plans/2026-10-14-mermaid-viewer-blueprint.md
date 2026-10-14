# Blueprint — `mermaid-viewer`: Rust CLI + MCP diagram viewer (raylib GUI)

## Global Design Brief

### A. Objective and Non-Goals

**Outcome.** A Rust workspace at `/home/markku/git/mermaid_viewer` producing:
1. `mermaid-viewer-core` — library with three capabilities: Mermaid→SVG/PNG rendering, a raylib GUI window (blocking and long-lived/controller modes), and an MCP server exposing diagram tools.
2. `mermaid-viewer` — single CLI binary using the library, with subcommands:
   - `view` — render to PNG and show a GUI window (blocks until the window is closed); input from `FILE`, `--stdin`, or `--code`.
   - `render` — write SVG or PNG to a file or stdout; same input sources.
   - `mcp` — run an MCP server over stdio for coding agents.

**Agent-approach decision (the "to be decided" item): ship both, with MCP as the agent-facing path.** A shell `view` call blocks the agent's shell until the human closes the window, which is the wrong shape for an agent loop. The MCP server keeps a persistent window alive for the whole session: `show_diagram` returns instantly, the agent can iteratively update the same window, and `close_diagram` ends it. The CLI remains for humans and one-shot pipelines.

**Non-goals:** no network transport (stdio MCP only); no Mermaid syntax beyond what the chosen renderer supports; no macOS CI target (raylib supports it, but CI = ubuntu + windows); no server for multiple simultaneous windows; no Node/Chromium runtime dependency of any kind.

### B. Existing System Evidence

The repository is empty (only `.git/`) — this is greenfield; all conventions below are established by this blueprint. Selections below are evidence-based from crate research performed during planning:

- **`mermaid-render` 0.10.0** (crates.io, MIT, github.com/alitrack/mermaid-render): "Pure Rust Mermaid diagram parser and SVG renderer — zero JS, zero Node.js". API (verified from `src/lib.rs`): `parse_mermaid(src) -> Result<MermaidDiagram, _>`, `render_diagram(&diagram, &DiagramStyle, &mut impl TextMeasure) -> Result<(svg, w, h), _>`; `TextMeasure` has a stock `EstimatedMeasure`. Covers 10 diagram types (flowchart, sequence, class, ER, state, gantt, pie, timeline, mindmap, gitgraph). Single dep `dagre` (Sugiyama layout). **Risk disclosed:** crate is young (June 2026, low download count); mitigation is the isolated `NativeRenderer` wrapper plus typed error surfacing — unsupported input yields `Err(Error::Render)`, never a panic.
- **`resvg` 0.48.1** (linebender, 30M+ downloads): pure-Rust SVG→pixel rasterizer; gives us PNG from `mermaid-render`'s SVG with a known scale factor.
- **`raylib` 6.0.0 crate** (raylib-rs): safe bindings, MSRV Rust **1.88**, default feature `opengl_33`. Build deps: `cmake` + platform graphics dev headers (Windows: MSVC tools; Linux: X11/GL dev libs). Critical facts shaping the design: `RaylibThread` is `!Send` (GUI must live on one dedicated thread; controller communicates via plain-data channels); raylib init is single-per-process; and the **`software_renderer` feature** (mutually exclusive with `opengl_33`) provides a headless rlsw Memory platform plus a `test_harness` (`with_headless`, `render_frame`, `pixel_at`, `assert_pixel`) — this is how GUI drawing is validated by automated tests in CI.
- **`rmcp` 3.5.1** (official MCP Rust SDK, MCP spec 2026-07-28, tokio-based): `cargo add rmcp --features server`; stdio transport is `(tokio_io::stdin(), tokio_io::stdout())`; `service.serve(transport).await?`; tools via `#[tool]`/`#[tool_router]` macros with serde + `rmcp::schemars`.
- **`clap` v4** (derive) — standard choice for CLI parsing.

### C. Intended Design

- **Rendering pipeline (one source of truth):** `mermaid source → [mermaid-render] → SVG (+w,h) → [resvg @ scale] → PNG`. `NativeRenderer` (unit struct) is the only renderer; a `Renderer`-agnostic struct layout keeps an optional `mmdc`/subprocess backend addable later without touching consumers, but it is **not** in scope.
- **View design:** a dedicated GUI thread owns the raylib handle+thread (enforcing `!Send`). `WindowController` (spawn/update/close/is_open) talks to that thread over an `std::sync::mpsc` channel carrying plain data (PNG bytes + commands). Two façades: `run_blocking(title, image)` — current-thread event loop for CLI `view`; the controller — for the MCP server whose main thread runs tokio. Window features: fit-to-window initial scale, mouse-wheel zoom, drag pan, close on ESC/window-close, texture hot-swap on update. A `MERMAID_VIEWER_FAKE_WINDOW=1` environment variable selects a headless fake backend with the identical state machine, so the full MCP path is testable in CI without a display.
- **Headless test seam for raylib:** core carries the raylib crate twice under renamed deps — `raylib-ogl` (default feature `view`) and `raylib-sw` (feature `view-headless`, `default-features = false` + `software_renderer` + required `SUPPORT_MODULE_*`). A `compile_error!` forbids both at once. CI runs a second test pass under `view-headless` using raylib-rs's `test_harness` to assert pixels — the GUI is genuinely validated automatically, on both target OSes.
- **MCP:** `DiagramServer` implements the rmcp `ServerHandler` (or `#[tool_router]` equivalent — builder's choice per docs.rs/rmcp/3.x) in core; the CLI `mcp` subcommand only constructs it, serves stdio, and manages the tokio runtime and Ctrl-C/EOF shutdown. Tool logic lives in directly-callable functions so unit tests drive it without an MCP transport; the e2e test speaks raw newline-delimited JSON-RPC to a spawned child process.
- **Rejected alternatives:** (1) `mmdc`/mermaid-cli subprocess — runtime Node + headless Chromium dependency, breaks "minimal deps" and headless CI; (2) embedded JS VM (rusty_v8/boa) for mermaid.js — huge binaries, mermaid.js is DOM-dependent; (3) feeding SVG straight to raylib's SVG loader — inconsistent with the export path; PNG via resvg gives identical pixels and known dimensions everywhere; (4) a separate `mermaid-viewer-mcp` binary — one binary with a subcommand is a simpler PATH/install story for agents and keeps the server logic in the library.

**Invariants every milestone preserves:** no panics on bad diagram input (typed `Error`); the CLI never prints diagram data to stderr; render is synchronous and pure (no I/O, no threads) so it's trivially testable; exactly one raylib init per process; window updates serialize through the channel.

### D. Contract and Compatibility Map

New crate, no existing consumers. These contracts are fixed for the builder:

**Workspace** — `Cargo.toml` at root: members `crates/mermaid-viewer-core`, `crates/mermaid-viewer`; `workspace.package.rust-version = "1.88"`, `edition = "2021"`.

**Core dependencies (all declared up front in M-1.b):** `mermaid-render = "0.10"`, `resvg = "0.48"`, `thiserror = "2"`, `raylib-ogl = { package = "raylib", version = "6.0" }` (optional), `raylib-sw = { package = "raylib", version = "6.0", default-features = false, features = ["software_renderer", "SUPPORT_MODULE_RSHAPES", "SUPPORT_MODULE_RTEXT", "SUPPORT_MODULE_RTEXTURES", "SUPPORT_IMAGE_GENERATION"] }` (optional), `rmcp = { version = "3.5", features = ["server"] }` (optional), `tokio = { version = "1", features = ["rt", "rt-multi-thread", "macros", "sync", "io-std", "signal"] }` (optional), `serde = { version = "1", features = ["derive"] }` (optional).
Core features: `default = ["view", "mcp"]`; `view = ["dep:raylib-ogl"]`; `view-headless = ["dep:raylib-sw"]`; `mcp = ["dep:rmcp", "dep:tokio", "dep:serde"]`. Both `view` + `view-headless` enabled ⇒ `compile_error!`.

**Binary dependencies:** `mermaid-viewer-core = { path = "../mermaid-viewer-core" }` (default features), `clap = { version = "4", features = ["derive"] }`, `tokio = { version = "1", features = ["rt-multi-thread", "macros", "io-std", "signal"] }`.

**Render contract (public API, `mermaid-viewer-core::render`):**
```rust
pub enum ImageFormat { Svg, Png }
pub struct RenderedImage { pub format: ImageFormat, pub width: u32, pub height: u32, pub bytes: Vec<u8> }
#[derive(Clone, Copy)] pub struct RenderOptions { pub png_scale: f64 }   // Default { png_scale: 2.0 }
pub struct NativeRenderer;
impl NativeRenderer {
    pub fn new() -> Self;
    pub fn render(&self, source: &str, format: ImageFormat, opts: &RenderOptions) -> Result<RenderedImage>;
}
```
Behavior: SVG output is a valid `<?xml?>…<svg width height>…</svg>` document; PNG bytes are a decodable PNG whose IHDR w/h equal `round(svg_w × png_scale)` / `round(svg_h × png_scale)`; empty/whitespace source and any source the parser rejects ⇒ `Err(Error::Render)` (never panic, never an "error SVG" passed as `Ok`).

**Error contract (`mermaid-viewer-core::error`):**
```rust
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("mermaid render failed: {0}")] Render(String),
    #[error("svg rasterization failed: {0}")] Rasterize(String),
    #[error("view failed: {0}")] View(String),
    #[error(transparent)] Io(#[from] std::io::Error),
}
pub type Result<T> = std::result::Result<T, Error>;
```

**View contract (`mermaid-viewer-core::view`):**
```rust
pub fn run_blocking(title: &str, image: &RenderedImage) -> Result<()>;   // current thread; returns on window close
pub struct WindowController;   // opaque; Send + Sync
impl WindowController {
    pub fn spawn(title: &str) -> Result<Self>;
    pub fn update(&self, image: &RenderedImage) -> Result<()>;  // opens window on first call, swaps content after
    pub fn close(&self);                                          // idempotent
    pub fn is_open(&self) -> bool;
}
```
State machine (real and fake backends identical): CLOSED → `update` → OPEN, response semantics "opened"; OPEN → `update` → "updated"; any state → `close` → CLOSED. `MERMAID_VIEWER_FAKE_WINDOW=1` selects the fake backend (no display, no raylib init; state only).

**MCP contract (`mermaid-viewer-core::mcp`):** server name `mermaid-viewer`, stdio transport, tools exactly:
- `show_diagram` — params `{ code: string (required), title: string?, scale: number? }` → success body `{"status":"opened"}` / `{"status":"updated"}`; render failure → MCP tool error result (isError), server stays healthy.
- `close_diagram` — params `{}` → `{"closed":true}` (idempotent).
`pub struct DiagramServer; impl DiagramServer { pub fn new() -> Self; }` implementing the rmcp server handler; served via rmcp `ServiceExt::serve`. Tool logic must live in directly-callable async functions (exact rmcp macro/trait wiring: builder's choice against docs.rs/rmcp 3.x).

**CLI contract (binary `mermaid-viewer`):**
```
mermaid-viewer view    [FILE]  [-c|--code <SRC>] [--stdin] [-t|--title <T>] [-s|--scale <F>]
mermaid-viewer render  [FILE]  [-c|--code <SRC>] [--stdin] [-o|--output <P>] [-f|--format svg|png] [-s|--scale <F>]
mermaid-viewer mcp
```
Defaults: `view` title `"Mermaid Diagram"`, scale `2.0`; `render` output `-` (stdout), format `svg`, scale `2.0` (applies to PNG only). Input sources are mutually exclusive; exactly one required (missing ⇒ clap usage error). Exit codes: `0` success; `1` runtime/render/IO failure (message on stderr); `2` usage error.

**Build/CI contract:** `cargo build --workspace`, `cargo test --workspace`, `cargo test -p mermaid-viewer-core --no-default-features --features view-headless`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check` all green on ubuntu-latest and windows-latest (cmake + platform graphics dev packages installed in CI).

### E. Milestone Map

1. **M-1 Workspace skeleton + CLI shell** — three skeleton sub-milestones; the stub binary compiles and the full clap surface (parse + input-source validation + exit codes) is fixed and unit-tested early, so later milestones only fill in handler bodies without touching the contract.
2. **M-2 Native renderer + `render`** — the complete file→SVG/PNG→file/stdout pipeline is real and fully tested; GUI not required, so this milestone validates on any machine.
3. **M-3 View module + `view`** — raylib window with headless pixel verification; after this, `view` is usable on POSIX and Windows.
4. **M-4 MCP server** — the agent path; unit-tested in-process and e2e-tested over real stdio JSON-RPC with the fake-window backend.
5. **M-5 Cross-platform CI + docs** — locks the build contract on both OSes and documents agent (MCP config) and human (CLI) usage.

Each gate leaves the repo buildable and testable; the only forward dependency is stub handler bodies returning a clean "not implemented" `Error` message until their milestone lands.

---

## [M-1] Workspace skeleton and CLI shell
*Establishes the workspace, crate layout, error type, and the complete clap surface with validated input handling — the fixed contract every later milestone fills in.*

#### Contract Card
- Workspace + member paths per Global Brief D ("Workspace", "Core dependencies", "Binary dependencies").
- `Error` enum + `Result` alias per D ("Error contract") — full definition lands in this milestone.
- CLI surface per D ("CLI contract") — full flag surface, mutual-exclusion, and exit codes are defined and tested **in this milestone** (handler bodies stubbed).
- Core `lib.rs` re-exports: `pub mod error;` (later milestones append `pub mod render;`, `pub mod view;`, `pub mod mcp;` — one line each).
- `builder's choice: <stub error message wording for unimplemented subcommands>`

#### Verification Plan
- **Commands:** `cargo build --workspace`; `cargo test --workspace`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --check`.
- **Named test cases:**
  - `cli_parses_view_args` — given argv `["view", "d.mmd", "--scale", "3"]` when parsed then `Command::View{ file: Some("d.mmd"), scale: 3.0, … }` with defaults filled (title = "Mermaid Diagram").
  - `cli_parses_render_defaults` — given argv `["render"]` when parsed then `output = "-"`, `format = Svg`, `scale = 2.0`.
  - `cli_rejects_multiple_sources` — given `["render", "f.mmd", "--code", "x"]` when parsed then clap usage error (exit 2 semantics).
  - `cli_rejects_no_source` — given `["view"]` when parsed then usage error.
  - `error_display_messages` — given each `Error` variant when displayed then stable prefix (`"mermaid render failed: "`, `"svg rasterization failed: "`, `"view failed: "`) appears.
- **Definition of done:** workspace builds; all M-1 tests pass; `mermaid-viewer -h` and each subcommand's `-h` render the full flag surface; `mermaid-viewer view` with any valid input exits 1 with the unimplemented-stub message on stderr (no panic).

#### [M-1.a] Workspace root
- **Intent:** Create the workspace root and a README stub so every subsequent path resolves and builds.
- **Target Files:**
  - `[CREATE]` `Cargo.toml`
  - `[CREATE]` `README.md`
- **Requirements:**
  - `Cargo.toml` — workspace with the two members, `workspace.package` pins (`rust-version = "1.88"`, `edition = "2021"`); no root package.
  - `README.md` — project name, one-paragraph description, placeholder "Building" and "Usage" sections (full content lands in M-5); must not invent flags not in the CLI contract.

#### [M-1.b] Core crate skeleton
- **Intent:** Scaffold `mermaid-viewer-core` with the complete dependency/feature matrix and the error contract.
- **Target Files:**
  - `[CREATE]` `crates/mermaid-viewer-core/Cargo.toml`
  - `[CREATE]` `crates/mermaid-viewer-core/src/lib.rs`
  - `[CREATE]` `crates/mermaid-viewer-core/src/error.rs`
- **Requirements:**
  - `Cargo.toml` — all dependencies and features exactly per contract card (deps optional behind `view`/`view-headless`/`mcp`; `default = ["view", "mcp"]`); the `compile_error!` guard lives in `lib.rs`.
  - `lib.rs` — `pub mod error;` only; crate-level docs; the mutual-exclusion `compile_error!` for `view` + `view-headless`.
  - `error.rs` — the `Error` enum and `Result` alias exactly as contracted; `thiserror` attributes give the message prefixes.

#### [M-1.c] Binary crate skeleton with full clap surface
- **Intent:** Fix the entire CLI contract (flags, validation, exit codes) up front; dispatch to stub handlers.
- **Target Files:**
  - `[CREATE]` `crates/mermaid-viewer/Cargo.toml`
  - `[CREATE]` `crates/mermaid-viewer/src/cli.rs`
  - `[CREATE]` `crates/mermaid-viewer/src/main.rs`
- **Requirements:**
  - `cli.rs` — clap v4 derive types for the full contracted surface: `Command` enum (`View`, `Render`, `Mcp`), input-source group with `conflicts_with`, per-subcommand defaults per contract; exposes a `parse<I: Iterator<Item = String>>(args: I) -> Result<Command, clap::Error>`-style seam for testing (builder's choice: exact seam shape).
  - `main.rs` — maps clap errors to exit 2, runtime `Error` to exit 1 (message on stderr), success to 0; each subcommand currently returns a clearly-wrapped "not implemented (planned in M-x)" `Error` so processes exit 1 cleanly; source reading (file/stdin/code) is implemented for real here in a `read_source(&Command) -> Result<String>` helper because M-2 depends on it, with `Io` mapped from `std::io::Error`.
  - `Cargo.toml` — binary dependencies per contract; `mermaid-viewer-core` with default features.
- **Acceptance examples:** `echo "graph TD\n A-->B" | mermaid-viewer render --stdin` → exit 1 + "not implemented" text on stderr (render lands in M-2); `mermaid-viewer render a.mmd --code x` → exit 2 + clap message.

#### Completion Checklist
- [ ] `cargo build --workspace` clean on Linux
- [ ] All M-1 named tests pass; clap surface matches the CLI contract verbatim
- [ ] Exit codes 0/1/2 behave per contract; no source code beyond the three crates + root files

---

## [M-2] Native renderer and `render` subcommand
*Delivers the complete Mermaid→SVG/PNG pipeline (mermaid-render + resvg) behind `NativeRenderer`, wired end-to-end through `mermaid-viewer render`.*

#### Contract Card
- `render` module public API per Global Brief D ("Render contract") — `ImageFormat`, `RenderedImage`, `RenderOptions`, `NativeRenderer::render`, all behavior clauses (valid SVG, PNG IHDR dimensions at scale, `Err(Error::Render)` on rejected/empty input).
- PNG dimension rule: `width = round(svg_w × png_scale)`, `height = round(svg_h × png_scale)` (minimum 1 px).
- CLI `render` behavior per D ("CLI contract"): `-o -` writes raw bytes to stdout (no extra output on stdout — diagnostics go to stderr); a path target is created with `std::fs::File` (no parent-dir creation); `--scale` ignored for SVG.
- `builder's choice: <PNG encoding mechanism — resvg pixel buffer vs the `png` crate — and resvg Options tuning>`; `builder's choice: <which exact fixture sources mermaid-render 0.10 rejects, used for the error tests>`

#### Verification Plan
- **Commands:** `cargo test -p mermaid-viewer-core --test render`; `cargo test -p mermaid-viewer --test cli -- render`; `cargo build --workspace`; clippy/fmt as in M-1.
- **Named test cases:**
  - `render_flowchart_svg` — given `graph TD\n  A-->B` when rendered to `Svg` then output starts with `<?xml` or `<svg`, contains node labels `A` and `B`, and `width`/`height` > 0.
  - `render_png_dimensions_follow_scale` — given the same diagram at `png_scale = 2.0` when rendered to `Png` then bytes start with the PNG signature and IHDR (offsets 16..24, big-endian u32) equals 2× the SVG dimensions; at scale 1.0 equals 1×.
  - `render_invalid_source_is_error` — given a source mermaid-render 0.10 rejects when rendered then `Err(Error::Render(msg))` with non-empty msg (no panic).
  - `render_empty_source_is_error` — given `""` and `"   \n"` when rendered then `Err`.
  - `render_supported_types_smoke` — given one fixture each for all 10 supported diagram types (sequence, class, er, state, gantt, pie, timeline, mindmap, gitgraph, flowchart) when rendered to `Svg` then all `Ok` with non-empty SVG.
  - `default_options_scale_is_2` — given `RenderOptions::default()` then `png_scale == 2.0`.
  - `cli_render_svg_to_file` — given a temp file with a flowchart when running `render` with `-o <tmp>` then file exists, contains `<svg`, stdout empty, exit 0.
  - `cli_render_png_to_stdout` — given `--code 'graph TD\n A-->B' --format png` when run with `-o -` then stdout bytes start with the PNG signature, exit 0.
  - `cli_render_stdin` — given piped stdin source when run with `--stdin` then exit 0 and valid SVG on stdout.
  - `cli_render_bad_input_exits_1` — given `--code '!!!not mermaid!!!'` when run then exit 1, stderr contains `mermaid render failed`, stdout empty.
- **Definition of done:** `mermaid-viewer render` is fully functional for file/stdin/code → SVG/PNG → file/stdout; all named tests pass; core compiles with `--no-default-features` (render has no feature-gated deps).

#### [M-2.a] `NativeRenderer` in core
- **Intent:** Implement and test the entire rendering pipeline in the library before any wiring.
- **Target Files:**
  - `[CREATE]` `crates/mermaid-viewer-core/src/render.rs`
  - `[CREATE]` `crates/mermaid-viewer-core/tests/render.rs`
  - `[MODIFY]` `crates/mermaid-viewer-core/src/lib.rs`
- **Requirements:**
  - `render.rs` — exactly the contracted public API; wraps `mermaid-render` (parse + layout + SVG) and, for PNG, rasterizes the SVG with `resvg` at the contracted scale; converts upstream error strings into `Error::Render` / `Error::Rasterize` (rasterization failures must be distinguishable from parse failures); no I/O, no threads, synchronous.
  - `lib.rs` — append `pub mod render;` + re-export of the public render items at crate root.
  - `tests/render.rs` — the named core tests (flowchart SVG, PNG dimensions at 1× and 2×, invalid, empty, 10-type smoke, default scale).
- **Acceptance examples:** `NativeRenderer::new().render("graph TD\n  A-->B", ImageFormat::Png, &RenderOptions { png_scale: 3.0 })` → `Ok(RenderedImage { width = 3×svg_w, … })`.

#### [M-2.b] Wire `render` subcommand
- **Intent:** Make the CLI `render` path real, end-to-end, with process-level tests.
- **Target Files:**
  - `[MODIFY]` `crates/mermaid-viewer/src/main.rs`
  - `[CREATE]` `crates/mermaid-viewer/tests/cli.rs`
- **Requirements:**
  - `main.rs` — `render` handler: `read_source` → `NativeRenderer` → write bytes (stdout vs file per contract); preserves exit-code contract; `view`/`mcp` remain stubs.
  - `tests/cli.rs` — the named CLI tests (svg-to-file, png-to-stdout, stdin, bad-input exit 1), driving the built binary via `std::process::Command` (builder's choice: tempdir handling).

#### Completion Checklist
- [ ] `NativeRenderer` public API matches the contract card verbatim
- [ ] All M-2 named tests pass, including the 10-type smoke
- [ ] `mermaid-viewer render` round-trips file/stdin/code → svg/png → file/stdout with contracted exit codes

---

## [M-3] View module and `view` subcommand
*Adds the raylib window (blocking + controller modes, real/fake backends) with headless pixel verification, and wires `mermaid-viewer view`.*

#### Contract Card
- `view` module public API per Global Brief D ("View contract") — `run_blocking`, `WindowController::{spawn, update, close, is_open}`, state machine, `MERMAID_VIEWER_FAKE_WINDOW=1` fake backend with identical observable state.
- Renderer input to the window is `RenderedImage` (PNG) — `run_blocking`/`update` accept PNG; SVG input to the window is rejected with `Err(Error::View)`.
- Core feature wiring per D: `view` ↔ `raylib-ogl` (default), `view-headless` ↔ `raylib-sw`; headless test binary is `tests/view_headless.rs` compiled to empty unless `view-headless` is set.
- `builder's choice: <zoom/pan/fit interaction model and per-frame draw math>`; `builder's choice: <texture swap strategy on update>`

#### Verification Plan
- **Commands:** `cargo test -p mermaid-viewer-core` (default features: fake-backend + layout tests); `cargo test -p mermaid-viewer-core --no-default-features --features view-headless` (headless pixel tests); `cargo build --workspace`; clippy/fmt.
- **Named test cases:**
  - `fake_window_lifecycle` — given `MERMAID_VIEWER_FAKE_WINDOW=1` with a spawned controller when `update`, `update`, `close`, `update` happen then `is_open` sequence is `true, true, false, true` and no display/GL init occurred (test passes in a display-less container).
  - `fit_layout_math` — given a 100×50 image in a 800×600 window at zoom 1.0 when computed then draw rect is centered, scaled to fit without overflow, aspect preserved (pure function, no raylib).
  - `headless_pixel_center` — given an 8×8 solid-magenta PNG (produced in-test by `NativeRenderer` rasterizing a magenta-rect SVG) when the headless harness renders one frame then `pixel_at(center) == magenta` (± tolerance per raylib-rs `assert_pixel`).
  - `headless_pixel_corners_clear` — given the same frame when sampled then four corners equal the clear color, proving the image is drawn as a contained rect not stretched full-screen.
  - `view_rejects_svg_image` — given an `ImageFormat::Svg` `RenderedImage` when `update` called then `Err(Error::View)`.
  - `cli_view_not_implemented_replaced` — given no display in CI: no `view` e2e (covered by fake-backend + headless tests); the CLI dispatch compiles and `view -h` is correct.
- **Definition of done:** both headless and default test passes green; window code paths (init/texture/draw/frame loop) are exercised by pixel assertions; `mermaid-viewer view` dispatches to `run_blocking` (manual smoke on a desktop: window shows the diagram, ESC closes, exit 0).

#### [M-3.a] View module in core
- **Intent:** Implement the window, the controller thread, backends, and the headless test seam.
- **Target Files:**
  - `[CREATE]` `crates/mermaid-viewer-core/src/view.rs`
  - `[CREATE]` `crates/mermaid-viewer-core/tests/view_headless.rs`
  - `[MODIFY]` `crates/mermaid-viewer-core/src/lib.rs`
- **Requirements:**
  - `view.rs` — the contracted API; per feature cfg, alias the active bindings (`raylib_ogl` / `raylib_sw`) as the single `raylib` name used in code; real backend: dedicated OS thread owning raylib init + `RaylibThread` (never crosses threads), mpsc channel of plain-data commands, per-frame event pump (wheel zoom, drag pan, ESC/close), texture hot-swap on update, re-openable after close (raylib re-init on the same thread); fake backend: state machine only, selected by the contracted env var at `spawn`; `run_blocking` = single-call convenience over the same draw-frame logic on the current thread.
  - `lib.rs` — append `pub mod view;` behind `#[cfg(feature = "view")]`/`#[cfg(feature = "view-headless")]` (module compiles under either), re-exports guarded the same way.
  - `tests/view_headless.rs` — `#![cfg(feature = "view-headless")]`; the two `headless_pixel_*` tests using raylib's `test_harness`; the fake-window and layout tests live here too only if they require the headless feature, otherwise they must also run in the default pass (builder's choice: dedupe without weakening coverage).
- **Acceptance examples:** `WindowController::spawn("d")` + `update(png)` twice + `close()` under `MERMAID_VIEWER_FAKE_WINDOW=1` → `is_open()` true/true/false, all `Ok`.

#### [M-3.b] Wire `view` subcommand
- **Intent:** Complete the human GUI path in the CLI.
- **Target Files:**
  - `[MODIFY]` `crates/mermaid-viewer/src/main.rs`
- **Requirements:**
  - `main.rs` — `view` handler: `read_source` → `NativeRenderer` (PNG, `--scale`) → `run_blocking(--title, image)`; exit-code contract preserved; render failure exits 1 before any window/GL init; `mcp` remains the only stub.

#### Completion Checklist
- [ ] `view` public API matches the contract card; `!Send` thread-token never leaves the GUI thread
- [ ] Default + headless test passes green, including both pixel tests
- [ ] `mermaid-viewer view` shows the diagram on a desktop; ESC/window-close exits 0

---

## [M-4] MCP server
*Delivers the agent-facing path: `DiagramServer` with `show_diagram`/`close_diagram` tools, unit-tested in-process and e2e-tested over real stdio JSON-RPC.*

#### Contract Card
- MCP contract per Global Brief D ("MCP contract"): server name `mermaid-viewer`; `show_diagram { code, title?, scale? } → {"status":"opened"|"updated"}`; `close_diagram → {"closed":true}`; render failure ⇒ tool error result, server survives.
- `DiagramServer` owns a lazily-created `WindowController` (created on first `show_diagram`; `close_diagram` closes it); render happens via `NativeRenderer` inside `tokio::task::spawn_blocking`.
- `mcp` subcommand: tokio runtime on the main thread, stdio transport `(stdin(), stdout())`, clean shutdown on EOF/Ctrl-C (window closed before exit); nothing except MCP protocol frames may be written to stdout.
- E2e tests set `MERMAID_VIEWER_FAKE_WINDOW=1` **on the child process environment only**.
- `builder's choice: <rmcp 3.x ServerHandler vs #[tool_router] wiring and exact tool-code generation>`; `builder's choice: <policy for concurrent in-flight show_diagram updates (en queue order vs latest-wins — responses must still follow the state machine)>`

#### Verification Plan
- **Commands:** `cargo test -p mermaid-viewer-core --test mcp`; `cargo test -p mermaid-viewer --test mcp_e2e`; `cargo build --workspace`; clippy/fmt.
- **Named test cases:**
  - `mcp_show_diagram_opens` — given a fresh `DiagramServer` (fake window) when `show_diagram` is invoked with a flowchart then tool result body `{"status":"opened"}` and controller open.
  - `mcp_show_diagram_updates` — given an open window when `show_diagram` invoked again then `{"status":"updated"}`.
  - `mcp_close_diagram_idempotent` — given open, then closed state when `close_diagram` invoked twice then both return `{"closed":true}`, and a subsequent `show_diagram` returns `"opened"`.
  - `mcp_show_diagram_render_error` — given rejected mermaid source when `show_diagram` invoked then tool result is an error (isError) containing the render message, and a following valid call returns `"opened"` (server healthy).
  - `e2e_initialize_lists_two_tools` — given a spawned `mermaid-viewer mcp` child (fake window env) when `initialize` + `tools/list` are sent then serverInfo name is `mermaid-viewer` and tool set is exactly `{show_diagram, close_diagram}`.
  - `e2e_show_and_close_roundtrip` — given the initialized child when `tools/call show_diagram` (flowchart) then `tools/call close_diagram` are sent then success bodies `{"status":"opened"}` and `{"closed":true}`.
  - `e2e_stdin_eof_exits_zero` — given a running child when stdin is closed then process exits 0 within a bounded timeout.
- **Definition of done:** both test binaries green; no non-protocol bytes on the child's stdout; unit tests pass without any transport (direct function calls).

#### [M-4.a] `DiagramServer` in core
- **Intent:** Implement and unit-test the MCP handler logic in the library.
- **Target Files:**
  - `[CREATE]` `crates/mermaid-viewer-core/src/mcp.rs`
  - `[CREATE]` `crates/mermaid-viewer-core/tests/mcp.rs`
  - `[MODIFY]` `crates/mermaid-viewer-core/src/lib.rs`
- **Requirements:**
  - `mcp.rs` — `DiagramServer::new` + rmcp server handler exposing the contracted tools with the contracted JSON bodies; tool logic in directly-callable async functions (the macro layer stays thin); lazy controller; `spawn_blocking` around rendering; render errors mapped to tool error results, never panics/abort.
  - `lib.rs` — append `pub mod mcp;` behind `#[cfg(feature = "mcp")]`.
  - `tests/mcp.rs` — the four `mcp_*` unit tests, each setting the fake-window env locally (builder's choice: per-test setup must be safe under parallel test threads — e.g. each test spawns its own server and the env is process-wide only at spawn-time reads).

#### [M-4.b] Wire `mcp` subcommand + e2e
- **Intent:** Complete the agent path end-to-end over a real process boundary.
- **Target Files:**
  - `[MODIFY]` `crates/mermaid-viewer/src/main.rs`
  - `[CREATE]` `crates/mermaid-viewer/tests/mcp_e2e.rs`
- **Requirements:**
  - `main.rs` — `mcp` handler: multi-thread tokio runtime, `DiagramServer` served on the stdio transport, shutdown on EOF/Ctrl-C closing the window, exit 0.
  - `tests/mcp_e2e.rs` — spawns the built binary's `mcp` subcommand with `MERMAID_VIEWER_FAKE_WINDOW=1`, speaks newline-delimited JSON-RPC (initialize with protocol version `2025-11-25`, `notifications/initialized`, `tools/list`, `tools/call`…) over piped stdin/stdout; implements the three `e2e_*` tests with a bounded read timeout.

#### Completion Checklist
- [ ] Tool names, parameter schemas, and JSON bodies match the contract card exactly
- [ ] In-process unit tests and process-level e2e tests both green
- [ ] `mermaid-viewer mcp` is registerable in an MCP client config `{"command": "mermaid-viewer", "args": ["mcp"]}`

---

## [M-5] Cross-platform build, CI, and documentation
*Locks the POSIX+Windows build contract in CI (including the headless test pass) and documents both usage modes.*

#### Contract Card
- CI contract per Global Brief D ("Build/CI contract"): matrix `ubuntu-latest` + `windows-latest`; install cmake + platform graphics dev packages (Linux: X11/GL dev libs incl. `libasound2-dev`; Windows: CMake + MSVC toolchain via the runner's defaults); run build, clippy `-D warnings`, fmt check, `cargo test --workspace`, and the `view-headless` test pass.
- README content: build prerequisites per OS; CLI usage for all three subcommands with examples; MCP section with the client config snippet and tool list/parameters; agent-usage guidance (MCP for agents, CLI for humans); note on renderer coverage (10 diagram types) and that unsupported input returns a typed error.
- `builder's choice: <exact CI package lists and runner steps>`

#### Verification Plan
- **Commands:** local: `cargo build --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check && cargo test --workspace && cargo test -p mermaid-viewer-core --no-default-features --features view-headless`; CI: green matrix on both OSes.
- **Named test cases:**
  - `ci_matrix_green` — given the workflow when run on ubuntu-latest and windows-latest then all five steps pass on both.
  - `readme_examples_execute` — given every shell example in the README when run against a built binary then exit 0 (validated manually at M-5 sign-off; examples limited to `render`/`-h` invocations plus the MCP config JSON).
- **Definition of done:** both CI legs green from a clean checkout (dependencies installed per the workflow alone); README accurate against the implemented CLI contract; no open stubs remain in the codebase.

#### [M-5.a] CI workflow and final README
- **Intent:** Make "builds on POSIX and Windows" a verified, repeatable fact, and ship the user-facing documentation.
- **Target Files:**
  - `[CREATE]` `.github/workflows/ci.yml`
  - `[MODIFY]` `README.md`
- **Requirements:**
  - `ci.yml` — two-OS matrix; dependency install steps (cmake + per-OS graphics dev packages); the five contracted command groups as separate steps; caching via `sccache`/cargo cache acceptable (builder's choice: cache strategy).
  - `README.md` — replace M-1 stub sections with the full contracted content; MCP client snippet exactly `{"mcpServers": {"mermaid-viewer": {"command": "mermaid-viewer", "args": ["mcp"]}}}`; renderer-coverage note per contract.

#### Completion Checklist
- [ ] CI green on ubuntu + windows from clean checkout
- [ ] All README examples correct; MCP section matches the tool contract
- [ ] Full verification matrix (build/clippy/fmt/tests/headless) green; zero stub handlers left

---

## Key decisions (summary)

1. **Pure-Rust rendering stack** — `mermaid-render` + `resvg`, no Node/Chromium runtime. Main risk: mermaid-render's youth; isolated behind `NativeRenderer` with typed errors.
2. **Both shell and MCP shipped, MCP as the documented agent path** — persistent, non-blocking window fits an agent loop; CLI remains for humans/one-shot pipelines.
3. **raylib 6.0 bindings with a headless `software_renderer` test pass** — the GUI is validated by automated pixel tests on both target OSes.
