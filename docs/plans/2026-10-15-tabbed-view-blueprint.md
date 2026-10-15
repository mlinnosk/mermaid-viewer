# Blueprint — Tabbed diagram window and ID-aware MCP tools

## Global Design Brief

### A. Goal and non-goals

**Goal.** Change the existing single-diagram viewer into one tabbed raylib window. The MCP server is the first consumer: callers can show diagrams under stable string identifiers, add more diagrams to the same running session, replace a diagram by showing its identifier again, and close one identifier without disturbing the others.

Example MCP flow:

```text
show_diagram { code, id: "diag1" }  -> opened
show_diagram { code, id: "diag2" }  -> added
show_diagram { code, id: "diag1" }  -> updated
close_diagram { id: "diag1" }      -> closes only diag1
```

The tab bar also lets a user select or close tabs directly. Closing the last tab closes the window.

**Non-goals:**

- Markdown parsing or embedded Mermaid blocks; this can build on the tab API later.
- Multiple simultaneous OS windows. This design uses one raylib window and multiple tabs.
- Changes to the CLI `view` behavior: it remains a blocking, single-diagram view.
- Tab reordering, drag-and-drop, or other advanced tab-management UI.

### B. Existing-system evidence

- `crates/mermaid-viewer-core/src/view.rs` currently models one window as one texture, image, zoom, and pan state. A `WindowController` owns one GUI thread and communicates with it through a channel of plain-data commands. The fake backend is selected with `MERMAID_VIEWER_FAKE_WINDOW=1`.
- `run_blocking` and `draw_frame` are used by the CLI path. The CLI path does not need to become tabbed for this change.
- `crates/mermaid-viewer-core/src/mcp.rs` owns one optional `WindowController` behind a mutex. `show_diagram` currently opens or updates that one window; `close_diagram` closes it.
- `crates/mermaid-viewer-core/tests/mcp.rs` exercises the tool logic in-process with the fake backend. `crates/mermaid-viewer/tests/mcp_e2e.rs` checks the MCP tool list, schemas, and wire responses.
- raylib 6.0's initialization path panics if a raylib window is already initialized in the process. A single window with tabs is therefore the appropriate in-process design.
- raylib 6.0 exposes `RaylibDraw::gui_tab_bar` when its `raygui` feature is enabled. It receives the tab labels and selected index, and returns the index of a tab requested for closing (or -1).
- raylib's own `tests/render_gui.rs` demonstrates raygui controls rendering under the software-renderer test harness. The existing headless pixel tests provide a suitable test seam for the tab UI.

### C. Intended design

#### Window and tab model

- A `Window` owns tabs in insertion order and an active-tab index. A tab contains its identifier, display label, texture/image, and independent zoom/pan state.
- Showing an existing identifier replaces that tab's content, resets its zoom/pan, and activates it. Showing a new identifier adds and activates a tab.
- A tab bar is drawn at the top using raygui. The active diagram is drawn in the remaining content area. Zoom and pan input applies to the active tab.
- Closing a tab through the UI removes it. The GUI thread reports the closed identifier to the controller so the MCP registry can be reconciled. Closing the final tab closes the raylib window. ESC or the OS close button closes the window and all tabs.
- Set a documented maximum of 16 tabs. The limit is enforced by the controller and surfaced as an MCP tool-level error; it must not silently drop an update.
- Add the `raygui` feature to both raylib dependency configurations (`raylib-ogl` and `raylib-sw`) so the production and headless builds compile the same UI code.

#### Controller and thread model

The raylib handle and all tab textures remain owned by the existing dedicated GUI thread. Only strings, rendered images, commands, acknowledgements, and close events cross channels; `RaylibThread` and other non-`Send` raylib values never leave that thread.

Replace the single-content update operation with ID-aware operations:

```rust
pub fn show_tab(&self, id: &str, label: &str, image: &RenderedImage) -> Result<()>;
pub fn close_tab(&self, id: &str) -> Result<()>;
pub fn drain_tab_closes(&self) -> Vec<String>;
```

`show_tab` acknowledges the GUI-thread result for both add and replace operations, including capacity errors. This makes failures observable to the MCP caller and keeps fake and real backends consistent. `close_tab` is idempotent for unknown identifiers. `close` still closes all tabs; `is_open` remains available.

The fake backend stores IDs and applies the same add/replace/close/capacity rules without raylib. It does not synthesize user-close events.

#### MCP contract

