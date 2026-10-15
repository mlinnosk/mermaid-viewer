//! Entry point for the `mermaid-viewer` CLI.
//!
//! Exit-code contract:
//!
//! - `0` — success
//! - `1` — runtime/render/IO failure (message on stderr)
//! - `2` — usage error (clap prints the usage message to stderr itself)
//!
//! As of M-2.b the `render` handler is real end-to-end: source →
//! [`NativeRenderer`] → bytes written to stdout (`-o -`) or a file.
//! As of M-3.b the `view` handler is real: source → [`NativeRenderer`]
//! (PNG at `--scale`) → [`run_blocking`] on a GUI window titled
//! `--title`, blocking until the window is closed. As of M-4.b the `mcp`
//! handler is real: the core's [`DiagramServer`] is served on the stdio
//! transport over a multi-threaded tokio runtime, and the process exits
//! `0` on stdin EOF or Ctrl-C (closing the window first). stdout carries
//! only MCP protocol frames; every diagnostic goes to stderr via
//! [`main`]. Source reading ([`read_source`]) is real.
//!
//! Lint policy: `#![deny(unsafe_code)]` — the binary contains no `unsafe`
//! (all FFI lives in `mermaid-viewer-core`'s documented raylib boundary).

#![deny(unsafe_code)]

mod cli;

use mermaid_viewer_core::error::{Error, Result};
use mermaid_viewer_core::mcp::CloseDiagramParams;
use mermaid_viewer_core::{
    run_blocking, DiagramServer, ImageFormat, NativeRenderer, RenderOptions,
};

use crate::cli::{parse, Command, OutputFormat, RenderArgs, ViewArgs, BIN_NAME};

fn main() {
    let args = std::env::args().skip(1);
    let exit_code = match parse(args) {
        Ok(command) => match run(command) {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("{BIN_NAME}: {err}");
                1
            }
        },
        Err(clap_err) => {
            // `--help`/`--version` surface as clap errors that request exit
            // 0; every other usage error exits 2. clap prints the message.
            let is_help = matches!(
                clap_err.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            );
            let _ = clap_err.print();
            if is_help {
                0
            } else {
                2
            }
        }
    };
    std::process::exit(exit_code);
}

/// Run the parsed command.
///
/// `view` (M-3.b), `render` (M-2.b), and `mcp` (M-4.b) are real:
/// `view`/`render` read their source exactly once (stdin is single-shot,
/// so a double read would silently see an empty stream), render it with
/// [`NativeRenderer`], and then show it in a GUI window or write it to
/// stdout/a file (a missing file fails early with a typed [`Error::Io`]).
/// `mcp` takes no diagram input and serves the core's [`DiagramServer`]
/// over its own stdio transport.
fn run(command: Command) -> Result<()> {
    match command {
        // `mcp` takes no diagram input and owns its tokio runtime.
        Command::Mcp => run_mcp(),
        command => {
            let source = read_source(&command)?;
            match command {
                Command::View(args) => run_view(&source, &args),
                Command::Render(args) => run_render(&source, &args),
                // Ruled out by the match above.
                Command::Mcp => unreachable!("mcp returns before the source is read"),
            }
        }
    }
}

/// The `mcp` handler (M-4.b): a multi-threaded tokio runtime on the main
/// thread serves [`DiagramServer`] over the stdio transport
/// (`tokio::io::stdin()` / `tokio::io::stdout()`), so the process speaks
/// the MCP protocol to coding agents on a pipe.
///
/// Clean shutdown: a stdin EOF (the agent closed the pipe) ends the
/// service loop; Ctrl-C cancels it. On every shutdown path the diagram
/// window is closed *before* the process exits `0`. stdout carries only
/// MCP protocol frames — every diagnostic (handshake/runtime failure) is a
/// typed [`Error`] printed to stderr by [`main`].
fn run_mcp() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::View(format!("failed to build tokio runtime: {e}")))?;
    runtime.block_on(serve_mcp())
}

