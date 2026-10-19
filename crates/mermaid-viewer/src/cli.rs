//! Command-line argument parsing for the `mermaid-viewer` binary.
//!
//! This module is the single source of truth for the CLI contract:
//!
//! ```text
//! mermaid-viewer view   [FILE]  [-c|--code <SRC>] [--stdin] [-w|--watch] [-t|--title <T>] [-s|--scale <F>]
//! mermaid-viewer render [FILE]  [-c|--code <SRC>] [--stdin] [-o|--output <P>] [-f|--format svg|png] [-s|--scale <F>]
//! mermaid-viewer mcp
//! ```
//!
//! The input sources (`FILE`, `--code`, `--stdin`) form a required,
//! mutually exclusive `source` group on each subcommand that takes a
//! diagram: at most one may be given, and at least one must be. A
//! violation is a clap usage error, which `main` maps to exit code 2.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Program name, shown in usage and help output.
pub const BIN_NAME: &str = "mermaid-viewer";

/// Default window title for `view`.
pub const DEFAULT_VIEW_TITLE: &str = "Mermaid Diagram";

/// Default PNG scale factor for `view` and `render`.
pub const DEFAULT_SCALE: f64 = 2.0;

/// Top-level argument structure for `mermaid-viewer`.
#[derive(Debug, Parser)]
#[command(
    name = BIN_NAME,
    version,
    about = "Render and view Mermaid diagrams, and serve them over MCP."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Subcommands of `mermaid-viewer`.
#[derive(Debug, Clone, PartialEq, Subcommand)]
pub enum Command {
    /// Render a diagram and show it in a GUI window.
    ///
    /// Blocks until the window is closed.
    ///
    /// Input containing `mermaid`-fenced code blocks (Markdown) is shown
    /// as one tab per block in the same window.
    #[command(group = clap::ArgGroup::new("source").multiple(false).required(true))]
    View(ViewArgs),

    /// Render a diagram to SVG or PNG, writing to a file or stdout.
    #[command(group = clap::ArgGroup::new("source").multiple(false).required(true))]
    Render(RenderArgs),

    /// Run a stdio MCP server exposing diagram tools.
    Mcp,
}

/// Output formats for `render`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Scalable vector graphics (default).
    Svg,
    /// Rasterized image, scaled by `--scale`.
    Png,
}

/// Arguments for `mermaid-viewer view`.
#[derive(Debug, Clone, PartialEq, Args)]
pub struct ViewArgs {
    /// Path to a Mermaid source file: a single diagram, or Markdown
    /// containing `mermaid`-fenced code blocks (shown as tabs).
    #[arg(value_name = "FILE", group = "source")]
    pub file: Option<PathBuf>,

    /// Mermaid source code, passed inline.
    #[arg(
        short = 'c',
        long,
        value_name = "SRC",
        group = "source",
        conflicts_with = "watch"
    )]
    pub code: Option<String>,

    /// Read the Mermaid source from stdin.
    #[arg(long, group = "source", conflicts_with = "watch")]
    pub stdin: bool,

    /// Re-render the window whenever the file changes on disk.
    /// Requires a `FILE` source.
    /// Not supported for Markdown input (input containing
    /// `mermaid`-fenced code blocks); a typed error is returned.
    #[arg(short = 'w', long, requires = "file")]
    pub watch: bool,

    /// Window title.
    #[arg(short = 't', long, value_name = "TITLE", default_value = DEFAULT_VIEW_TITLE)]
    pub title: String,

    /// PNG scale factor.
    #[arg(short = 's', long, value_name = "SCALE", default_value_t = DEFAULT_SCALE)]
    pub scale: f64,
}

/// Arguments for `mermaid-viewer render`.
#[derive(Debug, Clone, PartialEq, Args)]
pub struct RenderArgs {
    /// Path to a Mermaid source file.
    #[arg(value_name = "FILE", group = "source")]
    pub file: Option<PathBuf>,

    /// Mermaid source code, passed inline.
    #[arg(short = 'c', long, value_name = "SRC", group = "source")]
    pub code: Option<String>,

    /// Read the Mermaid source from stdin.
    #[arg(long, group = "source")]
    pub stdin: bool,

    /// Output file path, or `-` to write to stdout (default).
    #[arg(short = 'o', long, value_name = "OUTPUT", default_value = "-")]
    pub output: String,

    /// Output format: `svg` (default) or `png`.
    #[arg(short = 'f', long, value_enum, default_value_t = OutputFormat::Svg)]
    pub format: OutputFormat,

    /// PNG scale factor (ignored for SVG output).
    #[arg(short = 's', long, value_name = "SCALE", default_value_t = DEFAULT_SCALE)]
    pub scale: f64,
}

