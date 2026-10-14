//! The MCP server handler: [`DiagramServer`].
//!
//! This module is the `mcp` contract of `mermaid-viewer-core`: an
//! [`rmcp::ServerHandler`] exposing exactly two tools over the MCP
//! protocol (server name `mermaid-viewer`):
//!
//! - **`show_diagram {code, title?, scale?}`** — renders the Mermaid
//!   source to PNG and shows it in the persistent diagram window: the
//!   first call opens the window (`{"status":"opened"}`), later calls
//!   hot-swap the content (`{"status":"updated"}`). A rejected source is a
//!   *tool-level* error result (`isError: true`, content carrying the
//!   render message) — never a protocol error, never a panic — so the
//!   server keeps running and serves the next request.
//! - **`close_diagram {}`** — closes the window; idempotent; always
//!   returns `{"closed":true}`.
//!
//! ## Window ownership
//!
//! `DiagramServer` owns the window lazily: the [`WindowController`] is
//! created on the first `show_diagram` (with the provided `title`, or the
//! default title) and `close_diagram` closes it. The controller handle is
//! kept across closes so the window stays re-openable on the same GUI
//! thread (the [`crate::view`] state machine:
//! `CLOSED --update--> OPEN --update--> OPEN`, `any --close--> CLOSED`).
//!
//! ## Thread model
//!
//! The tool logic is async and transport-free: the MCP `tools/call`
//! surface is a thin `#[tool]` macro wrapper around the directly-callable
//! [`DiagramServer::show`] / [`DiagramServer::close`] methods.
//!
//! - Rendering runs in `tokio::task::spawn_blocking` (the render pipeline
//!   is synchronous and pure).
//! - The window state transition (lazy spawn, `is_open` check, `update`)
//!   is serialized under a mutex and also runs in `spawn_blocking`,
//!   because a first `update` may block until the GUI thread has actually
//!   opened the window. In-flight `show_diagram` calls therefore commit in
//!   enqueue order and their `opened`/`updated` statuses always agree with
//!   the window state machine.
//!
//! `DiagramServer` is `Send + Sync + 'static` as required by
//! [`rmcp::ServerHandler`] (the window handle talks to the GUI thread over
//! a plain-data channel).
//!
//! The MCP transport itself (stdio, tokio runtime, clean shutdown) is the
//! CLI's job — this module only provides the handler and the tool logic.
//! `MERMAID_VIEWER_FAKE_WINDOW=1` selects the headless fake window backend
//! at spawn time, for CI.

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

/// The default window title when `show_diagram` omits `title`.
const DEFAULT_TITLE: &str = "mermaid-viewer";

/// The default PNG scale when `show_diagram` omits `scale` (matches
/// [`RenderOptions::default()`]).
const DEFAULT_PNG_SCALE: f64 = 2.0;

/// The MCP server: the two diagram tools on a lazily-owned diagram window.
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
            .controller
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(WindowController::is_open)
    }

    /// Directly-callable `show_diagram` logic (no MCP transport).
    ///
    /// Renders `params.code` to PNG at `params.scale` (default
    /// [`DEFAULT_PNG_SCALE`], i.e. the same default as
    /// [`RenderOptions::default()`]) and shows it in the window: the first
    /// call opens the window (status [`ShowStatus::Opened`]), later calls
    /// hot-swap the content ([`ShowStatus::Updated`]).
    ///
    /// Render and window failures surface as typed
    /// [`crate::error::Error`]s — never a panic, never a bogus `Ok`.
    pub async fn show(&self, params: &ShowDiagramParams) -> CoreResult<ShowDiagramOutcome> {
        let code = params.code.clone();
        let scale = params.scale.unwrap_or(DEFAULT_PNG_SCALE);

        // 1. Render (synchronous + pure) off the tokio worker threads.
        let rendered = tokio::task::spawn_blocking(move || {
            NativeRenderer::new().render(
                &code,
                ImageFormat::Png,
                &RenderOptions { png_scale: scale },
            )
        })
        .await
        .map_err(|e| Error::View(format!("render task did not complete: {e}")))?;
        let image = rendered?;

        // 2. Commit the window state transition off the worker threads too
        //    (a first `update` blocks until the GUI thread opens the window).
        let state = Arc::clone(&self.state);
        let title = params.title.clone();
        let status = tokio::task::spawn_blocking(move || state.commit_show(&image, title))
            .await
            .map_err(|e| Error::View(format!("window task did not complete: {e}")))?;
        let status = status?;

        Ok(ShowDiagramOutcome { status })
    }

    /// Directly-callable `close_diagram` logic (no MCP transport).
    ///
    /// Closes the window if one exists; idempotent — always reports
    /// `closed: true` (a server that never showed a window has nothing to
    /// close, which is still a successful close).
    pub async fn close(&self) -> CloseDiagramOutcome {
        self.state.commit_close();
        CloseDiagramOutcome { closed: true }
    }
}

