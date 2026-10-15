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
use mermaid_viewer_core::mcp::{
    CloseDiagramOutcome, CloseDiagramParams, DiagramServer, ShowDiagramOutcome, ShowDiagramParams,
    ShowStatus,
};
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

/// Show parameters with a *missing* tab ID (the effective ID is the
/// `"default"` tab, the pre-M-2 single-diagram workflow).
fn params(code: &str, title: Option<&str>, scale: Option<f64>) -> ShowDiagramParams {
    ShowDiagramParams {
        code: code.to_string(),
        id: None,
        title: title.map(str::to_string),
        scale,
    }
}

/// Show parameters with an explicit tab ID.
fn params_with_id(
    code: &str,
    id: &str,
    title: Option<&str>,
    scale: Option<f64>,
) -> ShowDiagramParams {
    ShowDiagramParams {
        code: code.to_string(),
        id: Some(id.to_string()),
        title: title.map(str::to_string),
        scale,
    }
}

/// Close parameters: `None` is the close-all form (the pre-M-2 behavior,
/// `{"closed":true}`); an `id` targets one tab.
fn close_params(id: Option<&str>) -> CloseDiagramParams {
    CloseDiagramParams {
        id: id.map(str::to_string),
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
/// yields `{"id":"default","status":"opened"}` and the controller reports
/// the window open.
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
        serde_json::json!({ "id": "default", "status": "opened" })
    );
    assert!(
        server.is_window_open(),
        "the controller is open after the first show"
    );

    // Typed level: the directly-callable logic reports the same ID/status.
    let other = fresh_server();
    let outcome = other
        .show(&params(FLOWCHART, None, None))
        .await
        .expect("valid flowchart renders");
    assert_eq!(
        outcome,
        ShowDiagramOutcome {
            id: "default".into(),
            status: ShowStatus::Opened
        }
    );
    assert!(other.is_window_open(), "typed show opens the window");
}

/// Named: `mcp_show_diagram_updates` — with the window already open, a
/// second `show_diagram` of the same tab ID yields
/// `{"id":"default","status":"updated"}` (window stays open).
#[tokio::test]
async fn mcp_show_diagram_updates() {
    let server = fresh_server();

    let first = call(
        server
            .show_diagram(Parameters(params(FLOWCHART, Some("my diagram"), None)))
            .await,
    );
    assert_eq!(
        body_json(&first),
        serde_json::json!({ "id": "default", "status": "opened" })
    );
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
        serde_json::json!({ "id": "default", "status": "updated" })
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
    assert_eq!(outcome.id, "default");
    assert_eq!(outcome.status, ShowStatus::Updated);
}

/// Named: `mcp_show_diagram_adds_tab` — a fresh server opens the window
/// with the default tab (`opened`); a second tab ID is `added` to the
/// already-open window; re-showing *either* existing ID is `updated`
/// (wire and typed levels).
#[tokio::test]
async fn mcp_show_diagram_adds_tab() {
    let server = fresh_server();

    // Omitted ID: the default tab opens the window.
    let first = call(
        server
            .show_diagram(Parameters(params(FLOWCHART, None, None)))
            .await,
    );
    assert_eq!(
        body_json(&first),
        serde_json::json!({ "id": "default", "status": "opened" })
    );
    assert!(server.is_window_open());

    // A second ID adds a tab to the open window…
    let second = call(
        server
            .show_diagram(Parameters(params_with_id(
                FLOWCHART_TWO,
                "second",
                None,
                None,
            )))
            .await,
    );
    assert_eq!(
        complete(&second).is_error,
        Some(false),
        "a successful add is not a tool error"
    );
    assert_eq!(
        body_json(&second),
        serde_json::json!({ "id": "second", "status": "added" })
    );
    assert!(
        server.is_window_open(),
        "the window stays open with two tabs"
    );

    // …and re-showing *either* existing ID replaces it: `updated`.
    let again_default = call(
        server
            .show_diagram(Parameters(params_with_id(FLOWCHART, "default", None, None)))
            .await,
    );
    assert_eq!(
        body_json(&again_default),
        serde_json::json!({ "id": "default", "status": "updated" })
    );
    let again_second = call(
        server
            .show_diagram(Parameters(params_with_id(FLOWCHART, "second", None, None)))
            .await,
    );
    assert_eq!(
        body_json(&again_second),
        serde_json::json!({ "id": "second", "status": "updated" })
    );

    // Typed level: the same statuses at the directly-callable API, with
    // `title` acting as the tab label (never the ID).
    let outcome = server
        .show(&params_with_id(
            FLOWCHART_TWO,
            "second",
            Some("second label"),
            None,
        ))
        .await
        .expect("valid flowchart renders");
    assert_eq!(
        outcome,
        ShowDiagramOutcome {
            id: "second".into(),
            status: ShowStatus::Updated
        }
    );
    let outcome = server
        .show(&params_with_id(FLOWCHART_TWO, "third", None, None))
        .await
        .expect("valid flowchart renders");
    assert_eq!(
        outcome,
        ShowDiagramOutcome {
            id: "third".into(),
            status: ShowStatus::Added
        }
    );
}

