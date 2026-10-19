//! Shared verification plans [M-2.b] and [M-3] — process-level tests for
//! the CLI `render` and `view` subcommands, exercising the real binary
//! end-to-end: file/stdin/code sources → SVG/PNG → file/stdout, the
//! `view` Markdown tabbed path (fake window backend) and its exit-code
//! and stdout/stderr hygiene contract (diagnostics on stderr only).

use std::io::Write;
use std::process::Stdio;

const FLOWCHART: &str = "graph TD\n  A-->B";
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Markdown with prose surrounding two valid `mermaid`-fenced blocks
/// (the M-3 tabbed `view` fixture).
const MARKDOWN_TWO_BLOCKS: &str = "## Intro\n\nSome prose.\n\n```mermaid\ngraph TD\n  A-->B\n```\n\n\
                                   More prose.\n\n```mermaid\nflowchart LR\n  x --> y\n```\n\nDone.\n";

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

/// Named (M-3): `cli_view_markdown_fake_env_smoke` — a temp `.md` with
/// prose and two valid mermaid blocks, viewed with
/// `MERMAID_VIEWER_FAKE_WINDOW=1`: the content-based routing takes the
/// tabbed path (both blocks rendered to SVG, shown as tabs in the fake
/// controller, `wait_closed` returns immediately) — exit 0, stdout empty,
/// stderr empty.
#[test]
fn cli_view_markdown_fake_env_smoke() {
    let dir = TempDir::new("md-smoke");
    let input = dir.path("diagram.md");
    std::fs::write(&input, MARKDOWN_TWO_BLOCKS).expect("write markdown file");

    let out = std::process::Command::new(bin())
        .env("MERMAID_VIEWER_FAKE_WINDOW", "1")
        .arg("view")
        .arg(&input)
        .output()
        .expect("spawn mermaid-viewer");

    assert_eq!(
        out.status.code(),
        Some(0),
        "view on Markdown under the fake window backend must exit 0; \
         stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "stdout must be empty on the view path: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        out.stderr.is_empty(),
        "no diagnostics expected on a clean Markdown view: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Named (M-3): `cli_view_markdown_over_tab_limit_notes_omission` — 17
/// valid blocks (one over the 16-tab limit) viewed with the fake window
/// backend: the view still succeeds (exit 0) and stderr carries exactly
/// the one tab-limit note.
#[test]
fn cli_view_markdown_over_tab_limit_notes_omission() {
    let dir = TempDir::new("md-limit");
    let input = dir.path("many.md");
    let mut doc = String::new();
    for n in 1..=17 {
        doc.push_str(&format!("```mermaid\ngraph TD\n  A{n}-->B{n}\n```\n\n"));
    }
    std::fs::write(&input, doc).expect("write markdown file");

    let out = std::process::Command::new(bin())
        .env("MERMAID_VIEWER_FAKE_WINDOW", "1")
        .arg("view")
        .arg(&input)
        .output()
        .expect("spawn mermaid-viewer");

    assert_eq!(
        out.status.code(),
        Some(0),
        "an over-limit Markdown view must still succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "stdout must be empty on the view path: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("window tab limit"),
        "stderr must carry the tab-limit note: {stderr}"
    );
    assert_eq!(
        stderr,
        "mermaid-viewer: found 17 mermaid diagrams; showing the first 16 (window tab limit)\n",
        "exactly one tab-limit note, of the contracted form: {stderr:?}"
    );
}

/// Named (M-3): `cli_view_markdown_bad_block_exits_1` — one valid block
/// plus one parser-rejected block, viewed with the fake window backend:
/// the renders run *before* the window (the fake one never opens), so the
/// process exits 1 with `mermaid render failed` on stderr and empty
/// stdout. Same rejected fixture class as `cli_render_bad_input_exits_1`.
#[test]
fn cli_view_markdown_bad_block_exits_1() {
    let dir = TempDir::new("md-bad");
    let input = dir.path("bad.md");
    std::fs::write(
        &input,
        "```mermaid\ngraph TD\n  A-->B\n```\n\n```mermaid\ngitGraph\n  frobnicate\n```\n",
    )
    .expect("write markdown file");

    let out = std::process::Command::new(bin())
        .env("MERMAID_VIEWER_FAKE_WINDOW", "1")
        .arg("view")
        .arg(&input)
        .output()
        .expect("spawn mermaid-viewer");

    assert_eq!(
        out.status.code(),
        Some(1),
        "a parser-rejected Markdown block must exit 1; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("mermaid render failed"),
        "stderr must name the render failure: {stderr}"
    );
    assert!(
        out.stdout.is_empty(),
        "stdout must be empty when a Markdown block fails to render: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}
