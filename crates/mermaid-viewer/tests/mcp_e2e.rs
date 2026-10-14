//! [M-4.b] Process-level e2e tests for the `mcp` subcommand.
//!
//! Each test spawns the real built binary (`mermaid-viewer mcp`) with
//! `MERMAID_VIEWER_FAKE_WINDOW=1` **set on the child only** (never in this
//! test process), and speaks the MCP protocol to it over the stdio pipe
//! with newline-delimited JSON-RPC: `initialize` (protocol version
//! `2025-11-25`), `notifications/initialized`, `tools/list`, `tools/call`.
//!
//! Every read is bounded: the server's stdout lines are pumped by a reader
//! thread into a channel that the tests read with a timeout, and the EOF
//! test reaps the child with a polling deadline — no test can hang on a
//! stuck server.
//!
//! Contract under test (parent [M-4] contract card):
//!
//! - server name `mermaid-viewer`; exactly two tools: `show_diagram`
//!   (`{code, title?, scale?}`) and `close_diagram` (`{}`);
//! - `show_diagram` → `{"status":"opened"}` on the first call,
//!   `{"status":"updated"}` afterwards; `close_diagram` → `{"closed":true}`;
//! - a stdin EOF is a clean shutdown: the server exits `0` and its stdout
//!   carries only MCP protocol frames.

use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::json;

/// The MCP protocol version these tests negotiate with the server.
const PROTOCOL_VERSION: &str = "2025-11-25";

/// A valid flowchart (renders cleanly under mermaid-render 0.10).
const FLOWCHART: &str = "graph TD\n  A-->B";
/// A second valid flowchart (distinct content, for update calls).
const FLOWCHART_TWO: &str = "graph TD\n  A-->B-->C";

/// Bounded timeout for any single read of a server response. Generous for
/// the first `show_diagram` (a debug-build mermaid-render + resvg pass) but
/// small enough that a stuck server fails the test quickly.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Bounded timeout for the child to exit on its own after stdin EOF.
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

fn bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_mermaid-viewer"))
}

/// One JSON-RPC request (has an `id`, expects a response).
fn request(id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// One JSON-RPC notification (no `id`, no response expected).
fn notification(method: &str) -> serde_json::Value {
    json!({ "jsonrpc": "2.0", "method": method })
}

/// `initialize` parameters for the handshake.
fn init_params() -> serde_json::Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": "mermaid-viewer-e2e", "version": "0.1.0" }
    })
}

/// The MCP `tools/call` request for `tool` with `arguments`.
fn tool_call(id: u64, tool: &str, arguments: serde_json::Value) -> serde_json::Value {
    request(
        id,
        "tools/call",
        json!({ "name": tool, "arguments": arguments }),
    )
}