/// Named: `mcp_close_diagram_idempotent` — `close_diagram` with an omitted
/// ID (close-all) returns the exact `{"closed":true}` body repeatedly
/// (including on a server that never opened a window and on an
/// already-closed server), the window is closed, and a subsequent
/// `show_diagram` opens it again from a cleared registry (`opened`, never
/// `updated`).
#[tokio::test]
async fn mcp_close_diagram_idempotent() {
    let server = fresh_server();

    // Closing a server that never showed anything is an idempotent no-op…
    let r0 = call(server.close_diagram(Parameters(close_params(None))).await);
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
        serde_json::json!({ "id": "default", "status": "opened" })
    );

    let r1 = call(server.close_diagram(Parameters(close_params(None))).await);
    assert_eq!(
        body_json(&r1),
        serde_json::json!({ "closed": true }),
        "the close-all body is exactly {{\"closed\":true}} — no id, never null"
    );
    assert!(
        !server.is_window_open(),
        "the window is closed after close_diagram"
    );

    let r2 = call(server.close_diagram(Parameters(close_params(None))).await);
    assert_eq!(
        body_json(&r2),
        serde_json::json!({ "closed": true }),
        "a second close on an already-closed server is a no-op"
    );
    assert!(!server.is_window_open(), "a second close is a no-op");

    // A subsequent show re-opens the window (the controller survives
    // close) and reports `opened` — the close-all commit cleared the
    // tab-ID registry, so this is never a spurious `updated`.
    let reopened = call(
        server
            .show_diagram(Parameters(params(FLOWCHART, None, None)))
            .await,
    );
    assert_eq!(
        body_json(&reopened),
        serde_json::json!({ "id": "default", "status": "opened" })
    );
    assert!(server.is_window_open(), "the window re-opens after close");
}

