//! The MCP server handler: [`DiagramServer`].
//!
//! This module is the `mcp` contract of `mermaid-viewer-core`: an
//! [`rmcp::ServerHandler`] exposing exactly two tools over the MCP
//! protocol (server name `mermaid-viewer`):
//!
//! - **`show_diagram {code, id?, title?, scale?}`** — renders the Mermaid
//!   source to SVG and shows it in the persistent tabbed diagram window
//!   under a tab ID: an omitted `id` uses `"default"` (the existing
//!   single-diagram workflow), a fresh `id` adds a tab
//!   (`{"status":"added"}`), re-showing a known `id` replaces that tab
//!   (`{"status":"updated"}`), and a show that opens the (closed) window
//!   reports `{"status":"opened"}`. `title` is the tab's label (default:
//!   the effective ID); the OS window title is fixed. `scale` is the
//!   initial raster scale hint: a floor on the window's first raster
//!   sharpness (the raster runs at `max(hint, fit × headroom)`, clamped
//!   to a pixel cap). A rejected source,
//!   an empty `id`, and view failures (including the 16-tab limit) are
//!   *tool-level* error results (`isError: true`, content carrying the
//!   message) — never a protocol error, never a panic — so the server
//!   keeps running and serves the next request.
//! - **`close_diagram {id?}`** — closes the tab with that ID (omitting
//!   `id` closes the window and all tabs); idempotent — closing an unknown
//!   ID, an already-closed window, or a server that never opened one also
//!   succeeds; returns `{"id":"…","closed":true}` for a targeted close and
//!   `{"closed":true}` for a close-all (the ID is omitted, never
//!   serialized as `null`). An empty `id` is a *tool-level* error result
//!   (`isError: true`, content carrying the message) — never a protocol
//!   error, never a panic — so the server keeps running and serves the
//!   next request.
//!
//! ## Window + registry ownership
//!
//! `DiagramServer` owns the window *and* a tab-ID registry lazily: the
//! [`WindowController`] is created on the first `show_diagram` (always
//! with the fixed OS title [`DEFAULT_TITLE`] — the `title` parameter is
//! only the tab label), and `close_diagram` closes it (all tabs, or one
//! tab by ID). The controller
//! handle is kept across closes so the window stays re-openable on the
//! same GUI thread (the [`crate::view`] state machine: the window is open
//! exactly while it holds at least one tab, and a `show_tab` on a closed
//! window re-opens it).
//!
//! The registry (the set of active tab IDs) lives under the same mutex as
//! the controller and is the *sole basis* for the `opened`/`added`/
//! `updated` statuses: the real backend's `close`/`close_tab` are
//! fire-and-forget, so `is_open()` alone can lag an in-flight close.
//! Every commit first *reconciles* (the cross-cutting procedure): it
//! drains UI-closed tab IDs ([`WindowController::drain_tab_closes`]) and
//! removes them from the registry — always, even when the registry is
//! about to be cleared (a stale queued ID would otherwise remove a
//! legitimately re-added tab after a re-open) — then clears the registry
//! when the controller is absent or the window is no longer open. A
//! registry ID is gained only after `show_tab` acknowledges success (a
//! failed show never corrupts the registry), and lost only through
//! commits.
//!
//! ## Thread model
//!
//! The tool logic is async and transport-free: the MCP `tools/call`
//! surface is a thin `#[tool]` macro wrapper around the directly-callable
//! [`DiagramServer::show`] / [`DiagramServer::close`] methods.
//!
//! - Rendering runs in `tokio::task::spawn_blocking` (the render pipeline
//!   is synchronous and pure).
//! - The window + registry commit (lazy spawn, reconciliation, `show_tab`)
//!   is serialized under one mutex and also runs in `spawn_blocking`,
//!   because every `show_tab` blocks (at most
//!   [`crate::view::WindowController::GUI_ACK_TIMEOUT`]) until the GUI
//!   thread has processed it (on a closed window that includes opening it;
//!   a wedged GUI thread is reported as a tool error, never a hang).
//!   In-flight
//!   `show_diagram` calls therefore commit in enqueue order and their
//!   `opened`/`added`/`updated` statuses always agree with the registry.
//!
//! `DiagramServer` is `Send + Sync + 'static` as required by
//! [`rmcp::ServerHandler`] (the window handle talks to the GUI thread over
//! a plain-data channel).
//!
//! The MCP transport itself (stdio, tokio runtime, clean shutdown) is the
//! CLI's job — this module only provides the handler and the tool logic.
//! `MERMAID_VIEWER_FAKE_WINDOW=1` selects the headless fake window backend
//! at spawn time, for CI.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResponse, CallToolResult, ContentBlock},
    schemars::JsonSchema,
    tool, tool_handler, tool_router, ErrorData, ServerHandler,
};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result as CoreResult};
use crate::render::{ImageFormat, NativeRenderer, RenderOptions, RenderedImage};
use crate::view::WindowController;

