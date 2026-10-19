# Blueprint — Render all Mermaid diagrams found in a Markdown file

## Global Design Brief

### A. Objective and Non-Goals

**Objective.** `mermaid-viewer view <source>` gains one new input mode: when the input text contains one or more `mermaid`-tagged fenced code blocks, the CLI renders **every** such block and shows them as tabs in the *existing* tabbed window (`view::WindowController`, the same controller the MCP server drives), then blocks until the window is closed. Input text with no such fences keeps the current behaviour exactly: the whole input is one Mermaid diagram shown in the single-diagram full-window path (`run_blocking`).

**Non-goals (this change does not alter):**
- `render` and `mcp`: both remain single-diagram. No multi-output file convention, no MCP wire-contract change.
- `view --watch` on Markdown input: not supported; it is a typed runtime error (exit 1). `view --watch` on single-diagram input is unchanged.
- Markdown prose rendering: only the code *inside* `mermaid` fences is extracted and rendered.
- Multiple OS windows: one tabbed window, as today.
- File-extension-based detection: routing is purely content-based (`--code`, `--stdin`, and `FILE` all route identically).
- No new crate dependency: the fence scanner is hand-written pure logic.

### B. Existing System Evidence

- `crates/mermaid-viewer-core/src/view.rs` — `WindowController` (tabbed, dedicated GUI thread + on-demand SVG raster worker; `Send + Sync`): `spawn(title)` (line 2470), `show_tab(id, label, image, initial_scale)` (2530, requires **SVG**), `close_tab`, `drain_tab_closes`, `close`, `is_open` (2682), `Drop` (2693, bounded shutdown), `MAX_TABS = 16` (2464), `GUI_ACK_TIMEOUT` (2494). Backend chosen from `MERMAID_VIEWER_FAKE_WINDOW` at `spawn` (`fake_window_enabled`, line 177). There is currently **no** "block until the user closes the window" API — the MCP caller is long-lived.
- `crates/mermaid-viewer-core/src/view.rs` — `run_blocking(title, image)` (2260) / `run_blocking_updates` (2289): the CLI's current single-diagram path, PNG-only, **no tab strip**, runs on the calling thread, returns immediately under the fake backend (`ensure_png` + drain).
- `crates/mermaid-viewer-core/src/render.rs` — `NativeRenderer::render(source, format, opts) -> Result<RenderedImage>` (86), pure; `RenderOptions::default()` = `{ png_scale: 2.0 }` (56), `png_scale` is ignored for `ImageFormat::Svg`.
- `crates/mermaid-viewer-core/src/error.rs` — `Error::{Render, Rasterize, View, Io}`; message prefixes `mermaid render failed: `, `svg rasterization failed: `, `view failed: `.
- `crates/mermaid-viewer/src/main.rs` — `run` (91), `run_view(source, args)` (190), `run_view_watch` (221), `read_source` (402); imports include `ImageFormat, NativeRenderer, RenderOptions, RenderedImage, run_blocking, run_blocking_updates`. `main` prints `"{BIN_NAME}: {err}"` to stderr on failure; exit 1.
- `crates/mermaid-viewer/src/cli.rs` — single source of truth for CLI flags; `ViewArgs.file` doc is "Path to a Mermaid source file."; `View` subcommand doc line "Render a diagram and show it in a GUI window." (asserted by `tests/exit_codes.rs::cli_view_not_implemented_replaced`).
- `crates/mermaid-viewer/tests/cli.rs` — process-level CLI tests + `TempDir` helper; `view_watch_fake_env_smoke` (189) shows the pattern of setting `MERMAID_VIEWER_FAKE_WINDOW=1` on the child.
- `crates/mermaid-viewer-core/tests/view_headless.rs` — headless (`view-headless`) pixel tests, `#![cfg(feature = "view-headless")]`; `crates/mermaid-viewer-core/tests/mcp.rs` — fake-backend in-process MCP tests.
- `crates/mermaid-viewer-core/src/lib.rs` — module declarations (39–55): `error` + `render` always; `view` gated on a raylib feature; `mcp` gated on `mcp` + a raylib feature. Crate is `#![deny(missing_docs, unsafe_code)]`.
- `README.md` — line ~231 states "Markdown support (parsing Mermaid blocks out of Markdown) ... unsupported"; the `view` section (98–122) documents the single-diagram blocking window.