/// Parse command-line arguments (without the program name) into a
/// [`Command`].
///
/// The program name is synthesized internally, so `args` starts at the
/// subcommand, e.g. `["view", "d.mmd", "--scale", "3"]`.
pub fn parse<I>(args: I) -> Result<Command, clap::Error>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let args = std::iter::once(BIN_NAME.to_owned()).chain(args.into_iter().map(Into::into));
    Cli::try_parse_from(args).map(|cli| cli.command)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared verification plan: `cli_parses_view_args`.
    #[test]
    fn cli_parses_view_args() {
        let command = parse(["view", "d.mmd", "--scale", "3"]).unwrap();
        match command {
            Command::View(args) => {
                assert_eq!(args.file.as_deref(), Some(std::path::Path::new("d.mmd")));
                assert_eq!(args.scale, 3.0);
                assert_eq!(args.title, DEFAULT_VIEW_TITLE);
                assert_eq!(args.title, "Mermaid Diagram");
                assert!(args.code.is_none());
                assert!(!args.stdin);
            }
            other => panic!("expected View, got {other:#?}"),
        }
    }

    /// Shared verification plan: `view_parses_watch_flag`.
    ///
    /// `--watch` parses into `ViewArgs.watch == true` alongside a `FILE`
    /// source; a plain `FILE` parse leaves `watch == false` (the default).
    #[test]
    fn view_parses_watch_flag() {
        let command = parse(["view", "--watch", "d.mmd"]).unwrap();
        match command {
            Command::View(args) => {
                assert!(args.watch, "--watch must set `watch` to true");
                assert_eq!(
                    args.file.as_deref(),
                    Some(std::path::Path::new("d.mmd")),
                    "the FILE source must be recorded alongside --watch"
                );
            }
            other => panic!("expected View, got {other:#?}"),
        }

        let command = parse(["view", "d.mmd"]).unwrap();
        match command {
            Command::View(args) => {
                assert!(
                    !args.watch,
                    "watch must default to false when --watch is absent"
                );
                assert_eq!(
                    args.file.as_deref(),
                    Some(std::path::Path::new("d.mmd")),
                    "the FILE source must parse as before"
                );
            }
            other => panic!("expected View, got {other:#?}"),
        }
    }

    /// Shared verification plan: `watch_requires_file_source`.
    ///
    /// `--watch` requires a `FILE` source: pairing it with `--code` or
    /// `--stdin` (neither of which is a `FILE`) is a clap usage error,
    /// which `main` maps to exit code 2.
    #[test]
    fn watch_requires_file_source() {
        assert!(
            parse(["view", "--watch", "--code", "graph TD"]).is_err(),
            "--watch with a --code source must be a usage error"
        );
        assert!(
            parse(["view", "--watch", "--stdin"]).is_err(),
            "--watch with a --stdin source must be a usage error"
        );
    }

    /// Shared verification plan: `view_code_and_stdin_parse_without_watch`.
    ///
    /// `--code` and `--stdin` parse on their own (without `--watch`),
    /// recording the chosen source with `watch == false`: the
    /// `conflicts_with = "watch"` declarations must not regress solo use
    /// of the two fields whose attributes changed.
    #[test]
    fn view_code_and_stdin_parse_without_watch() {
        let command = parse(["view", "--code", "graph TD"]).unwrap();
        match command {
            Command::View(args) => {
                assert!(!args.watch, "watch must default to false without --watch");
                assert_eq!(
                    args.code.as_deref(),
                    Some("graph TD"),
                    "the --code source must be recorded"
                );
                assert!(!args.stdin);
                assert!(args.file.is_none());
            }
            other => panic!("expected View, got {other:#?}"),
        }

        let command = parse(["view", "--stdin"]).unwrap();
        match command {
            Command::View(args) => {
                assert!(!args.watch, "watch must default to false without --watch");
                assert!(args.stdin, "the --stdin source must be recorded");
                assert!(args.code.is_none());
                assert!(args.file.is_none());
            }
            other => panic!("expected View, got {other:#?}"),
        }
    }

    /// Shared verification plan: `cli_parses_render_defaults`.
    ///
    /// The contract requires exactly one input source (a missing source is
    /// a usage error, see `cli_rejects_no_source`), so the defaults are
    /// asserted on a parse with a valid `--stdin` source.
    #[test]
    fn cli_parses_render_defaults() {
        let command = parse(["render", "--stdin"]).unwrap();
        match command {
            Command::Render(args) => {
                assert_eq!(args.output, "-");
                assert_eq!(args.format, OutputFormat::Svg);
                assert_eq!(args.scale, DEFAULT_SCALE);
                assert_eq!(args.scale, 2.0);
                assert!(args.file.is_none());
                assert!(args.code.is_none());
                assert!(args.stdin);
            }
            other => panic!("expected Render, got {other:#?}"),
        }
    }

    /// Shared verification plan: `cli_rejects_multiple_sources`.
    #[test]
    fn cli_rejects_multiple_sources() {
        let err = parse(["render", "f.mmd", "--code", "x"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    /// Shared verification plan: `cli_rejects_no_source`.
    #[test]
    fn cli_rejects_no_source() {
        let err = parse(["view"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    /// `mcp` takes no arguments and parses on its own.
    #[test]
    fn mcp_parses_bare() {
        assert_eq!(parse(["mcp"]).unwrap(), Command::Mcp);
    }

    /// `--code` and `--stdin` are also mutually exclusive on `view`.
    #[test]
    fn view_rejects_code_and_stdin() {
        let err = parse(["view", "--code", "graph TD", "--stdin"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    /// `FILE` and `--stdin` conflict on `render` too.
    #[test]
    fn render_rejects_file_and_stdin() {
        let err = parse(["render", "f.mmd", "--stdin"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    /// `--format png` and `--title` parse into their typed fields.
    #[test]
    fn render_parses_png_format_and_view_title() {
        let render = parse(["render", "--code", "graph TD", "-f", "png"]).unwrap();
        match render {
            Command::Render(args) => {
                assert_eq!(args.format, OutputFormat::Png);
                assert_eq!(args.code.as_deref(), Some("graph TD"));
            }
            other => panic!("expected Render, got {other:#?}"),
        }

        let view = parse(["view", "--code", "graph TD", "-t", "My Diagram"]).unwrap();
        match view {
            Command::View(args) => assert_eq!(args.title, "My Diagram"),
            other => panic!("expected View, got {other:#?}"),
        }
    }
}