- `show_diagram` gains optional `id`; omitted IDs use `"default"`, preserving the existing single-diagram workflow. IDs must be non-empty. `title` becomes the tab label and defaults to the ID; the OS window title remains the server's fixed default title. `scale` remains unchanged.
- Status values are `opened` (the window was closed and is now open), `added` (new tab in an already-open window), and `updated` (existing tab replaced). The success body is `{"id":"…","status":"opened|added|updated"}`.
- `close_diagram` gains optional `id`. With an ID, it closes only that tab; without an ID it closes all tabs, preserving the old close-all behavior. Both are idempotent. The response is `{"id":"…","closed":true}` for a targeted close and `{"closed":true}` for close-all. The optional response ID is omitted, not serialized as `null`, for close-all.
- The server keeps a set of known tab IDs under the existing state mutex. Before each state operation it drains UI-close events and removes those IDs. If the controller reports the window closed (for example, ESC or OS close), the set is cleared. MCP show/close commits remain serialized under the mutex.
- Rendering remains in `spawn_blocking`; window operations remain off Tokio worker threads. Render and view failures are returned as tool-level errors, not protocol errors.

The server continues to expose exactly the two existing tools; only their schemas and result bodies change.

### D. Contract and compatibility map

**View contract (`mermaid-viewer-core::view`):**

- `WindowController::spawn(title)` creates an initially closed controller.
- `show_tab(id, label, image)` opens the window if necessary, adds or replaces the named tab, and activates it. It rejects non-PNG images and new tabs beyond `MAX_TABS = 16` with a typed view error.
- `close_tab(id)` removes the tab if present; removing the final tab closes the window.
- `drain_tab_closes()` returns IDs closed through the tab bar since the previous drain.
- `close()` closes the window and all tabs; `is_open()` reports whether the window has at least one tab.
- `run_blocking(title, image)` remains a one-diagram, blocking view API.

**MCP contract (`mermaid-viewer-core::mcp`):**

- `show_diagram { code, id?, title?, scale? }` → `{"id":"…","status":"opened|added|updated"}`. `id` defaults to `"default"`; `title` is the tab label and defaults to the ID.
- `close_diagram { id? }` → targeted close `{"id":"…","closed":true}` or close-all `{"closed":true}`.
- Existing tool names and server name remain unchanged. Invalid diagrams, invalid IDs, tab-capacity failures, and view failures are tool-level errors.

**CLI contract:** unchanged. `view`, `render`, and `mcp` command-line syntax is not otherwise changed.

**Build contract:** add raygui to both raylib feature configurations. The default build and `view-headless` build must both compile and test the tab code.

### E. Milestone map

1. **M-1 — Tabbed view/controller:** tab state, controller commands, fake backend, raygui UI, and headless tests.
2. **M-2 — MCP IDs:** ID-aware show/close tools and in-process tests.
3. **M-3 — E2E and docs:** MCP process tests, README updates, and full verification.

**Invariants:** one raylib initialization per process; raylib handles stay on the GUI thread; channels carry plain data; window state changes are serialized; bad input does not panic; fake and real backends follow the same lifecycle and capacity rules; MCP stdout contains protocol frames only.

---

## [M-1] Tabbed view and controller

*Replace the single diagram texture with an ID-addressable tab collection and add a raygui tab bar without changing the CLI's one-shot view path.*

### Contract card

- `Window` owns `Vec<Tab>` and an active index. Each tab has ID, label, texture/image, zoom, and pan.
- Add/replace and close commands are addressed by ID. A close event from raygui is returned to the controller as an ID.
- The window is open exactly while it has one or more tabs. Removing its last tab closes it; closing the window by ESC/OS closes all tabs.
- `show_tab` acknowledges success or failure, including tab-capacity failure. `MAX_TABS` is 16.
- The fake backend mirrors ID, replacement, close, and capacity behavior; UI-close event draining returns no events there.
- `run_blocking` and its single-diagram rendering path remain unchanged.
- Both raylib dependency configurations enable `raygui`.

### Verification plan

- Commands: `cargo test -p mermaid-viewer-core`; `cargo test -p mermaid-viewer-core --no-default-features --features view-headless`; then workspace build, clippy, and fmt checks.
- Named cases:
  - `fake_tab_lifecycle` — add two IDs, close one while the other remains open, close the last, then show again.
  - `fake_tab_replace` — showing the same ID twice replaces rather than duplicates it.
  - `fake_tab_cap` — 16 distinct IDs succeed; the 17th fails; replacement at capacity succeeds.
  - `fake_close_unknown_tab_is_noop` — closing an unknown ID leaves the current tabs unchanged.
  - `view_rejects_svg_image` — `show_tab` rejects SVG while closed and open; `run_blocking` continues to reject SVG.
  - `tab_close_index_updates_active` — pure state logic checks active-index adjustment and last-tab closure.
  - `headless_tabbed_frame_active_content` — two differently colored tabs render the selected tab's pixels.
  - `headless_tab_bar_renders` — the headless frame contains rendered raygui tab-bar pixels.
  - Existing layout and single-frame tests continue to pass, adjusted only if their drawing area now excludes the tab strip.

### Target files

- `[MODIFY]` `crates/mermaid-viewer-core/src/view.rs`
- `[MODIFY]` `crates/mermaid-viewer-core/Cargo.toml`
- `[MODIFY]` `crates/mermaid-viewer-core/tests/view_headless.rs`

### Completion checklist