**Conventions to preserve:** `thiserror` typed errors, never panic on bad input; every public item documented; diagnostics to stderr, stdout clean; pure logic unit-tested in-module; process tests via the real binary with the fake window backend; renderers run *before* any window is opened, so a bad source never opens a window.

### C. Intended Design

A pure extraction unit in core + a routing change in the CLI + one small controller addition.

1. **`mermaid_viewer_core::markdown` (new, unconditional module).** `pub fn mermaid_blocks(source: &str) -> Vec<String>` scans for fenced code blocks whose info string's first token is `mermaid` (ASCII case-insensitive) and returns the *bodies*, in document order, whitespace-trimmed, with whitespace-only blocks omitted. It skips non-mermaid fences as opaque blocks so a `` ```mermaid `` line *inside* another code block is not mistaken for an opener. This is the single classification signal: **non-empty result ⇒ Markdown/tabbed path; empty result ⇒ single-diagram path.** No keyword list of diagram types, no comment/front-matter special-casing, no file-extension logic — chosen over a diagram-type-word check because it cannot misclassify a valid diagram and needs no duplicated grammar knowledge.

2. **`WindowController::wait_closed(&self)` (new).** Blocks the calling thread until the window is no longer open; returns immediately under the fake backend (there is no user to close it — mirrors `run_blocking`'s fake fast-path). This lets the CLI reuse the exact tabbed controller and still "block until closed".

3. **CLI `view` routing (`main.rs`).** `run_view` first computes `mermaid_blocks(source)`. Empty ⇒ existing single-diagram body (extracted verbatim into a helper). Non-empty ⇒ render each block to **SVG** with `NativeRenderer::render(block, ImageFormat::Svg, &RenderOptions::default())` (all up front, before any window), spawn `WindowController` titled `args.title`, `show_tab` the first `MAX_TABS` blocks as `diagram-{n}` / `Diagram {n}` with `initial_scale = args.scale`, print a one-line stderr note if blocks were omitted, then `wait_closed()`. `--watch` combined with the Markdown path is rejected with a typed `Error::View`.

**Rejected alternative:** render blocks to PNG and use the single-diagram eager path (`run_blocking_updates`) — that path is single-texture, full-window, and has no tab strip; tabs require the controller, and `show_tab` is SVG-only.

**Invariants every milestone preserves:**
- `render`/`mcp` wire and file contracts unchanged; `view` single-diagram output unchanged.
- No panic on malformed input; every failure is a typed `Error`; exit codes unchanged (`0`/`1`/`2`).
- stdout carries only image/protocol bytes; all diagnostics on stderr.
- Rendering happens before the window opens; a rejected block exits 1 with no window.
- raylib stays single-init on the controller's GUI thread; the CLI only touches `WindowController` (channel-based).
- `#![deny(missing_docs, unsafe_code)]` holds (no new `unsafe`).

### D. Contract and Compatibility Map

**New public contract — `mermaid-viewer-core::markdown`** (authoritative):

```rust
pub fn mermaid_blocks(source: &str) -> Vec<String>
```
- Returns the body of every `mermaid`-tagged fenced code block in `source`, in document order; `Vec::new()` when there is none.
- A fenced block opens on a line whose content, after removing **0–3** leading spaces, begins with a run of **≥3 identical** `` ` `` or `~` characters; the info string is the remainder of that line, trimmed.
- The block is a *mermaid* block when the first ASCII-whitespace-separated token of the info string equals `mermaid` byte-wise case-insensitively (`Mermaid`, `mermaid title="x"` match).
- The body is every line up to the matching **closing fence** — a line that, after removing 0–3 leading spaces, is a run of ≥ the opener's length of the **same** fence character followed by whitespace only. The opener's indentation (0–3 spaces) is stripped from each body line. An unterminated mermaid fence extends to end of input (CommonMark).
- A **non-mermaid** fence and its body are skipped as opaque text (a ```` ```mermaid ```` line inside another code block is not an opener).
- The body is `trim()`-ed; a whitespace-only (empty) body is **omitted** from the result.
- `\r\n` line endings are tolerated (trailing `\r` stripped). Never panics.
- `builder's choice: CommonMark fringe cases not listed above (backtick-containing info strings, mixed fence characters, closing-fence indentation beyond the 0–3 rule)`.

