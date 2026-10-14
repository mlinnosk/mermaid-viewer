//! [M-4.a] Unit tests for the MCP `DiagramServer` handler logic.
//!
//! Exercises the directly-callable tool logic of
//! `mermaid_viewer_core::mcp` **without any MCP transport**: the
//! `#[tool]`-generated `show_diagram` / `close_diagram` fns (the exact fns
//! the rmcp router dispatches `tools/call` to) are invoked in-process, and
//! the typed `DiagramServer::show` / `close` logic is checked alongside.
//!
//! All tests run under the fake window backend
//! (`MERMAID_VIEWER_FAKE_WINDOW=1`) — no display, no raylib, no stdio.
//!
//! Parallel-test safety: the fake-window env var is process-global, and
//! the fake backend reads it only at `WindowController::spawn` time. Each
//! test creates its own fresh `DiagramServer` (no window state is shared
//! between tests), and the env var is set exactly once per process — by
//! whichever test reaches the `Once` first — so no test thread ever races
//! a `set_var` against a concurrent read.
#![cfg(all(feature = "mcp", any(feature = "view", feature = "view-headless")))]

use std::sync::Once;

use mermaid_viewer_core::error::Error;
use mermaid_viewer_core::mcp::{DiagramServer, ShowDiagramOutcome, ShowDiagramParams, ShowStatus};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResponse, ContentBlock};

/// A valid flowchart (renders cleanly under mermaid-render 0.10).
const FLOWCHART: &str = "graph TD\n  A-->B";
/// A second valid flowchart (distinct content, for update calls).
const FLOWCHART_TWO: &str = "graph TD\n  A-->B-->C";
/// A source mermaid-render 0.10 rejects (verified fixture from [M-2.a]).
const REJECTED_SOURCE: &str = "gitGraph\n  frobnicate";

/// Sets `MERMAID_VIEWER_FAKE_WINDOW=1` exactly once per process.
fn ensure_fake_window_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| std::env::set_var("MERMAID_VIEWER_FAKE_WINDOW", "1"));
}

/// A fresh server: fake backend, window starts CLOSED.
fn fresh_server() -> DiagramServer {
    ensure_fake_window_env();
    DiagramServer::new()
}

fn params(code: &str, title: Option<&str>, scale: Option<f64>) -> ShowDiagramParams {
    ShowDiagramParams {
        code: code.to_string(),
        title: title.map(str::to_string),
        scale,
    }
}

/// Extracts the complete tool result from a `CallToolResponse`.
fn complete(response: &CallToolResponse) -> &rmcp::model::CallToolResult {
    match response {
        CallToolResponse::Complete(result) => result,
        other => panic!("expected a complete tool result, got {other:?}"),
    }
}

/// Unwraps a tool invocation: these tools never return a protocol error
/// (render failures are tool-level `isError` results), so a protocol error
/// here would be a server bug, not the system under test.
fn call(response: Result<CallToolResponse, rmcp::ErrorData>) -> CallToolResponse {
    response.expect("the tool returns a result; never a protocol error")
}

/// Parses the tool result's text content as JSON (the wire body).
fn body_json(response: &CallToolResponse) -> serde_json::Value {
    let text = complete(response)
        .content
        .iter()
        .find_map(ContentBlock::as_text)
        .expect("tool result carries a text content")
        .text
        .clone();
    serde_json::from_str(&text).expect("tool result text is JSON")
}

/// Named: `mcp_show_diagram_opens` — fresh fake server + valid flowchart
/// yields `{"status":"opened"}` and the controller reports the window open.
#[tokio::test]
async fn mcp_show_diagram_opens() {
    // Wire level: first `show_diagram` through the `#[tool]` fn.
    let server = fresh_server();
    assert!(
        !server.is_window_open(),
        "a fresh server starts with the window CLOSED"
    );

    let response = call(
        server
            .show_diagram(Parameters(params(FLOWCHART, None, None)))
            .await,
    );
    assert_eq!(
        complete(&response).is_error,
        Some(false),
        "a successful show is not a tool error"
    );
    assert_eq!(
        body_json(&response),
        serde_json::json!({ "status": "opened" })
    );
    assert!(
        server.is_window_open(),
        "the controller is open after the first show"
    );

    // Typed level: the directly-callable logic reports the same status.
    let other = fresh_server();
    let outcome = other
        .show(&params(FLOWCHART, None, None))
        .await
        .expect("valid flowchart renders");
    assert_eq!(
        outcome,
        ShowDiagramOutcome {
            status: ShowStatus::Opened
        }
    );
    assert!(other.is_window_open(), "typed show opens the window");
}