/// Interior state: the lazily-created window controller.
///
/// The mutex serializes the window state transitions (lazy spawn,
/// `is_open` check, `update`, `close`) so concurrent `show_diagram` calls
/// commit in enqueue order and their `opened`/`updated` statuses stay
/// consistent with the window state machine.
#[derive(Default)]
struct State {
    controller: Mutex<Option<WindowController>>,
}

impl State {
    /// Commits one rendered diagram to the window: opens the (lazily
    /// created) window with it, or hot-swaps the content, and reports which
    /// happened.
    fn commit_show(&self, image: &RenderedImage, title: Option<String>) -> CoreResult<ShowStatus> {
        let mut guard = self.controller.lock().unwrap_or_else(|e| e.into_inner());
        let controller = match guard.as_mut() {
            Some(controller) => controller,
            None => {
                let title = title.unwrap_or_else(|| DEFAULT_TITLE.to_string());
                *guard = Some(WindowController::spawn(&title)?);
                guard.as_mut().expect("controller was just inserted")
            }
        };

        let was_open = controller.is_open();
        controller.update(image)?;
        Ok(if was_open {
            ShowStatus::Updated
        } else {
            ShowStatus::Opened
        })
    }

    /// Closes the window if one exists. Idempotent no-op otherwise.
    fn commit_close(&self) {
        let guard = self.controller.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(controller) = guard.as_ref() {
            controller.close();
        }
    }
}

/// `show_diagram` parameters: `code` is required, `title`/`scale` optional.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShowDiagramParams {
    /// The Mermaid diagram source to render and display.
    pub code: String,
    /// Window title, used when the window is created. Ignored while the
    /// window already exists (the title is fixed at window creation).
    #[serde(default)]
    pub title: Option<String>,
    /// PNG pixel scale for the window image (default 2.0).
    #[serde(default)]
    pub scale: Option<f64>,
}

/// The `show_diagram` status: this call opened the window, or it hot-swapped
/// an already-open window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ShowStatus {
    /// The window was opened by this call.
    Opened,
    /// The window was already open; its content was replaced.
    Updated,
}

/// The `show_diagram` JSON body: `{"status":"opened"|"updated"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ShowDiagramOutcome {
    /// Whether this call opened the window (`opened`) or updated an open
    /// one (`updated`).
    pub status: ShowStatus,
}

/// The `close_diagram` JSON body: `{"closed":true}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CloseDiagramOutcome {
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
    /// Renders `code` and shows it in the persistent diagram window (opens
    /// it on the first call, hot-swaps the content afterwards).
    ///
    /// Body: `{"status":"opened"|"updated"}`. A rejected diagram source is
    /// a tool-level error result (`isError: true`) carrying the render
    /// message — the server keeps running.
    #[tool(
        name = "show_diagram",
        description = "Render a Mermaid diagram and show it in the persistent diagram window: opens the window on the first call, hot-swaps the content afterwards. Returns {\"status\":\"opened\"|\"updated\"}. A rejected diagram source is reported as a tool error and the server keeps running."
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

    /// Closes the diagram window.
    ///
    /// Idempotent: closing an already-closed window (or a server that never
    /// opened one) also succeeds. Body: `{"closed":true}`.
    #[tool(
        name = "close_diagram",
        description = "Close the diagram window. Idempotent: closing an already-closed window also succeeds. Returns {\"closed\":true}."
    )]
    pub async fn close_diagram(&self) -> Result<CallToolResponse, ErrorData> {
        let outcome = self.close().await;
        Ok(CallToolResult::success(vec![ContentBlock::json(outcome)?]).into())
    }
}

/// The rmcp server handler for [`DiagramServer`]: the exact two tools above,
/// served under the server name `mermaid-viewer`.
#[tool_handler(name = "mermaid-viewer")]
impl ServerHandler for DiagramServer {}
