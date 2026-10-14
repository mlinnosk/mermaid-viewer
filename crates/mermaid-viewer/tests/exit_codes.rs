//! Integration tests for the CLI exit-code contract, exercising the real
//! binary: `0` success/help, `1` runtime failure, `2` usage error.

fn bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_mermaid-viewer"))
}

/// `--help` and `--version` are successful invocations, not usage errors.
#[test]
fn help_and_version_exit_zero() {
    for args in [
        ["-h"].as_slice(),
        ["-V"].as_slice(),
        ["view", "-h"].as_slice(),
        ["render", "-h"].as_slice(),
        ["mcp", "-h"].as_slice(),
    ] {
        let status = std::process::Command::new(bin())
            .args(args)
            .status()
            .expect("spawn mermaid-viewer");
        assert!(status.success(), "{args:?} should exit 0, got {status}");
    }
}

/// A clap usage error (conflicting sources) exits 2 with a message on stderr.
#[test]
fn conflicting_sources_exit_two() {
    let output = std::process::Command::new(bin())
        .args(["render", "a.mmd", "--code", "x"])
        .output()
        .expect("spawn mermaid-viewer");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot be used with"), "stderr: {stderr}");
}

/// A missing source is a usage error and exits 2.
#[test]
fn missing_source_exits_two() {
    let status = std::process::Command::new(bin())
        .arg("view")
        .status()
        .expect("spawn mermaid-viewer");
    assert_eq!(status.code(), Some(2));
}

/// M-4.b: `mcp` is real. `output()` pipes the child's stdin from
/// `Stdio::null()`, so the server sees an immediate stdin EOF and must
/// exit 0 cleanly (no non-protocol output on stdout). The full protocol
/// behavior is covered by `tests/mcp_e2e.rs`.
#[test]
fn mcp_stdin_eof_exits_zero() {
    let mcp = std::process::Command::new(bin())
        .arg("mcp")
        .env("MERMAID_VIEWER_FAKE_WINDOW", "1")
        .output()
        .expect("spawn mermaid-viewer");
    assert_eq!(
        mcp.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&mcp.stderr)
    );
    assert!(
        mcp.stdout.is_empty(),
        "stdout must carry only MCP protocol frames; nothing was \
         requested, so it must be empty: {:?}",
        String::from_utf8_lossy(&mcp.stdout)
    );
}

/// Named (M-3.b): `cli_view_not_implemented_replaced` — the `view` stub is
/// gone. A real end-to-end display test is not possible without a display,
/// so the gate here is: the CLI `view` dispatch compiles (the binary
/// builds) and `view -h` is correct — exit 0 with the full flag surface
/// (`FILE`, `--code`, `--stdin`, `--title`, `--scale`).
#[test]
fn cli_view_not_implemented_replaced() {
    let out = std::process::Command::new(bin())
        .args(["view", "-h"])
        .output()
        .expect("spawn mermaid-viewer");
    assert_eq!(
        out.status.code(),
        Some(0),
        "view -h should exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for needle in [
        "Render a diagram and show it in a GUI window",
        "FILE",
        "--code",
        "--stdin",
        "--title",
        "--scale",
    ] {
        assert!(
            stdout.contains(needle),
            "view -h is missing {needle:?}: {stdout}"
        );
    }
}