/// Parses the `tools/call` result's first text content block as the JSON
/// tool body.
fn tool_body(result: &serde_json::Value) -> serde_json::Value {
    let text = result
        .get("content")
        .and_then(serde_json::Value::as_array)
        .and_then(|blocks| blocks.first())
        .and_then(|block| block.get("text"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("tool result carries text content: {result}"));
    serde_json::from_str(text).expect("tool result text is JSON")
}

/// Asserts a `tools/call` result is not a tool-level error.
fn assert_not_tool_error(result: &serde_json::Value, what: &str) {
    assert_ne!(
        result.get("isError"),
        Some(&serde_json::Value::Bool(true)),
        "{what} reported a tool error: {result}"
    );
}

/// A line-oriented MCP stdio client for one spawned `mermaid-viewer mcp`
/// child.
struct McpClient {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<Result<String, std::io::Error>>,
}

impl McpClient {
    /// Spawns `mermaid-viewer mcp` with `MERMAID_VIEWER_FAKE_WINDOW=1` set
    /// **on the child only** (the test process must stay display-free and
    /// must not leak the fake-window env to other tests).
    fn spawn() -> Self {
        let mut child = Command::new(bin())
            .arg("mcp")
            .env("MERMAID_VIEWER_FAKE_WINDOW", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mermaid-viewer mcp");
        let stdout = child.stdout.take().expect("stdout is piped");
        let (tx, rx) = mpsc::channel();
        // Pump server stdout lines into the channel; the test reads them
        // with a timeout. The thread unblocks on stdout EOF when the child
        // is killed in `Drop`.
        std::thread::spawn(move || {
            let stdout = std::io::BufReader::new(stdout);
            for line in stdout.lines() {
                if tx.send(line).is_err() {
                    break; // the test side is already done
                }
            }
        });
        let stdin = child.stdin.take().expect("stdin is piped");
        Self {
            child,
            stdin,
            lines: rx,
        }
    }

    /// Sends one JSON-RPC line to the server's stdin.
    fn send(&mut self, msg: serde_json::Value) {
        let mut line = serde_json::to_string(&msg).expect("serialize JSON-RPC");
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .expect("write to the child's stdin");
        self.stdin.flush().expect("flush the child's stdin");
    }

    /// Sends `initialize` + `notifications/initialized` and verifies the
    /// handshake: server name `mermaid-viewer` and the negotiated protocol
    /// version. Returns the initialize result.
    fn initialize(&mut self) -> serde_json::Value {
        self.send(request(1, "initialize", init_params()));
        let response = self.read_response(1);
        let result = response
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("initialize response carries a result: {response}"));
        assert_eq!(
            result
                .get("serverInfo")
                .and_then(|info| info.get("name"))
                .and_then(serde_json::Value::as_str),
            Some("mermaid-viewer"),
            "serverInfo.name must be `mermaid-viewer`: {result}"
        );
        assert_eq!(
            result
                .get("protocolVersion")
                .and_then(serde_json::Value::as_str),
            Some(PROTOCOL_VERSION),
            "the server must echo the negotiated protocol version: {result}"
        );
        self.send(notification("notifications/initialized"));
        result
    }

    /// Reads server lines (bounded by `READ_TIMEOUT`) until the JSON-RPC
    /// response for `id` arrives; server notifications are skipped.
    fn read_response(&mut self, id: u64) -> serde_json::Value {
        let deadline = Instant::now() + READ_TIMEOUT;
        loop {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1));
            let line = self
                .lines
                .recv_timeout(remaining)
                .unwrap_or_else(|_| {
                    panic!(
                        "no response for id {id} within {READ_TIMEOUT:?} \
                         (server stuck?)"
                    )
                })
                .unwrap_or_else(|e| panic!("reading server stdout failed: {e}"));
            let value: serde_json::Value = serde_json::from_str(&line).unwrap_or_else(|e| {
                panic!("server stdout is not a JSON-RPC frame ({e}): {line:?}")
            });
            if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return value;
            }
            // A notification (no `id`) — none are expected on this surface;
            // skip it and keep waiting for our response.
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Kill first so a failed test never leaks a running server, then
        // reap; the reader thread unblocks when stdout hits EOF.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Named: `e2e_initialize_lists_two_tools` — over the real stdio pipe,
/// `initialize` (protocol version `2025-11-25`) reports server name
/// `mermaid-viewer`, and `tools/list` reports exactly the two contract
/// tools: `show_diagram` and `close_diagram`.
#[test]
fn e2e_initialize_lists_two_tools() {
    let mut client = McpClient::spawn();
    client.initialize();

    client.send(request(2, "tools/list", json!({})));
    let response = client.read_response(2);
    let tools = response
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| panic!("tools/list response carries a tools array: {response}"));

    assert_eq!(tools.len(), 2, "exactly two tools, got: {tools:?}");
    let names: BTreeSet<&str> = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(serde_json::Value::as_str))
        .collect();
    let expected: BTreeSet<&str> = ["close_diagram", "show_diagram"].iter().copied().collect();
    assert_eq!(
        names, expected,
        "the tool names must be exactly show_diagram + close_diagram"
    );

    // `show_diagram` takes exactly `{code, title?, scale?}` per the contract.
    let show = tools
        .iter()
        .find(|tool| tool.get("name").and_then(serde_json::Value::as_str) == Some("show_diagram"))
        .expect("show_diagram is listed");
    let schema = show
        .get("inputSchema")
        .and_then(|s| s.get("properties"))
        .and_then(serde_json::Value::as_object)
        .expect("show_diagram has an inputSchema with properties");
    let schema_names: BTreeSet<&str> = schema.keys().map(String::as_str).collect();
    assert_eq!(
        schema_names,
        BTreeSet::from(["code", "scale", "title"]),
        "show_diagram parameters are exactly code, title?, scale?"
    );
    let required: Vec<&str> = show
        .get("inputSchema")
        .and_then(|s| s.get("required"))
        .and_then(serde_json::Value::as_array)
        .map(|req| req.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();
    assert_eq!(
        required,
        vec!["code"],
        "only `code` is required for show_diagram"
    );
}