/// The fixed OS window title: the controller window is always spawned with
/// it (`show_diagram`'s `title` parameter is the tab label, not the OS
/// title).
const DEFAULT_TITLE: &str = "mermaid-viewer";

/// The default initial raster scale hint when `show_diagram` omits
/// `scale` (matches [`RenderOptions::default()`]).
const DEFAULT_INITIAL_RASTER_SCALE: f64 = 2.0;

/// The default tab ID when `show_diagram` omits `id`: the existing
/// single-diagram workflow, unchanged — every such diagram is shown in the
/// one `"default"` tab, and a re-show replaces it (the tab label is the
/// provided `title`, else the effective ID).
const DEFAULT_TAB_ID: &str = "default";

/// The MCP server: the two diagram tools on a lazily-owned tabbed diagram
/// window plus the tab-ID registry that decides the show statuses.
///
/// Created with no window; the [`WindowController`] is created on the
/// first `show_diagram` and closed (idempotently) by `close_diagram`.
#[derive(Default)]
pub struct DiagramServer {
    state: Arc<State>,
}

impl std::fmt::Debug for DiagramServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiagramServer")
            .field("window_open", &self.is_window_open())
            .finish()
    }
}

impl DiagramServer {
    /// Creates a new server with no window (the window is created lazily
    /// on the first `show_diagram`).
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the diagram window is currently open.
    pub fn is_window_open(&self) -> bool {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .controller
            .as_ref()
            .is_some_and(WindowController::is_open)
    }

    /// Directly-callable `show_diagram` logic (no MCP transport).
    ///
    /// Renders `params.code` to SVG and shows it in the tabbed window
    /// under the effective tab ID: an omitted `id` uses
    /// [`DEFAULT_TAB_ID`], and a provided *empty* ID is rejected with a
    /// typed [`crate::error::Error::View`] naming the empty-ID problem,
    /// before anything is rendered. The tab label is `params.title` if
    /// provided, else the effective ID (the OS window title is always the
    /// fixed [`DEFAULT_TITLE`]).
    ///
    /// `params.scale` (if provided) is the show's initial raster scale
    /// hint (default [`DEFAULT_INITIAL_RASTER_SCALE`], i.e. the same
    /// default as [`RenderOptions::default()`]): a floor on the window's
    /// first raster sharpness (the GUI thread rasters the SVG at
    /// `max(hint, fit × headroom)`, clamped to a pixel cap — a non-finite
    /// or non-positive hint is ignored, never an error).
    ///
    /// The status describes this commit against the tab-ID registry:
    /// [`ShowStatus::Opened`] (the window was closed and this call opened
    /// it), [`ShowStatus::Added`] (a new tab in an already-open window),
    /// or [`ShowStatus::Updated`] (an existing tab replaced).
    ///
    /// Render and window failures (capacity included) surface as typed
    /// [`crate::error::Error`]s — never a panic, never a bogus `Ok`.
    pub async fn show(&self, params: &ShowDiagramParams) -> CoreResult<ShowDiagramOutcome> {
        let id = effective_id(params)?;
        let label = params.title.clone().unwrap_or_else(|| id.clone());
        let code = params.code.clone();
        let scale = params.scale.unwrap_or(DEFAULT_INITIAL_RASTER_SCALE);

        // 1. Render (synchronous + pure) off the tokio worker threads.
        //    The show path carries SVG: the raster happens on the window's
        //    GUI thread at the hint's fit-aware target, so the render
        //    options (the PNG scale) are immaterial here.
        let rendered = tokio::task::spawn_blocking(move || {
            NativeRenderer::new().render(&code, ImageFormat::Svg, &RenderOptions::default())
        })
        .await
        .map_err(|e| Error::View(format!("render task did not complete: {e}")))?;
        let image = rendered?;

        // 2. Commit the window + registry state transition off the worker
        //    threads too (a first show blocks until the GUI thread opens
        //    the window and rasters the tab's SVG).
        let state = Arc::clone(&self.state);
        let committed_id = id.clone();
        let status = tokio::task::spawn_blocking(move || {
            state.commit_show(&committed_id, &label, &image, scale)
        })
        .await
        .map_err(|e| Error::View(format!("window task did not complete: {e}")))?;
        let status = status?;

        Ok(ShowDiagramOutcome { id, status })
    }