- [ ] Fake and real controller paths support add, replace, close, capacity errors, and reopen.
- [ ] Tab selection, per-tab zoom/pan, and user tab-close events work in the raygui window.
- [ ] Default and software-renderer test passes are green.
- [ ] Manual desktop smoke: add two tabs through MCP, switch between them, close one from the tab bar, and confirm the other remains.

---

## [M-2] ID-aware MCP tools

*Manage the tab collection through the existing MCP tools, preserving the single-diagram default behavior.*

### Contract card

- Replace `State`'s single-controller-only ownership with a controller plus a set of active tab IDs, guarded by the existing mutex.
- On each commit, reconcile UI-closed IDs and clear the registry if the window is no longer open.
- `show_diagram` defaults a missing ID to `"default"`; title is the tab label and defaults to the ID. Status is `opened`, `added`, or `updated` as defined in the global contract.
- `close_diagram` with an ID closes one tab; without an ID closes all. Both are idempotent.
- Update tool descriptions, parameter documentation, outcome types, and the directly callable API to match the wire contract.
- Reject empty IDs and surface render/view/capacity failures as MCP tool errors.

### Verification plan

- Command: `cargo test -p mermaid-viewer-core --test mcp`.
- Named cases:
  - `mcp_show_diagram_opens` — default ID returns `{"id":"default","status":"opened"}`.
  - `mcp_show_diagram_updates` — the same ID returns `updated` on the next call.
  - `mcp_show_diagram_adds_tab` — a second ID returns `added`; showing either existing ID returns `updated`.
  - `mcp_close_diagram_by_id` — closing one of two IDs preserves the other; close-all then closes the window.
  - `mcp_close_unknown_id_noop` — unknown ID returns successful idempotent close without changing other tabs.
  - `mcp_close_diagram_idempotent` — omitted ID still closes all and a later show reopens.
  - `mcp_show_diagram_tab_cap` — the limit becomes a tool-level error and the server remains usable after a tab is closed.
  - `mcp_show_diagram_render_error` — render errors remain tool-level errors and do not corrupt other tabs.
  - `mcp_rejects_empty_id` — empty ID returns a tool-level error.

### Target files

- `[MODIFY]` `crates/mermaid-viewer-core/src/mcp.rs`
- `[MODIFY]` `crates/mermaid-viewer-core/tests/mcp.rs`

### Completion checklist

- [ ] Directly callable logic and generated MCP tools have matching semantics.
- [ ] Existing one-diagram callers work without supplying an ID.
- [ ] Per-ID and close-all behavior, errors, and concurrency serialization are tested.

---

## [M-3] MCP E2E, documentation, and final verification

### Contract card

- Keep exactly two MCP tools, with `id` optional in both schemas.
- Update the README to describe the tab bar, `id` defaults, `opened`/`added`/`updated` statuses, targeted close, close-all, and the 16-tab limit.
- State that Markdown support and multiple OS windows are not part of this change.
- Keep CLI documentation unchanged except for clarifying that MCP's window is tabbed.

### Verification plan

- Commands: `cargo build --workspace`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --check`; `cargo test --workspace`; `cargo test -p mermaid-viewer-core --no-default-features --features view-headless`.
- Named cases:
  - `e2e_initialize_lists_two_tools` — verifies both tool schemas include optional `id`, with `code` still the only required show parameter.
  - `e2e_show_and_close_roundtrip` — verifies default-ID opened/updated and omitted-ID close-all responses.
  - `e2e_two_tabs_close_one` — shows two IDs, closes one, and confirms close-all remains available.
  - `e2e_stdin_eof_exits_zero` — shutdown remains clean with the tabbed controller.

### Target files

- `[MODIFY]` `crates/mermaid-viewer/tests/mcp_e2e.rs`
- `[MODIFY]` `README.md`

### Completion checklist

- [ ] E2E tests verify schemas and multi-tab wire behavior under the fake backend.
- [ ] Full default and headless verification matrices pass.
- [ ] README contracts match the implemented MCP schema and outcomes.

---

## Key decisions

1. **One window with tabs.** raylib's process-global initialization rules make multiple independent raylib windows unsuitable here; tabs provide the requested in-session multi-diagram workflow.
2. **raygui tab bar.** Enable the crate's `raygui` feature for both production and software-renderer dependencies; headless pixels verify it renders.
3. **Stable MCP IDs.** Omitted IDs map to `"default"`, preserving the current single-diagram usage. Re-showing an ID updates that tab.
4. **Close one or all.** `close_diagram` with an ID closes one tab; omitted ID retains existing close-all semantics.
5. **UI close synchronization.** The GUI reports closed IDs; the MCP state reconciles them on the next operation. Closing the OS window clears all known IDs on the next operation.
6. **Fixed tab cap.** Limit each window to 16 tabs and return a clear tool-level error on overflow.
7. **Markdown deferred.** A later document-rendering feature can extract diagrams and use this ID-addressable tab API.