/// Named: `e2e_show_and_close_roundtrip` — over the real stdio pipe,
/// `show_diagram` opens the window (`{"status":"opened"}`), a second
/// `show_diagram` hot-swaps it (`{"status":"updated"}`), and
/// `close_diagram` reports `{"closed":true}`.
#[test]
fn e2e_show_and_close_roundtrip() {
    let mut client = McpClient::spawn();
    client.initialize();

    // First show: opens the (fake) window.
    client.send(tool_call(2, "show_diagram", json!({ "code": FLOWCHART })));
    let opened = client.read_response(2);
    let result = opened
        .get("result")
        .cloned()
        .unwrap_or_else(|| panic!("show_diagram response carries a result: {opened}"));
    assert_not_tool_error(&result, "first show_diagram");
    assert_eq!(
        tool_body(&result),
        json!({ "status": "opened" }),
        "the first show opens the window"
    );

    // Second show: hot-swaps the open window.
    client.send(tool_call(
        3,
        "show_diagram",
        json!({ "code": FLOWCHART_TWO, "title": "ignored while open", "scale": 1.0 }),
    ));
    let updated = client.read_response(3);
    let result = updated
        .get("result")
        .cloned()
        .unwrap_or_else(|| panic!("show_diagram response carries a result: {updated}"));
    assert_not_tool_error(&result, "second show_diagram");
    assert_eq!(
        tool_body(&result),
        json!({ "status": "updated" }),
        "a subsequent show updates the open window"
    );

    // Close: idempotent, always reports closed.
    client.send(tool_call(4, "close_diagram", json!({})));
    let closed = client.read_response(4);
    let result = closed
        .get("result")
        .cloned()
        .unwrap_or_else(|| panic!("close_diagram response carries a result: {closed}"));
    assert_not_tool_error(&result, "close_diagram");
    assert_eq!(
        tool_body(&result),
        json!({ "closed": true }),
        "close_diagram reports closed: true"
    );
}

/// Named: `e2e_stdin_eof_exits_zero` — with no request in flight, closing
/// the child's stdin (EOF) is a clean shutdown: the server exits `0`
/// within a bounded timeout, its stdout carries no non-protocol output
/// (nothing was sent, so it must be empty), and stderr stays clean under
/// the fake window backend.
#[test]
fn e2e_stdin_eof_exits_zero() {
    let mut child = Command::new(bin())
        .arg("mcp")
        .env("MERMAID_VIEWER_FAKE_WINDOW", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mermaid-viewer mcp");

    // Drop the only owned `ChildStdin` handle: closing the write end of
    // the pipe delivers EOF to the server's stdin.
    let stdin = child.stdin.take().expect("stdin is piped");
    drop(stdin);

    // Bounded exit: poll `try_wait` (no unblocking `wait`). Once the child
    // has exited, `wait_with_output` collects its buffered pipe output.
    let deadline = Instant::now() + EXIT_TIMEOUT;
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None => {
                assert!(
                    Instant::now() < deadline,
                    "mcp server did not exit within {EXIT_TIMEOUT:?} of stdin EOF"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    let output = child
        .wait_with_output()
        .expect("collect the child's output after it exited");

    assert_eq!(
        status.code(),
        Some(0),
        "stdin EOF must exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "stdout must carry only MCP protocol frames; nothing was requested, \
         so it must be empty: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.stderr.is_empty(),
        "no diagnostics expected on a clean EOF shutdown: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