    /// Directly-callable `close_diagram` logic (no MCP transport).
    ///
    /// An omitted `id` closes the window and all its tabs (the old
    /// close-all behavior, body `{"closed":true}`); a provided `id` closes
    /// only that tab (body `{"id":"…","closed":true}`) — the window closes
    /// when the last tab goes. Both forms are idempotent: an unknown ID, an
    /// already-closed window, and a server that never opened one are all
    /// successful no-ops, and the outcome always reports `closed: true`.
    ///
    /// A provided *empty* ID is rejected with a typed
    /// [`crate::error::Error::View`] naming the empty-ID problem, before
    /// anything is committed (other tabs are untouched). Window failures
    /// (a GUI thread that is gone) surface as typed
    /// [`crate::error::Error`]s — never a panic, never a bogus `Ok`.
    pub async fn close(&self, params: &CloseDiagramParams) -> CoreResult<CloseDiagramOutcome> {
        let target = close_target(params)?;

        // Commit under the one mutex, off the worker threads (the same
        // model as the show commit: every window + registry state change
        // is serialized; the `close_tab` send is quick, the mutex is the
        // serialization point).
        let state = Arc::clone(&self.state);
        let committed = tokio::task::spawn_blocking(move || match target {
            None => {
                state.commit_close();
                Ok(CloseDiagramOutcome {
                    id: None,
                    closed: true,
                })
            }
            Some(id) => {
                state.commit_targeted_close(&id)?;
                Ok(CloseDiagramOutcome {
                    id: Some(id),
                    closed: true,
                })
            }
        })
        .await
        .map_err(|e| Error::View(format!("window task did not complete: {e}")))?;
        committed
    }
}

/// Interior state: one mutex guarding *both* the lazily-created window
/// controller and the registry of active tab IDs.
///
/// The mutex serializes every commit (lazy spawn, reconciliation,
/// `show_tab`, `close_tab`, `close`) so concurrent `show_diagram` / `close_diagram`
/// calls commit in enqueue order and their statuses stay consistent with
/// the registry — the sole basis for the `opened`/`added`/`updated`
/// statuses (see the module docs: the real backend's `close`/`close_tab`
/// are fire-and-forget, so `is_open()` alone can lag an in-flight close).
#[derive(Default)]
struct State {
    inner: Mutex<StateInner>,
}

/// What the single [`State`] mutex guards: the window controller plus the
/// active tab-ID registry.
#[derive(Default)]
struct StateInner {
    /// The lazily-created window controller (kept across closes so the
    /// window stays re-openable on the same GUI thread).
    controller: Option<WindowController>,
    /// Active tab IDs (set-of-`String` semantics, independent of the
    /// window's tab order). Gained only after `show_tab` acknowledges
    /// success; lost only through commits.
    tab_ids: BTreeSet<String>,
}

/// Resolves the effective tab ID of a show: an omitted `id` uses
/// [`DEFAULT_TAB_ID`]; a provided *empty* ID is a typed
/// [`crate::error::Error::View`] naming the empty-ID problem (invalid IDs
/// are tool-level errors, validated before rendering).
fn effective_id(params: &ShowDiagramParams) -> CoreResult<String> {
    match &params.id {
        Some(id) if id.is_empty() => Err(Error::View(
            "diagram ID must not be empty: omit `id` to use the \"default\" tab".to_string(),
        )),
        Some(id) => Ok(id.clone()),
        None => Ok(DEFAULT_TAB_ID.to_string()),
    }
}

/// Resolves the target of a close: an omitted `id` is a close-all
/// (`None`); a provided *empty* ID is a typed
/// [`crate::error::Error::View`] naming the empty-ID problem (invalid IDs
/// are tool-level errors, validated before anything is committed).
fn close_target(params: &CloseDiagramParams) -> CoreResult<Option<String>> {
    match &params.id {
        Some(id) if id.is_empty() => Err(Error::View(
            "diagram ID must not be empty: omit `id` to close all tabs".to_string(),
        )),
        id => Ok(id.clone()),
    }
}

impl State {
    /// Reconciles the registry with the controller at the start of *every*
    /// commit (the cross-cutting procedure): first drains UI-closed tab IDs
    /// and removes them from the registry — always, even when the registry
    /// is about to be cleared (a stale queued ID would otherwise remove a
    /// legitimately re-added tab after a re-open) — then clears the
    /// registry when the controller is absent or the window is no longer
    /// open.
    fn reconcile(inner: &mut StateInner) {
        if let Some(controller) = inner.controller.as_ref() {
            for closed in controller.drain_tab_closes() {
                inner.tab_ids.remove(&closed);
            }
        }
        if !inner.controller.as_ref().is_some_and(|c| c.is_open()) {
            inner.tab_ids.clear();
        }
    }