**New public contract — `mermaid-viewer-core::view::WindowController`:**

```rust
pub fn wait_closed(&self)
```
- Real backend: blocks the calling thread until the window is no longer open (user close / ESC / last tab closed) or the GUI thread has finished; then returns.
- Fake backend: returns immediately.
- Infallible; safe to call from any thread; document that it is intended for a controller whose tabs were just shown.

**Unchanged contracts (must not change):** `show_diagram`/`close_diagram` tools, their schemas, statuses and bodies; `render` CLI flags and output; `view` flags; `NativeRenderer`, `RenderOptions`, `RenderedImage`, `ImageFormat`; `Error` variants; `run_blocking`, `run_blocking_updates`, all existing `WindowController` methods; exit codes; README's documented `view` single-diagram behaviour.

**Affected consumers:** only the `mermaid-viewer` binary's `view` handler and its help text. No existing caller of a changed signature exists; all changes are additive except the `view` routing (backward compatible: no-fence input behaves exactly as before).

**CLI-facing decisions:** tab ID = `diagram-{n}`, label = `Diagram {n}`, `n` 1-based and contiguous over the blocks actually shown; `--scale` becomes the controller's initial raster-scale hint for the Markdown path; at most `WindowController::MAX_TABS` (16) tabs, extras omitted with a stderr note; `--watch` + Markdown ⇒ `Error::View` (exit 1).

### E. Milestone Map

1. **M-1 — Core fence extraction** (`markdown` module + `lib.rs`): the pure classifier everything else depends on; independently unit-testable and compiles under every feature pass.
2. **M-2 — Controller blocking wait** (`view.rs`): the one core API the CLI needs; additive and safe to land before the CLI uses it.
3. **M-3 — CLI wiring** (`main.rs`, `cli.rs`, CLI tests): routes `view` to the tabbed path; depends on M-1 and M-2.
4. **M-4 — Documentation and final verification** (`README.md` + full matrix).

Ordering keeps each intermediate tree buildable/testable: M-1 adds a self-contained module; M-2 adds an unused-but-tested method; M-3 is the only behaviour change and is fully gated by the earlier two; M-4 only edits prose and re-runs the matrix.

---

## [M-1] Core Markdown fence extraction

*Adds the pure, feature-independent scanner that decides "single diagram vs Markdown" and yields the diagram sources.*

#### Contract Card — stated once, referenced everywhere
- Module `mermaid_viewer_core::markdown` (always compiled).
- `pub fn mermaid_blocks(source: &str) -> Vec<String>` with the exact semantics in the Global Design Brief §D ("New public contract — `mermaid-viewer-core::markdown`").
- No new dependencies; no feature gate; no new `Error` variant.

