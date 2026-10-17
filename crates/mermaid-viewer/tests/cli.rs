//! Shared verification plan [M-2.b] — process-level tests for the CLI
//! `render` subcommand, exercising the real binary end-to-end:
//! file/stdin/code sources → SVG/PNG → file/stdout, plus the exit-code
//! and stdout/stderr hygiene contract (diagnostics on stderr only).

use std::io::Write;
use std::process::Stdio;

const FLOWCHART: &str = "graph TD\n  A-->B";
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

fn bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_mermaid-viewer"))
}

/// Builder's choice for the tempdir mechanism: `std::env::temp_dir` with
/// a per-test, process- and time-unique directory name, removed on drop
/// (no new test dependency).
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "mermaid-viewer-cli-{tag}-{}-{now}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Named: `cli_render_svg_to_file` — a temp input flowchart rendered with
/// `-o <tmp>`: the output file exists and contains `<svg`, stdout is
/// empty, exit 0.
#[test]
fn cli_render_svg_to_file() {
    let dir = TempDir::new("svg-file");
    let input = dir.path("diagram.mmd");
    std::fs::write(&input, FLOWCHART).expect("write input file");
    let output = dir.path("diagram.svg");

    let out = std::process::Command::new(bin())
        .arg("render")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .output()
        .expect("spawn mermaid-viewer");

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "stdout must be empty when writing to a file: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let svg = std::fs::read_to_string(&output).expect("output file must exist and be valid UTF-8");
    assert!(svg.contains("<svg"), "output must be an SVG document");
}

/// Named: `cli_render_png_to_stdout` — `--code … --format png -o -`
/// yields PNG bytes on stdout, exit 0.
#[test]
fn cli_render_png_to_stdout() {
    let out = std::process::Command::new(bin())
        .args(["render", "--code", FLOWCHART, "--format", "png", "-o", "-"])
        .output()
        .expect("spawn mermaid-viewer");

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.len() >= PNG_SIGNATURE.len(),
        "stdout too small for a PNG: {} bytes",
        out.stdout.len()
    );
    assert_eq!(
        &out.stdout[..PNG_SIGNATURE.len()],
        &PNG_SIGNATURE,
        "stdout must start with the PNG signature"
    );
    assert!(
        out.stderr.is_empty(),
        "no diagnostics expected on success: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Named: `cli_render_stdin` — a piped stdin source with `--stdin` exits
/// 0 and writes a valid SVG document to stdout (the default format).
#[test]
fn cli_render_stdin() {
    let mut child = std::process::Command::new(bin())
        .args(["render", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mermaid-viewer");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(FLOWCHART.as_bytes())
        .expect("write diagram source to stdin");
    let out = child.wait_with_output().expect("wait for mermaid-viewer");

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let svg = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        svg.starts_with("<?xml") || svg.starts_with("<svg"),
        "stdout must be a full SVG document: {:?}",
        &svg[..svg.len().min(60)]
    );
    assert!(
        svg.contains("A") && svg.contains("B"),
        "missing node labels: {svg}"
    );
}

/// Named: `cli_render_bad_input_exits_1` — a source the parser rejects
/// exits 1, with `mermaid render failed` on stderr and empty stdout.
///
/// Fixture choice (M-2 contract card, builder's choice): mermaid-render
/// 0.10 is lenient about unknown type tokens (`!!!not mermaid!!!` warns
/// and falls back to a flowchart parse, then renders), so this test uses
/// a source verified to be rejected by mermaid-render 0.10 — the same
/// fixture class as the core test `render_invalid_source_is_error`.
#[test]
fn cli_render_bad_input_exits_1() {
    let out = std::process::Command::new(bin())
        .args(["render", "--code", "gitGraph\n  frobnicate"])
        .output()
        .expect("spawn mermaid-viewer");

    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("mermaid render failed"),
        "stderr must name the render failure: {stderr}"
    );
    assert!(
        out.stdout.is_empty(),
        "stdout must be empty when rendering fails: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// Shared verification plan [M-2.b]: `view_watch_fake_env_smoke` —
/// spawn the real binary with `MERMAID_VIEWER_FAKE_WINDOW=1` and
/// `view --watch <tempfile>`: the watch path renders the initial
/// diagram, opens the update channel, spawns the watcher thread, and
/// runs the fake update-drain window, which returns after draining —
/// exit 0, no display, no hang: `main`'s `std::process::exit` does not
/// wait on the (blocking) watcher thread.
#[test]
fn view_watch_fake_env_smoke() {
    let dir = TempDir::new("watch-smoke");
    let input = dir.path("diagram.mmd");
    std::fs::write(&input, FLOWCHART).expect("write input file");

    let out = std::process::Command::new(bin())
        .env("MERMAID_VIEWER_FAKE_WINDOW", "1")
        .arg("view")
        .arg("--watch")
        .arg(&input)
        .output()
        .expect("spawn mermaid-viewer");

    assert_eq!(
        out.status.code(),
        Some(0),
        "view --watch under the fake window backend must exit 0; \
         stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "no diagnostics expected on a clean watch startup: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
