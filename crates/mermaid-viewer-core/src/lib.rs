//! # mermaid-viewer-core
//!
//! Core library behind the `mermaid-viewer` CLI. It provides three
//! capabilities, each landing in its own module in a later milestone:
//!
//! - **`render`** — a synchronous, pure Mermaid → SVG/PNG rendering pipeline.
//! - **`view`** — a long-lived raylib diagram window with blocking and
//!   controller modes, for humans and for coding agents.
//! - **`mcp`** — a stdio MCP server (`DiagramServer`) exposing the
//!   `show_diagram` / `close_diagram` tools.
//!
//! All fallible operations return [`error::Result<T>`] with a typed
//! [`error::Error`]; the crate never panics on bad diagram input.
//!
//! ## Features
//!
//! - `view` *(default)* — raylib OpenGL-3.3 GUI backend (`raylib-ogl`).
//! - `view-headless` — raylib software-renderer GUI backend (`raylib-sw`),
//!   used for the headless CI test pass. Mutually exclusive with `view`.
//! - `mcp` *(default)* — rmcp-based MCP server support.
//!
//! ## Lint policy
//!
//! The library is `#![deny(missing_docs, unsafe_code)]`: every public item
//! must be documented, and the only `unsafe` in the crate is the documented
//! raylib FFI boundary in `view::image_from_rgba` (allowed explicitly there).

#![deny(missing_docs, unsafe_code)]

/// Enforce single-GUI-toolchain-per-build: the two raylib backend features
/// are mutually exclusive (raylib initialization is single-per-process and
/// the two backends compile conflicting platform code).
#[cfg(all(feature = "view", feature = "view-headless"))]
compile_error!(
    "features `view` and `view-headless` are mutually exclusive; \
     enable at most one raylib backend"
);

pub mod error;
pub mod render;

/// The raylib diagram window (blocking + controller modes). Compiled under
/// exactly one of the `view` / `view-headless` features.
#[cfg(any(feature = "view", feature = "view-headless"))]
pub mod view;

/// The rmcp MCP server handler ([`DiagramServer`], exposing the
/// `show_diagram` / `close_diagram` tools). Requires the `mcp` feature and
/// a raylib backend: the server drives the diagram window through
/// `view::WindowController`, so a windowless build has no MCP server.
#[cfg(all(feature = "mcp", any(feature = "view", feature = "view-headless")))]
pub mod mcp;

#[cfg(all(feature = "mcp", any(feature = "view", feature = "view-headless")))]
pub use mcp::DiagramServer;
pub use render::{ImageFormat, NativeRenderer, RenderOptions, RenderedImage};
#[cfg(any(feature = "view", feature = "view-headless"))]
pub use view::{run_blocking, run_blocking_updates, WindowController};