/// Named: `mcp_close_diagram_by_id` — a targeted close removes exactly
/// one tab: exact `{"id":"…","closed":true}` body, the survivor preserved
/// (window stays open; re-showing it reports `updated`), and a subsequent
/// close-all closes the window. Typed mirror of both forms: the targeted
/// outcome echoes the requested ID, the close-all outcome omits it.
#[tokio::test]
async fn mcp_close_diagram_by_id() {
    let server = fresh_server();

    // Open two tabs.
    let a = call(
        server
            .show_diagram(Parameters(params_with_id(FLOWCHART, "a", None, None)))
            .await,
    );
    assert_eq!(
        body_json(&a),
        serde_json::json!({ "id": "a", "status": "opened" })
    );
    let b = call(
        server
            .show_diagram(Parameters(params_with_id(FLOWCHART_TWO, "b", None, None)))
            .await,
    );
    assert_eq!(
        body_json(&b),
        serde_json::json!({ "id": "b", "status": "added" })
    );
    assert!(server.is_window_open());

    // Targeted close of "a": exact body, not a tool error.
    let closed = call(
        server
            .close_diagram(Parameters(close_params(Some("a"))))
            .await,
    );
    assert_eq!(
        complete(&closed).is_error,
        Some(false),
        "a targeted close is not a tool error"
    );
    assert_eq!(
        body_json(&closed),
        serde_json::json!({ "id": "a", "closed": true })
    );

    // The survivor is preserved: the window is open and "b" is still
    // registered (re-show reports `updated`, never a fresh `opened`).
    assert!(
        server.is_window_open(),
        "the window stays open with the surviving tab"
    );
    let b_again = call(
        server
            .show_diagram(Parameters(params_with_id(FLOWCHART_TWO, "b", None, None)))
            .await,
    );
    assert_eq!(
        body_json(&b_again),
        serde_json::json!({ "id": "b", "status": "updated" })
    );

    // Close-all then closes the window (and remains idempotent).
    let all = call(server.close_diagram(Parameters(close_params(None))).await);
    assert_eq!(
        body_json(&all),
        serde_json::json!({ "closed": true }),
        "the close-all body is exactly {{\"closed\":true}}"
    );
    assert!(!server.is_window_open(), "close-all closes the window");

    // Typed mirror: targeted close echoes the requested ID; close-all
    // omits it entirely (never `null`).
    let other = fresh_server();
    other
        .show(&params_with_id(FLOWCHART, "a", None, None))
        .await
        .expect("valid flowchart renders");
    other
        .show(&params_with_id(FLOWCHART_TWO, "b", None, None))
        .await
        .expect("valid flowchart renders");
    let outcome = other
        .close(&close_params(Some("a")))
        .await
        .expect("the targeted typed close succeeds");
    assert_eq!(
        outcome,
        CloseDiagramOutcome {
            id: Some("a".into()),
            closed: true
        }
    );
    assert!(
        other.is_window_open(),
        "the surviving tab keeps the window open (typed)"
    );
    let outcome = other
        .close(&close_params(None))
        .await
        .expect("the typed close-all succeeds");
    assert_eq!(
        outcome,
        CloseDiagramOutcome {
            id: None,
            closed: true
        }
    );
    assert!(!other.is_window_open(), "the window is closed (typed)");
}

/// Named: `mcp_close_unknown_id_noop` — a targeted close of an unknown ID
/// is a successful idempotent no-op (`{"id":"nope","closed":true}`) that
/// leaves the existing tabs untouched: the window stays open and
/// re-showing the tab reports `updated`.
#[tokio::test]
async fn mcp_close_unknown_id_noop() {
    let server = fresh_server();
    let a = call(
        server
            .show_diagram(Parameters(params_with_id(FLOWCHART, "a", None, None)))
            .await,
    );
    assert_eq!(
        body_json(&a),
        serde_json::json!({ "id": "a", "status": "opened" })
    );

    let closed = call(
        server
            .close_diagram(Parameters(close_params(Some("nope"))))
            .await,
    );
    assert_eq!(
        complete(&closed).is_error,
        Some(false),
        "closing an unknown ID is a successful no-op, not a tool error"
    );
    assert_eq!(
        body_json(&closed),
        serde_json::json!({ "id": "nope", "closed": true })
    );

    // Existing tabs unchanged: the window stays open and "a" is still
    // registered (re-show reports `updated`, not `opened`).
    assert!(
        server.is_window_open(),
        "the unknown targeted close did not close the window"
    );
    let a_again = call(
        server
            .show_diagram(Parameters(params_with_id(FLOWCHART_TWO, "a", None, None)))
            .await,
    );
    assert_eq!(
        body_json(&a_again),
        serde_json::json!({ "id": "a", "status": "updated" })
    );
}