#### Verification Plan — the review gate
- **Commands:** `cargo test -p mermaid-viewer-core markdown` and `cargo build -p mermaid-viewer-core --no-default-features` (proves the module is feature-independent) and `cargo clippy -p mermaid-viewer-core --all-targets -- -D warnings`.
- **Named test cases** (all in `markdown.rs`'s `#[cfg(test)] mod tests`):
  - `markdown_extracts_blocks_in_order` — given a doc with prose and two fenced mermaid blocks when extracting then exactly those two bodies are returned, in order, trimmed.
  - `markdown_ignores_non_mermaid_fences` — given ```` ```rust ```` and bare ```` ``` ```` blocks when extracting then the result is empty.
  - `markdown_ignores_mermaid_inside_other_fence` — given a `mermaid` fence nested inside a ```` ```markdown ```` block when extracting then nothing is extracted.
  - `markdown_supports_tilde_and_long_fences` — given `~~~mermaid` and ```` ````mermaid ```` blocks when extracting then both bodies are returned; a `` ``` ``-opened block is not closed by `~~~`.
  - `markdown_unterminated_block_extends_to_eof` — given an unclosed mermaid fence when extracting then the body is the rest of the input.
  - `markdown_info_token_matches_attributes_and_case` — given ```` ```Mermaid title="x" ```` when extracting then the body is returned.
  - `markdown_strips_up_to_three_space_indent` — given a fence indented 2 spaces when extracting then the body has that indentation removed.
  - `markdown_omits_whitespace_only_blocks` — given an empty mermaid fence plus a valid one when extracting then only the valid body is returned.
  - `markdown_returns_empty_without_fences` — given a plain `graph TD\n A-->B` when extracting then the result is empty.
  - `markdown_handles_crlf` — given CRLF-terminated fences when extracting then the body contains `\n`-joined lines without `\r`.
- **Definition of done:** the module builds with and without default features; every named case passes; no panic on any input; the crate still passes `missing_docs`/`unsafe_code` lints.

#### [M-1.a] Add the `markdown` module
- **Intent:** introduce the pure extraction function and expose it from the crate.
- **Target Files:**
  - `[CREATE]` `crates/mermaid-viewer-core/src/markdown.rs`
  - `[MODIFY]` `crates/mermaid-viewer-core/src/lib.rs`
- **Requirements:**
  - `markdown.rs` — **Purpose:** pure text scanner implementing the `mermaid_blocks` contract-card entry. **Contract changes:** none beyond the card entry. **Behavioral:** implement the opener/close/opaque-skip/indent-strip/trim/omit rules exactly; total function, never panics; deterministic; `#[must_use]` optional (builder's choice). **Acceptance example:** `"text\n```mermaid\ngraph TD\n  A-->B\n```\nmore"` ⇒ `vec!["graph TD\n  A-->B"]`.
  - `lib.rs` — **Purpose:** register the module. **Contract changes:** add `pub mod markdown;` **unconditionally** (outside the `view`/`mcp` gates) and add one crate-doc bullet naming the Markdown extraction capability. **Behavioral:** the module must compile when neither `view` nor `mcp` is enabled.

#### Completion Checklist
- [ ] `mermaid_blocks` matches the contract card exactly.
- [ ] All named M-1 cases pass under `cargo test -p mermaid-viewer-core markdown`.
- [ ] `cargo build -p mermaid-viewer-core --no-default-features` succeeds.

---

## [M-2] Blocking wait for the tabbed controller

*Adds the one method the CLI needs to reuse the MCP tabbed window as a blocking `view`.*

#### Contract Card — stated once, referenced everywhere
- `pub fn WindowController::wait_closed(&self)` with the exact semantics in the Global Design Brief §D ("New public contract — `view::WindowController`").
- No change to any existing `WindowController` method, `Window`, or `run_blocking*`.

#### Verification Plan — the review gate
- **Commands:** `cargo test -p mermaid-viewer-core` and `cargo clippy -p mermaid-viewer-core --all-targets -- -D warnings`.
- **Named test case:** `view_wait_closed_fake_returns_immediately` — given the fake window backend enabled and a controller with one `show_tab`-ed SVG tab when `wait_closed()` is called then it returns promptly without blocking (the fake backend has no user).
- **Definition of done:** the method is public, documented, and infallible; the fake path returns without blocking; the real path polls the open state without holding the controller mutex across the sleep; existing `view` tests still pass.

#### [M-2.a] Add `WindowController::wait_closed`
- **Intent:** provide a blocking "run until the user closes the window" call for controller-driven CLI viewing.
- **Target Files:**
  - `[MODIFY]` `crates/mermaid-viewer-core/src/view.rs`
- **Requirements:**
  - **Purpose:** blocking terminal hook for a controller whose tabs are already shown. **Contract changes:** the contract-card entry only. **Behavioral:** `Inner::Fake` returns immediately; `Inner::Real` loop-sleeps, re-reading the `open` flag each iteration (and treating a finished GUI thread as closed), and returns as soon as the window is not open — never holds `RealView`'s mutex across the sleep; never panics. **Acceptance example:** fake backend, one tab shown ⇒ returns in the same call, no sleep loop. `builder's choice: poll interval (tens of milliseconds)`.
  - Add the named test case to `view.rs`'s `#[cfg(test)] mod tests`; it needs `MERMAID_VIEWER_FAKE_WINDOW=1` set (env is process-global and read only at `spawn`; no other `view.rs` unit test spawns a window, so this is race-free within the lib test binary — note this in the test doc, mirroring `tests/mcp.rs`).

#### Completion Checklist
- [ ] `wait_closed` exists, is documented, and is infallible.
- [ ] Fake backend returns immediately; real backend polls without blocking the GUI thread.
- [ ] `cargo test -p mermaid-viewer-core` green.

---

## [M-3] CLI `view` renders Markdown blocks as tabs

*Routes `view` to the tabbed controller when the input contains fenced mermaid blocks, leaving every other path unchanged.*

#### Contract Card — stated once, referenced everywhere
- Routing: `run_view` calls `mermaid_blocks(source)`; empty ⇒ single-diagram path unchanged; non-empty ⇒ Markdown path.
- Markdown path: for the first `min(blocks.len(), WindowController::MAX_TABS)` blocks, render SVG via `NativeRenderer::render(block, ImageFormat::Svg, &RenderOptions::default())`; spawn `WindowController::spawn(&args.title)`; `show_tab(id = "diagram-{n}", label = "Diagram {n}", svg, initial_scale = args.scale)` for `n = 1..=shown`; then `WindowController::wait_closed()`; drop the controller.
- Over-limit: when `blocks.len() > MAX_TABS`, print exactly one stderr line of the form `mermaid-viewer: found {total} mermaid diagrams; showing the first {MAX_TABS} (window tab limit)` and show only the first `MAX_TABS`; the view still succeeds (exit 0). Blocks beyond the limit are neither rendered nor validated.
- `--watch` + Markdown ⇒ `Err(Error::View("--watch is not supported for Markdown input; remove --watch or point it at a single Mermaid diagram".into()))` (exit 1).
- Rendering is content-based for all sources (`FILE`, `--code`, `--stdin`); stdout stays empty; diagnostics on stderr.
- `cli.rs`: update `ViewArgs.file`'s doc to name Markdown-with-fences as an accepted input; append a Markdown sentence to the `View` subcommand doc **without** changing its first line.

#### Verification Plan — the review gate
- **Commands:** `cargo test -p mermaid-viewer` and `cargo clippy -p mermaid-viewer --all-targets -- -D warnings`.
- **Named test cases:**
  - `run_view_markdown_fake_env_returns_ok` (unit, `main.rs`) — given the fake backend and `--code` holding two valid fenced mermaid blocks when `run` is called then it returns `Ok`.
  - `run_view_markdown_with_watch_is_view_error` (unit, `main.rs`) — given a temp `.md` containing fences and `--watch` set when `run` is called then it returns `Err(Error::View(_))`.
  - `cli_view_markdown_fake_env_smoke` (process, `tests/cli.rs`) — given a temp `.md` with prose and two valid mermaid blocks when the real binary runs `view <file>` with `MERMAID_VIEWER_FAKE_WINDOW=1` then exit 0, stdout empty, stderr empty.
  - `cli_view_markdown_over_tab_limit_notes_omission` (process) — given 17 valid blocks when viewing then exit 0 and stderr contains `window tab limit`.
  - `cli_view_markdown_bad_block_exits_1` (process) — given one valid and one parser-rejected block when viewing then exit 1, stderr contains `mermaid render failed`, stdout empty, and (fake backend) no window opens.
  - Existing `run_view_*`, `cli_render_*`, `view_watch_fake_env_smoke`, and exit-code tests remain green untouched.
- **Definition of done:** no-fence input behaves exactly as before; fence input shows one tab per block via `WindowController`; over-limit succeeds with the note; `--watch` + Markdown errors; `render`/`mcp` and all exit codes unchanged.

#### [M-3.a] Route `view` and implement the Markdown tab path
- **Intent:** make `view` detect fenced diagrams and show them as tabs.
- **Target Files:**
  - `[MODIFY]` `crates/mermaid-viewer/src/main.rs`
  - `[MODIFY]` `crates/mermaid-viewer/src/cli.rs`
- **Requirements:**
  - `main.rs` — **Purpose:** classify input and drive the tabbed controller for the Markdown path. **Contract changes:** none beyond the contract card. **Behavioral:** move the current `run_view` body verbatim into a single-diagram helper; the new `run_view` branches on `mermaid_blocks(source).is_empty()`; the Markdown helper renders all shown blocks to SVG *before* spawning the controller (so a rejected block exits 1 with no window), enforces the cap + note, passes `args.scale` as the `initial_scale` hint, and calls `wait_closed`; the `--watch` guard runs before any render. Add the required imports (`mermaid_viewer_core::markdown::mermaid_blocks`, `WindowController`). **Acceptance examples:** a `--code` value with two mermaid fences and `--watch:false` under the fake backend returns `Ok`; the same value with `--watch:true` (via a temp `FILE`) returns `Err(Error::View(_))`.
  - `cli.rs` — **Purpose:** describe the widened `view` input. **Contract changes:** doc/help text only. **Behavioral:** keep the subcommand's first doc line identical; end the `--watch` doc note with the Markdown restriction.

#### [M-3.b] Process-level CLI tests
- **Intent:** pin the end-to-end Markdown `view` behaviour of the real binary.
- **Target Files:**
  - `[MODIFY]` `crates/mermaid-viewer/tests/cli.rs`
- **Requirements:**
  - **Purpose:** add the three process-level named cases in the M-3 verification plan, reusing the file's `TempDir` helper and the fake-backend child-env pattern (`view_watch_fake_env_smoke`). **Contract changes:** none. **Behavioral:** assert exit codes, empty stdout, and the exact stderr conditions named in the plan; the over-limit fixture uses 17 valid `graph TD` blocks; the bad-block fixture reuses the `gitGraph\n  frobnicate` rejected-source class already used across the suite.

#### Completion Checklist
- [ ] No-fence input unchanged (existing tests pass).
- [ ] Fence input shows one tab per block via the controller.
- [ ] Over-limit shows 16 with the stderr note; exit 0.
- [ ] `--watch` + Markdown is a typed error.
- [ ] `render`/`mcp` untouched; stdout/stderr hygiene holds.

---

## [M-4] Documentation and final verification

*Align the README with the shipped behaviour and run the full matrix.*

#### Contract Card — stated once, referenced everywhere
- README statements must match the implementation: `view` accepts Mermaid source *or* Markdown with fenced `mermaid` blocks (one tab per block); the tab limit and omission note; `--watch` is unsupported for Markdown input; `render` and `mcp` remain single-diagram.
- No code contract changes in this milestone.

#### Verification Plan — the review gate
- **Commands:** `cargo build --workspace`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --check`; `cargo test --workspace`; `cargo test -p mermaid-viewer-core --no-default-features --features view-headless`; `cargo build -p mermaid-viewer-core --no-default-features`.
- **Named check:** the README's `view` section and the "Markdown support ... unsupported" sentence no longer contradict the implementation.
- **Definition of done:** full matrix green on both feature passes; no source file outside the target list changed.

#### [M-4.a] Update the README
- **Intent:** document the new `view` behaviour and remove the now-false "unsupported" claim.
- **Target Files:**
  - `[MODIFY]` `README.md`
- **Requirements:**
  - **Purpose:** keep README contracts true. **Contract changes:** documentation only. **Behavioral:** in the `view` section, state that a source containing `mermaid`-tagged fenced code blocks is rendered as one tab per block in the tabbed window, with the 16-tab limit and the omission note; state that `--watch` applies to single-diagram sources only; replace the line declaring Markdown support unsupported with a statement that Markdown *block extraction* is supported for `view` while `render`/`mcp` remain single-diagram; leave all other sections unchanged.

#### Completion Checklist
- [ ] README matches the implementation.
- [ ] Full verification matrix green (default and `view-headless`; no-default-features build).
- [ ] `cargo fmt --check` clean.

---

[BLUEPRINT COMPLETE - READY FOR IMPLEMENTATION]