    /// Commits one rendered diagram to the window under `id`: opens the
    /// (lazily created) window if it is closed (always with the fixed OS
    /// title [`DEFAULT_TITLE`]), adds a tab for a fresh ID, replaces an
    /// existing tab, and reports the status decided from the *reconciled*
    /// registry — `Opened` (registry empty), `Updated` (ID known), or
    /// `Added` (ID unknown, registry non-empty).
    ///
    /// `initial_scale` is the show's initial raster scale hint, passed
    /// through to `show_tab` unchanged: the GUI thread rasters the tab's
    /// SVG at `max(hint, fit × headroom)`, clamped to the pixel cap
    /// (degenerate hints are ignored there, never an error).
    ///
    /// Spawn and `show_tab` failures propagate with the registry untouched
    /// (a failed show never corrupts it); the ID is inserted only after
    /// `show_tab` acknowledges success.
    fn commit_show(
        &self,
        id: &str,
        label: &str,
        image: &RenderedImage,
        initial_scale: f64,
    ) -> CoreResult<ShowStatus> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Self::reconcile(&mut inner);

        // The status basis is the reconciled registry, decided *before* the
        // window operation: `is_open()` lags in-flight closes on the real
        // backend (a close-all and a racing show can report `added`/
        // `updated` where the window actually closed and re-opened).
        let status = if inner.tab_ids.is_empty() {
            ShowStatus::Opened
        } else if inner.tab_ids.contains(id) {
            ShowStatus::Updated
        } else {
            ShowStatus::Added
        };

        let controller = match inner.controller.as_mut() {
            Some(controller) => controller,
            None => inner
                .controller
                .insert(WindowController::spawn(DEFAULT_TITLE)?),
        };
        controller.show_tab(id, label, image, initial_scale)?;
        inner.tab_ids.insert(id.to_string());
        Ok(status)
    }

    /// Closes the window and all its tabs. Idempotent no-op when no window
    /// exists. The reconciliation runs first, then `controller.close()` and
    /// the registry clear happen in the *same* critical section: the real
    /// backend's close is fire-and-forget, so the registry must not wait
    /// for the GUI thread (or a re-show after close-all would misreport
    /// `updated` instead of `opened`).
    fn commit_close(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Self::reconcile(&mut inner);
        if let Some(controller) = inner.controller.as_ref() {
            controller.close();
        }
        inner.tab_ids.clear();
    }

    /// Closes the tab `id` (a targeted close). The reconciliation runs
    /// first; then, in the *same* critical section, `close_tab` runs on the
    /// controller (an unknown ID and a closed window are idempotent
    /// no-ops; a GUI thread that is gone is a typed error, propagated) and
    /// the ID is removed from the registry. A failed close leaves the
    /// registry untouched (it is the sole basis for the show statuses).
    fn commit_targeted_close(&self, id: &str) -> CoreResult<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Self::reconcile(&mut inner);
        if let Some(controller) = inner.controller.as_ref() {
            controller.close_tab(id)?;
        }
        inner.tab_ids.remove(id);
        Ok(())
    }
}

/// `show_diagram` parameters: `code` is required, `id`/`title`/`scale`
/// optional.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShowDiagramParams {
    /// The Mermaid diagram source to render and display.
    pub code: String,
    /// The tab ID the diagram is shown under (default: `"default"`).
    /// Re-showing a known ID replaces that tab; a fresh ID adds a tab.
    /// Must not be empty when provided.
    #[serde(default)]
    pub id: Option<String>,
    /// The tab's label in the window's tab bar (default: the effective tab
    /// ID). The OS window title is fixed and not user-settable.
    #[serde(default)]
    pub title: Option<String>,
    /// The initial raster scale hint for the window image (default 2.0):
    /// a floor on the window's first raster sharpness (the raster runs at
    /// `max(hint, fit × headroom)`, clamped to a pixel cap). A non-finite
    /// or non-positive value is ignored, never an error.
    #[serde(default)]
    pub scale: Option<f64>,
}

/// The `show_diagram` status: this call opened the (closed) window, added a
/// new tab to an already-open window, or replaced an existing tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ShowStatus {
    /// The window was closed and this call opened it.
    Opened,
    /// A new tab was added to the already-open window.
    Added,
    /// The window was open; the tab with this ID was replaced.
    Updated,
}

