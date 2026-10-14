# mermaid-viewer

A Rust workspace for viewing and rendering Mermaid diagrams natively — no
JavaScript, no browser, no Node runtime. `mermaid-viewer-core` provides the
rendering pipeline (Mermaid source → SVG → PNG), a raylib-based diagram window
with blocking and controller modes, and a stdio MCP server for coding agents.
`mermaid-viewer` is a single CLI with three subcommands:

- `view` — renders a diagram to PNG and displays it in a window until the
  window is closed.
- `render` — writes SVG or PNG to a file or stdout.
- `mcp` — serves the diagram tools over stdio for MCP-capable clients.

## Building

Prerequisites:

- A Rust toolchain (MSRV 1.88, edition 2021) — e.g. via `rustup`.
- CMake, and a C/C++ compiler (raylib's native build step).
- Platform graphics development packages:
  - **Linux (X11/OpenGL):**
    `sudo apt-get install -y --no-install-recommends cmake clang \
     libasound2-dev libgl1-mesa-dev libx11-dev libxext-dev libxi-dev \
     libxrandr-dev libxinerama-dev libxcursor-dev libxxf86vm-dev`
    (`clang` supplies libclang, which the raylib bindings need at build time.)
  - **Windows:** CMake plus the MSVC C++ toolchain (Visual Studio Build
    Tools, C++ workload), plus LLVM for libclang — `choco install clang`.

```sh
cargo build --release
# binary: target/release/mermaid-viewer
```

The `view` and `mcp` subcommands open a window on a machine with a display.
For headless runs (testing, CI), `MERMAID_VIEWER_FAKE_WINDOW=1` selects a
fake window backend with the same state machine and no display, so the full
`view`/`mcp` paths execute without a GUI.

## Usage

### `view` — show a diagram in a window

```text
mermaid-viewer view [FILE] [-c|--code <SRC>] [--stdin] [-t|--title <T>] [-s|--scale <F>]

mermaid-viewer view diagram.mmd
mermaid-viewer view --code "graph TD
  A-->B"
echo "graph TD
  A-->B" | mermaid-viewer view --stdin
```

Blocks until the window is closed (ESC or the window close button). The
diagram is fit to the window on open; resize re-fits, the mouse wheel zooms,
and left-drag pans. Defaults: title `Mermaid Diagram`, PNG scale `2.0`.
`(These examples open a window, so they are shown as text, not runnable
blocks.)`

### `render` — write SVG or PNG to a file or stdout

```text
mermaid-viewer render [FILE] [-c|--code <SRC>] [--stdin] [-o|--output <P>] [-f|--format svg|png] [-s|--scale <F>]
```

```sh
# SVG (the default format) to stdout
mermaid-viewer render --code "graph TD
  A-->B"

# PNG (default 2x scale) to a file
mermaid-viewer render --code "graph TD
  A-->B" -o diagram.png -f png

# from stdin
echo "graph TD
  A-->B" | mermaid-viewer render --stdin -o diagram.svg
```

`-o -` (the default) writes the raw image bytes to stdout; any other `-o`
path writes to that file (parent directories are not created). `--scale`
only affects PNG output; SVG ignores it.

### `mcp` — stdio MCP server

```sh
mermaid-viewer mcp
```

Takes no arguments. Speaks the Model Context Protocol on stdin/stdout and
exits `0` cleanly on stdin EOF or Ctrl-C (closing its diagram window first).
stdout carries only MCP protocol frames; diagnostics go to stderr.

### Exit codes

- `0` — success (also for `--help`/`--version`)
- `1` — runtime/render/IO failure (message on stderr)
- `2` — usage error (clap prints the usage message)

Exactly one input source (`FILE`, `-c`/`--code`, `--stdin`) is required per
`view`/`render` invocation; giving none or several is a usage error (exit 2).

## MCP for coding agents

Prefer the MCP server for agents: a shell `view` call blocks the agent until
a human closes the window, which is the wrong shape for an agent loop. The
MCP server keeps a persistent window alive across the session —
`show_diagram` returns instantly and the agent can iteratively hot-swap the
window content. The CLI stays for humans and one-shot pipelines.

Register the server in your MCP client configuration:

```json
{"mcpServers": {"mermaid-viewer": {"command": "mermaid-viewer", "args": ["mcp"]}}}
```

Tools:

- `show_diagram({code, title?, scale?})` — renders `code` (Mermaid source) to
  PNG and shows it in the persistent window. The first call opens the window
  and returns `{"status":"opened"}`; later calls hot-swap the content and
  return `{"status":"updated"}`. `title` is used when the window is created;
  `scale` is the PNG scale (default `2.0`). A rejected source is reported as
  a tool-level error (`isError: true`) carrying the render message — the
  server keeps running and serves the next request.
- `close_diagram({})` — closes the window and returns `{"closed":true}`.
  Idempotent: closing an already-closed (or never-opened) window also
  succeeds.

## Renderer coverage

Diagrams are rendered natively by [mermaid-render](https://crates.io/crates/mermaid-render)
(pure Rust, no JS engine). The supported diagram types are the 10
`mermaid-render` covers: **flowchart, sequence, class, ER, state, gantt,
pie, timeline, mindmap, and gitgraph**. Anything the parser does not accept —
including unsupported diagram types and empty input — returns a typed error
(`mermaid render failed: …`, exit code `1` from the CLI, an `isError` tool
result from MCP); the renderer never panics on bad input.

## Workspace layout

- `crates/mermaid-viewer-core` — the library: `render` (the
  `NativeRenderer` SVG/PNG pipeline), `view` (`run_blocking` +
  `WindowController`), and `mcp` (the `DiagramServer` handler). Features:
  `view` (raylib OpenGL-3.3 backend, default), `view-headless` (raylib
  software-renderer backend used by the headless CI test pass; mutually
  exclusive with `view`), and `mcp` (default).
- `crates/mermaid-viewer` — the `mermaid-viewer` binary described above.