/// Serves the [`DiagramServer`] on stdio until shutdown (stdin EOF or
/// Ctrl-C), closing any open diagram window before returning.
async fn serve_mcp() -> Result<()> {
    // `Arc` because rmcp's `ServerHandler` has an `Arc` wrapper impl: the
    // same server instance is both handed to the transport *and* kept for
    // the explicit window close on shutdown.
    let server = std::sync::Arc::new(DiagramServer::new());

    // `serve_server` performs the `initialize` handshake. A stdin EOF
    // (transport closed) or a Ctrl-C before it can complete is a clean
    // shutdown, not a failure — there is at most no window to close.
    let running = match rmcp::serve_server(server.clone(), rmcp::transport::io::stdio()).await {
        Ok(running) => running,
        Err(err) => {
            let clean_shutdown = matches!(
                err,
                rmcp::service::ServerInitializeError::ConnectionClosed(_)
                    | rmcp::service::ServerInitializeError::Cancelled
            );
            if !clean_shutdown {
                return Err(Error::View(format!("MCP initialize failed: {err}")));
            }
            server.close(&CloseDiagramParams { id: None }).await?;
            return Ok(());
        }
    };

    // Ctrl-C → cancellation token → the service loop ends
    // (`QuitReason::Cancelled`).
    let cancellation = running.cancellation_token();
    let ctrl_c = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancellation.cancel();
        }
    });

    // stdin EOF (transport closed) or Ctrl-C ends the loop; a transport
    // error mid-session is the only failure path.
    let _reason = running
        .waiting()
        .await
        .map_err(|e| Error::View(format!("MCP service failed: {e}")))?;
    ctrl_c.abort();

    // Shutdown closes the window first (idempotent), then the process
    // exits `0`.
    server.close(&CloseDiagramParams { id: None }).await?;
    Ok(())
}

/// The `view` handler (M-3.b): `source` → [`NativeRenderer`] (PNG at
/// `--scale`) → [`run_blocking`], which shows the diagram in a GUI window
/// titled `--title` on the current thread and blocks until the window is
/// closed (ESC or window close).
///
/// The render runs *before* any window/GL initialization: a parser
/// rejection is a typed [`Error::Render`] (exit 1 in [`main`]) and no
/// window is ever opened. With `MERMAID_VIEWER_FAKE_WINDOW=1` the core's
/// fake window backend takes over, so the whole path runs without a
/// display (CI).
fn run_view(source: &str, args: &ViewArgs) -> Result<()> {
    let image = NativeRenderer::new().render(
        source,
        ImageFormat::Png,
        &RenderOptions {
            png_scale: args.scale,
        },
    )?;
    run_blocking(&args.title, &image)
}

/// The `render` handler: `source` → [`NativeRenderer`] → bytes to stdout
/// or a file.
///
/// * `-o -` writes the raw image bytes to stdout and nothing else to
///   stdout (diagnostics always go to stderr via [`main`]).
/// * Any other `-o` target is created with [`std::fs::File`] — no
///   parent-directory creation; a missing parent surfaces as
///   [`Error::Io`] (exit 1).
/// * `--scale` is passed through to the renderer, which applies it to PNG
///   only; SVG output ignores it by construction.
fn run_render(source: &str, args: &RenderArgs) -> Result<()> {
    let format = match args.format {
        OutputFormat::Svg => ImageFormat::Svg,
        OutputFormat::Png => ImageFormat::Png,
    };
    let image = NativeRenderer::new().render(
        source,
        format,
        &RenderOptions {
            png_scale: args.scale,
        },
    )?;
    write_render_output(&args.output, &image.bytes)
}

/// Write the rendered bytes to stdout when `output` is `-`, otherwise to
/// the file at `output`.
fn write_render_output(output: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if output == "-" {
        // Lock the stdout handle so a buffered write + flush is atomic
        // w.r.t. any other writer of the process's stdout.
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        lock.write_all(bytes)?;
        lock.flush()?;
        return Ok(());
    }
    // Contract: plain `std::fs::File` target — creating a file whose
    // parent directory does not exist is an `Error::Io`, not an attempt
    // to create the directory.
    let mut file = std::fs::File::create(output)?;
    file.write_all(bytes)?;
    Ok(())
}