/// Named: `mcp_show_diagram_updates` — with the window already open, a
/// second `show_diagram` yields `{"status":"updated"}` (window stays open).
#[tokio::test]
async fn mcp_show_diagram_updates() {
    let server = fresh_server();

    let first = call(
        server
            .show_diagram(Parameters(params(FLOWCHART, Some("my diagram"), None)))
            .await,
    );
    assert_eq!(body_json(&first), serde_json::json!({ "status": "opened" }));
    assert!(server.is_window_open());

    let second = call(
        server
            .show_diagram(Parameters(params(
                FLOWCHART_TWO,
                Some("ignored while open"),
                Some(1.0),
            )))
            .await,
    );
    assert_eq!(
        complete(&second).is_error,
        Some(false),
        "a successful update is not a tool error"
    );
    assert_eq!(
        body_json(&second),
        serde_json::json!({ "status": "updated" })
    );
    assert!(
        server.is_window_open(),
        "the window stays open after an update"
    );

    // Typed level on the same server: updates keep reporting `updated`.
    let outcome = server
        .show(&params(FLOWCHART, None, None))
        .await
        .expect("valid flowchart renders");
    assert_eq!(outcome.status, ShowStatus::Updated);
}

/// Named: `mcp_close_diagram_idempotent` — `close_diagram` returns
/// `{"closed":true}` repeatedly (including on a server that never opened a
/// window), the window is closed, and a subsequent `show_diagram` opens it
/// again.
#[tokio::test]
async fn mcp_close_diagram_idempotent() {
    let server = fresh_server();

    // Closing a server that never showed anything is an idempotent no-op…
    let r0 = call(server.close_diagram().await);
    assert_eq!(
        complete(&r0).is_error,
        Some(false),
        "close is not a tool error"
    );
    assert_eq!(body_json(&r0), serde_json::json!({ "closed": true }));
    assert!(!server.is_window_open());

    // …and so are the first and second closes of an opened window.
    let opened = call(
        server
            .show_diagram(Parameters(params(FLOWCHART, None, None)))
            .await,
    );
    assert_eq!(
        body_json(&opened),
        serde_json::json!({ "status": "opened" })
    );

    let r1 = call(server.close_diagram().await);
    assert_eq!(body_json(&r1), serde_json::json!({ "closed": true }));
    assert!(
        !server.is_window_open(),
        "the window is closed after close_diagram"
    );

    let r2 = call(server.close_diagram().await);
    assert_eq!(body_json(&r2), serde_json::json!({ "closed": true }));
    assert!(!server.is_window_open(), "a second close is a no-op");

    // A subsequent show re-opens the window (the controller survives close).
    let reopened = call(
        server
            .show_diagram(Parameters(params(FLOWCHART, None, None)))
            .await,
    );
    assert_eq!(
        body_json(&reopened),
        serde_json::json!({ "status": "opened" })
    );
    assert!(server.is_window_open(), "the window re-opens after close");
}

/// Named: `mcp_show_diagram_render_error` — a rejected source yields a
/// tool-level error result (`isError: true`) carrying the render message —
/// not a protocol error, not a panic. The window stays closed, the server
/// survives, and a subsequent valid call opens the window.
#[tokio::test]
async fn mcp_show_diagram_render_error() {
    let server = fresh_server();

    let failed = call(
        server
            .show_diagram(Parameters(params(REJECTED_SOURCE, None, None)))
            .await,
    );
    let result = complete(&failed);
    assert_eq!(
        result.is_error,
        Some(true),
        "a rejected source is a tool-level error"
    );
    let message = result
        .content
        .iter()
        .find_map(ContentBlock::as_text)
        .expect("the error result carries text content")
        .text
        .clone();
    assert!(
        message.starts_with("mermaid render failed:"),
        "the render message surfaces in the result, got: {message}"
    );
    assert!(
        !server.is_window_open(),
        "the failed show did not open a window"
    );

    // The server survives: the next valid call opens the window.
    let recovered = call(
        server
            .show_diagram(Parameters(params(FLOWCHART, None, None)))
            .await,
    );
    assert_eq!(complete(&recovered).is_error, Some(false));
    assert_eq!(
        body_json(&recovered),
        serde_json::json!({ "status": "opened" })
    );
    assert!(
        server.is_window_open(),
        "the window opens after a failed show"
    );

    // Empty sources are rejected the same way…
    let empty = call(server.close_diagram().await);
    assert_eq!(body_json(&empty), serde_json::json!({ "closed": true }));
    let empty_failed = call(
        server
            .show_diagram(Parameters(params("", None, None)))
            .await,
    );
    assert_eq!(
        complete(&empty_failed).is_error,
        Some(true),
        "empty source is a tool-level error"
    );
    // …and the typed logic returns the typed `Render` error, never a panic.
    let other = fresh_server();
    let err = other
        .show(&params(REJECTED_SOURCE, None, None))
        .await
        .expect_err("a rejected source fails");
    assert!(
        matches!(err, Error::Render(_)),
        "a render failure is a typed Render error, got: {err}"
    );
    assert!(
        !other.is_window_open(),
        "the failed typed show did not open a window"
    );
}