/// Named: `mcp_show_diagram_tab_cap` — 16 tabs fill the window (the first
/// `opened`, the rest `added`); the 17th fresh ID is a *tool-level* error
/// naming the 16-tab limit (the typed logic: `Err(Error::View(_))`),
/// leaving the existing tabs intact (a known re-show reports `updated`);
/// a targeted close frees a slot, and the next fresh ID then succeeds
/// with `added` — the window is not corrupted and the server stays usable.
#[tokio::test]
async fn mcp_show_diagram_tab_cap() {
    let server = fresh_server();

    // 16 IDs succeed: the first opens the window, the rest add tabs.
    for i in 0..16u32 {
        let id = format!("tab-{i}");
        let response = call(
            server
                .show_diagram(Parameters(params_with_id(FLOWCHART, &id, None, None)))
                .await,
        );
        let status = if i == 0 { "opened" } else { "added" };
        assert_eq!(
            body_json(&response),
            serde_json::json!({ "id": id, "status": status }),
            "tab {i} ({id}) is {status}"
        );
    }
    assert!(server.is_window_open(), "the window is open at capacity");

    // The 17th fresh ID is a tool-level error naming the 16-tab limit…
    let over = call(
        server
            .show_diagram(Parameters(params_with_id(FLOWCHART, "tab-16", None, None)))
            .await,
    );
    let result = complete(&over);
    assert_eq!(
        result.is_error,
        Some(true),
        "a new tab beyond the 16-tab limit is a tool-level error"
    );
    let message = result
        .content
        .iter()
        .find_map(ContentBlock::as_text)
        .expect("the error result carries text content")
        .text
        .clone();
    assert!(
        message.contains("16-tab limit"),
        "the capacity error names the 16-tab limit, got: {message}"
    );

    // …and the typed logic returns the typed `View` error, never a panic.
    let err = server
        .show(&params_with_id(FLOWCHART, "tab-16", None, None))
        .await
        .expect_err("a 17th tab is rejected");
    assert!(
        matches!(err, Error::View(_)),
        "a capacity failure is a typed View error, got: {err}"
    );

    // The existing tabs are intact: a known re-show reports `updated`
    // (replacing a known tab always succeeds, at capacity included).
    let known = call(
        server
            .show_diagram(Parameters(params_with_id(
                FLOWCHART_TWO,
                "tab-0",
                None,
                None,
            )))
            .await,
    );
    assert_eq!(
        body_json(&known),
        serde_json::json!({ "id": "tab-0", "status": "updated" })
    );

    // A targeted close frees a slot…
    let closed = call(
        server
            .close_diagram(Parameters(close_params(Some("tab-15"))))
            .await,
    );
    assert_eq!(
        body_json(&closed),
        serde_json::json!({ "id": "tab-15", "closed": true })
    );

    // …and the next fresh ID then succeeds with `added`: the window is not
    // corrupted and the server stays usable.
    let fresh = call(
        server
            .show_diagram(Parameters(params_with_id(FLOWCHART, "tab-16", None, None)))
            .await,
    );
    assert_eq!(
        body_json(&fresh),
        serde_json::json!({ "id": "tab-16", "status": "added" })
    );
    assert!(server.is_window_open(), "the window is still usable");
}