/// Read the Mermaid source selected by a `view`/`render` command.
///
/// Exactly one source is always present: clap's required, mutually
/// exclusive `source` group enforces it before dispatch. File and stdin
/// I/O failures map to [`Error::Io`] via `#[from]`.
fn read_source(command: &Command) -> Result<String> {
    let (file, code, stdin) = match command {
        Command::View(args) => (args.file.as_deref(), args.code.as_deref(), args.stdin),
        Command::Render(args) => (args.file.as_deref(), args.code.as_deref(), args.stdin),
        // `mcp` takes no diagram input and never reaches this helper.
        Command::Mcp => {
            return Err(Error::Render("mcp takes no diagram input".to_string()));
        }
    };

    if let Some(path) = file {
        Ok(std::fs::read_to_string(path)?)
    } else if let Some(source) = code {
        Ok(source.to_string())
    } else if stdin {
        use std::io::Read as _;
        let mut source = String::new();
        std::io::stdin().read_to_string(&mut source)?;
        Ok(source)
    } else {
        // Defensive: clap already rejects a missing source with exit 2.
        Err(Error::Render("no diagram input source".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{OutputFormat, RenderArgs, ViewArgs};

    fn render_args(file: Option<std::path::PathBuf>, code: Option<&str>) -> RenderArgs {
        RenderArgs {
            file,
            code: code.map(str::to_string),
            stdin: false,
            output: "-".to_string(),
            format: OutputFormat::Svg,
            scale: 2.0,
        }
    }

    /// `--code` is returned verbatim without any I/O.
    #[test]
    fn read_source_from_code() {
        let command = Command::View(ViewArgs {
            file: None,
            code: Some("graph TD\n  A-->B".to_string()),
            stdin: false,
            title: DEFAULT_TITLE.to_string(),
            scale: 2.0,
        });
        assert_eq!(read_source(&command).unwrap(), "graph TD\n  A-->B");
    }

    /// A `FILE` source is read through `std::fs`.
    #[test]
    fn read_source_from_file() {
        let path = std::env::temp_dir().join(format!(
            "mermaid-viewer-src-{}-{}.mmd",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&path, "graph TD\n  A-->B\n").unwrap();
        let command = Command::Render(render_args(Some(path.clone()), None));
        assert_eq!(read_source(&command).unwrap(), "graph TD\n  A-->B\n");
        let _ = std::fs::remove_file(&path);
    }

    /// A missing file surfaces as a typed [`Error::Io`], never a panic.
    #[test]
    fn read_source_maps_missing_file_to_io_error() {
        let command = Command::Render(render_args(
            Some(std::path::PathBuf::from(
                "/nonexistent/mermaid-viewer/m.mmd",
            )),
            None,
        ));
        assert!(matches!(read_source(&command), Err(Error::Io(_))));
    }

    /// `mcp` has no diagram input.
    #[test]
    fn read_source_rejects_mcp() {
        assert!(matches!(read_source(&Command::Mcp), Err(Error::Render(_))));
    }

    // The `mcp` stub test is gone with the stub (M-4.b): `run(Command::Mcp)`
    // now serves a real stdio MCP server and cannot be exercised in-process
    // (it would block on the test's inherited stdin). The `mcp` path is
    // covered process-level by `tests/mcp_e2e.rs` (named e2e tests) and
    // `tests/exit_codes.rs` (EOF → exit 0), which spawn the real binary
    // with the core's fake window backend.

    /// M-3.b wiring, proven headless: with the core's fake window backend
    /// env var set, the full `view` path (`--code` → [`NativeRenderer`]
    /// PNG → [`run_blocking`]) succeeds without a display and without
    /// blocking, returning `Ok` when the (fake) window closes.
    #[test]
    fn run_view_fake_env_returns_ok() {
        std::env::set_var("MERMAID_VIEWER_FAKE_WINDOW", "1");
        let command = Command::View(ViewArgs {
            file: None,
            code: Some("graph TD\n  A-->B".to_string()),
            stdin: false,
            title: DEFAULT_TITLE.to_string(),
            scale: 2.0,
        });
        assert!(
            run(command).is_ok(),
            "view path must succeed under the fake window backend"
        );
    }

    /// Render failure is the first fallible step of the `view` path: a
    /// parser-rejected source is a typed [`Error::Render`] (exit 1 in
    /// [`main`]) and no window/GL init is even attempted. Same fixture
    /// class as `run_render_bad_source_is_render_error`.
    #[test]
    fn run_view_bad_source_is_render_error_before_window() {
        let command = Command::View(ViewArgs {
            file: None,
            code: Some("gitGraph\n  frobnicate".to_string()),
            stdin: false,
            title: DEFAULT_TITLE.to_string(),
            scale: 2.0,
        });
        assert!(matches!(run(command), Err(Error::Render(_))));
    }

    /// `render` is real in M-2.b: a valid `--code` source renders to SVG
    /// and is written to the `-o` target; nothing goes to stdout here
    /// (the write helper does, in-process this is just the file).
    #[test]
    fn run_render_writes_svg_to_file() {
        let path = std::env::temp_dir().join(format!(
            "mermaid-viewer-render-out-{}-{}.svg",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let args = RenderArgs {
            output: path.to_string_lossy().into_owned(),
            ..render_args(None, Some("graph TD\n  A-->B"))
        };
        run(Command::Render(args)).expect("render succeeds");
        let svg = std::fs::read_to_string(&path).expect("output file written");
        assert!(svg.contains("<svg"), "output must be an SVG document");
        let _ = std::fs::remove_file(&path);
    }

    /// A parser-rejected source surfaces the typed render error (exit 1
    /// in `main`), never a panic. Fixture verified to be rejected by
    /// mermaid-render 0.10 (same as `render_invalid_source_is_error`;
    /// unknown tokens like `!!!...` are not — the parser warns and
    /// falls back to flowchart).
    #[test]
    fn run_render_bad_source_is_render_error() {
        let args = render_args(None, Some("gitGraph\n  frobnicate"));
        assert!(matches!(run(Command::Render(args)), Err(Error::Render(_))));
    }

    const DEFAULT_TITLE: &str = "Mermaid Diagram";
}