/// The `show_diagram` JSON body:
/// `{"id":"…","status":"opened"|"added"|"updated"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShowDiagramOutcome {
    /// The effective tab ID the diagram was shown under (the provided `id`,
    /// or the default tab ID when `id` was omitted).
    pub id: String,
    /// Whether this call opened the window (`opened`), added a new tab
    /// (`added`), or replaced an existing tab (`updated`).
    pub status: ShowStatus,
}

/// `close_diagram` parameters: an optional tab ID.
///
/// An omitted `id` — including the JSON forms rmcp 3.5.1 passes for it
/// (`{}`, `{"id":null}`) — means close-all; a malformed argument type is a
/// tool-level deserialization error, not a protocol error.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloseDiagramParams {
    /// The tab ID to close (default: all tabs — the window closes). Must
    /// not be empty when provided.
    #[serde(default)]
    pub id: Option<String>,
}

/// The `close_diagram` JSON body: `{"id":"…","closed":true}` for a
/// targeted close, `{"closed":true}` for a close-all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CloseDiagramOutcome {
    /// The tab ID that was closed, for a targeted close; `None` for a
    /// close-all (omitted from the JSON — never serialized as `null`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Always `true`: closing is idempotent.
    pub closed: bool,
}

/// The MCP `tools/call` surface: thin `#[tool]` wrappers around the
/// directly-callable [`DiagramServer::show`] / [`DiagramServer::close`]
/// logic. These are the exact fns the rmcp tool router dispatches
/// `tools/call` requests to — they can also be invoked in-process without
/// any MCP transport.
#[tool_router]
impl DiagramServer {
    /// Renders `code` and shows it under a tab in the persistent diagram
    /// window: an omitted `id` uses the `"default"` tab, a fresh `id` adds
    /// a tab, and a known `id` replaces that tab (which is activated). The
    /// first call opens the window; `title` is the tab label (default: the
    /// tab ID), and the OS window title stays fixed.
    ///
    /// Body: `{"id":"…","status":"opened"|"added"|"updated"}`. A rejected
    /// diagram source, an empty `id`, and window failures (including the
    /// 16-tab limit) are tool-level error results (`isError: true`) — the
    /// server keeps running.
    #[tool(
        name = "show_diagram",
        description = "Render a Mermaid diagram and show it under a tab in the persistent diagram window: an omitted id uses the \"default\" tab, a new id adds a tab, and a known id replaces that tab (the first call opens the window). title is the tab label (default: the tab id); the OS window title stays fixed. Returns {\"id\":\"…\",\"status\":\"opened\"|\"added\"|\"updated\"}. Rejected sources, empty ids, and window failures (including the 16-tab limit) are reported as tool errors and the server keeps running."
    )]
    pub async fn show_diagram(
        &self,
        Parameters(params): Parameters<ShowDiagramParams>,
    ) -> Result<CallToolResponse, ErrorData> {
        match self.show(&params).await {
            Ok(outcome) => Ok(CallToolResult::success(vec![ContentBlock::json(outcome)?]).into()),
            Err(err) => Ok(CallToolResult::error(vec![ContentBlock::text(err.to_string())]).into()),
        }
    }

    /// Closes a tab in the diagram window by ID; omitting the ID closes
    /// the window and all tabs.
    ///
    /// Idempotent: closing an unknown ID, an already-closed window, or a
    /// server that never opened one also succeeds. Body:
    /// `{"id":"…","closed":true}` for a targeted close, `{"closed":true}`
    /// for a close-all (the ID omitted, never `null`). An empty `id` is a
    /// tool-level error result (`isError: true`) — the server keeps
    /// running.
    #[tool(
        name = "close_diagram",
        description = "Close a tab in the diagram window by id; omitting id closes the window and all tabs. Idempotent: closing an unknown id, an already-closed window, or a server that never opened one also succeeds. Returns {\"id\":\"…\",\"closed\":true} for a targeted close and {\"closed\":true} for a close-all (the id is omitted, never null). An empty id is a tool error and the server keeps running."
    )]
    pub async fn close_diagram(
        &self,
        Parameters(params): Parameters<CloseDiagramParams>,
    ) -> Result<CallToolResponse, ErrorData> {
        match self.close(&params).await {
            Ok(outcome) => Ok(CallToolResult::success(vec![ContentBlock::json(outcome)?]).into()),
            Err(err) => Ok(CallToolResult::error(vec![ContentBlock::text(err.to_string())]).into()),
        }
    }
}

/// The rmcp server handler for [`DiagramServer`]: the exact two tools above,
/// served under the server name `mermaid-viewer`.
#[tool_handler(name = "mermaid-viewer")]
impl ServerHandler for DiagramServer {}