/// Named: `mcp_show_diagram_render_error` — a rejected source yields a
/// tool-level error result (`isError: true`) carrying the render message —
/// not a protocol error, not a panic. The window stays closed, the server
/// survives, and a subsequent valid call opens the window. A failed show
/// also leaves the *other* tabs' registry state untouched: re-showing an
/// open tab after a failed show of another ID reports `updated`.
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
        serde_json::json!({ "id": "default", "status": "opened" })
    );
    assert!(
        server.is_window_open(),
        "the window opens after a failed show"
    );

    // Empty sources are rejected the same way…
    let empty = call(server.close_diagram(Parameters(close_params(None))).await);
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

    // Other tabs: a failed show of tab "b" leaves tab "a" registered —
    // re-showing "a" reports `updated`, never a spurious `opened`.
    let other = fresh_server();
    let a = call(
        other
            .show_diagram(Parameters(params_with_id(FLOWCHART, "a", None, None)))
            .await,
    );
    assert_eq!(
        body_json(&a),
        serde_json::json!({ "id": "a", "status": "opened" })
    );
    let b_failed = call(
        other
            .show_diagram(Parameters(params_with_id(REJECTED_SOURCE, "b", None, None)))
            .await,
    );
    assert_eq!(
        complete(&b_failed).is_error,
        Some(true),
        "the failed show of the other tab is a tool-level error"
    );
    let a_again = call(
        other
            .show_diagram(Parameters(params_with_id(FLOWCHART_TWO, "a", None, None)))
            .await,
    );
    assert_eq!(
        complete(&a_again).is_error,
        Some(false),
        "a valid re-show of the other tab succeeds"
    );
    assert_eq!(
        body_json(&a_again),
        serde_json::json!({ "id": "a", "status": "updated" })
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

/// Named: `mcp_rejects_empty_id` — a provided *empty* tab ID is a
/// tool-level error on *both* tools, naming the empty-ID problem: on show
/// the window stays closed, on close the open tabs are preserved; the
/// typed logic returns a typed `Error::View(_)` in both cases (no panic,
/// no render/commit attempt), and a subsequent valid call works.
#[tokio::test]
async fn mcp_rejects_empty_id() {
    let server = fresh_server();

    let response = call(
        server
            .show_diagram(Parameters(params_with_id(FLOWCHART, "", None, None)))
            .await,
    );
    let result = complete(&response);
    assert_eq!(
        result.is_error,
        Some(true),
        "an empty ID is a tool-level error"
    );
    let message = result
        .content
        .iter()
        .find_map(ContentBlock::as_text)
        .expect("the error result carries text content")
        .text
        .clone();
    assert!(
        message.contains("empty"),
        "the error message names the empty-ID problem, got: {message}"
    );
    assert!(
        !server.is_window_open(),
        "an empty-ID show does not open a window"
    );

    // The typed show logic rejects it too: a typed `View` error, never a
    // panic.
    let err = server
        .show(&params_with_id(FLOWCHART, "", None, None))
        .await
        .expect_err("an empty ID fails");
    assert!(
        matches!(err, Error::View(_)),
        "an empty ID is a typed View error, got: {err}"
    );

    // A subsequent valid show opens the window normally.
    let opened = call(
        server
            .show_diagram(Parameters(params(FLOWCHART, None, None)))
            .await,
    );
    assert_eq!(
        body_json(&opened),
        serde_json::json!({ "id": "default", "status": "opened" })
    );
    assert!(server.is_window_open());

    // Close side: an empty ID is a tool-level error naming the problem…
    let empty_close = call(
        server
            .close_diagram(Parameters(close_params(Some(""))))
            .await,
    );
    let result = complete(&empty_close);
    assert_eq!(
        result.is_error,
        Some(true),
        "an empty-ID close is a tool-level error"
    );
    let message = result
        .content
        .iter()
        .find_map(ContentBlock::as_text)
        .expect("the error result carries text content")
        .text
        .clone();
    assert!(
        message.contains("empty"),
        "the close error message names the empty-ID problem, got: {message}"
    );

    // …and the typed close logic rejects it with a typed `View` error.
    let err = server
        .close(&close_params(Some("")))
        .await
        .expect_err("an empty close ID fails");
    assert!(
        matches!(err, Error::View(_)),
        "an empty close ID is a typed View error, got: {err}"
    );

    // Tabs preserved: the window is still open, "default" is still
    // registered (re-show reports `updated`), and a close-all still works.
    assert!(
        server.is_window_open(),
        "the rejected close did not close the window"
    );
    let default_again = call(
        server
            .show_diagram(Parameters(params(FLOWCHART_TWO, None, None)))
            .await,
    );
    assert_eq!(
        body_json(&default_again),
        serde_json::json!({ "id": "default", "status": "updated" })
    );
    let all = call(server.close_diagram(Parameters(close_params(None))).await);
    assert_eq!(body_json(&all), serde_json::json!({ "closed": true }));
    assert!(
        !server.is_window_open(),
        "a close-all works after a rejected close"
    );
}
