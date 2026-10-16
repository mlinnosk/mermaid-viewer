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
- CMake, a C compiler, **and a C++ compiler** (raylib's native build step;
  CMake requires a CXX compiler even though raylib itself is C).
- libclang (the `raylib-sys` build script generates Rust bindings with
  bindgen, which loads libclang).
- X11/OpenGL development packages (Linux; per-distro commands below).

**Fedora / RHEL (dnf):**

```sh
sudo dnf install -y cmake gcc gcc-c++ clang-libs \
    libX11-devel libxcb-devel libXi-devel \
    libXcursor-devel libXrandr-devel libXinerama-devel libXxf86vm-devel \
    libglvnd-devel openal-soft-devel
```

> **Fedora bindgen quirk:** Fedora's `clang-libs` puts `libclang.so` in
> `/lib64` but its built-in C headers in `/usr/lib/clang/<ver>/include`, so
> libclang cannot locate `stdarg.h` and the build fails with
> `fatal error: 'stdarg.h' file not found` during binding generation. Fix by
> pointing bindgen at the resource dir (version from `ls /usr/lib/clang`):
>
> ```sh
> export BINDGEN_EXTRA_CLANG_ARGS="-resource-dir=/usr/lib/clang/22"
> ```
>
> One-time system alternative: `sudo ln -s /usr/lib/clang /usr/lib64/clang`.

**Debian / Ubuntu (apt):**

```sh
sudo apt-get install -y --no-install-recommends cmake clang \
    libgl1-mesa-dev libx11-dev libxext-dev libxcb1-dev libxi-dev \
    libxrandr-dev libxinerama-dev libxcursor-dev libxxf86vm-dev \
    libopenal-dev
```

(`clang` supplies libclang, which the raylib bindings need at build time.)

**Windows:** CMake plus the MSVC C++ toolchain (Visual Studio Build
Tools, C++ workload), plus LLVM for libclang — `choco install clang`.

```sh
cargo build --release
# binary: target/release/mermaid-viewer
```

## Runtime dependencies

The binary statically links raylib and GLFW; it needs on the target system:

- `glibc` and `libgcc` — present on any system.
- `libX11` (which pulls in `libxcb` and `libXau`) — loaded at process start
  for **all** subcommands, even headless `render`.

For `view` and `mcp` (anything that opens a window) additionally:

- A running X11 session and an OpenGL/GLX implementation. Desktop systems
  have this by default; on a bare Linux system install Mesa DRI drivers
  (Fedora: `sudo dnf install -y mesa-dri-drivers`) and an X server.

The X11 extension libraries (`libXrandr`, `libXi`, `libXcursor`,
`libXinerama`, `libXxf86vm`) and the GL stack (`libGL`/`libGLX`) are loaded
dynamically at runtime, not linked directly — they come with any standard
desktop install. No audio package is needed at runtime: the raylib audio
module is unused by this app and dropped at link time.

Minimal runtime install for a bare machine (Fedora):

```sh
sudo dnf install -y libX11 mesa-dri-drivers
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
stdout carries only MCP protocol frames; diagnostics go to stderr. The
diagram window the server opens is tabbed — several diagrams can stay open
at once, each under its own tab ID (see `MCP for coding agents` below).

### Exit codes

- `0` — success (also for `--help`/`--version`)
- `1` — runtime/render/IO failure (message on stderr)
- `2` — usage error (clap prints the usage message)

Exactly one input source (`FILE`, `-c`/`--code`, `--stdin`) is required per
`view`/`render` invocation; giving none or several is a usage error (exit 2).

## MCP for coding agents

Prefer the MCP server for agents: a shell `view` call blocks the agent until
a human closes the window, which is the wrong shape for an agent loop. The
MCP server keeps a persistent, tabbed window alive across the session —
`show_diagram` returns instantly, so the agent can iteratively hot-swap a
tab's content or keep several diagrams open at once under stable string
IDs. The window shows a tab bar at the top — the user can select a tab or
close one directly (closing the last tab closes the window) — with the
active tab's diagram below it. The CLI stays for humans and one-shot
pipelines.

Register the server in your MCP client configuration:

```json
{"mcpServers": {"mermaid-viewer": {"command": "mermaid-viewer", "args": ["mcp"]}}}
```

Tools:

- `show_diagram({code, id?, title?, scale?})` — renders `code` (Mermaid
  source) to SVG and shows it in the persistent window under a tab; the
  window rasterizes the SVG to a texture on demand (below) —
  `show_diagram` does no rasterization itself. An omitted `id` uses the
  `"default"` tab (the old single-diagram workflow); showing a fresh `id`
  adds a tab and returns `{"id":"…","status":"added"}`; re-showing a known
  `id` replaces that tab and returns `{"id":"…","status":"updated"}`; and
  a show that opens the (closed) window returns
  `{"id":"…","status":"opened"}`. `title` is the tab's label in the tab
  bar (default: the effective `id`; the OS window title is fixed);
  `scale` is the initial raster scale hint (default `2.0`): a floor on
  the tab's first-raster sharpness — the first raster runs at
  `max(hint, fit × 1.25)`, clamped to the pixel cap — and the window
  re-rasters at display-appropriate scales as the user zooms. A
  non-positive or non-finite hint is ignored, never an error. At most 16
  tabs may be open at once: adding a 17th tab, like a rejected source or
  an empty `id`, is reported as a tool-level error (`isError: true`)
  carrying the message — the server keeps running and serves the next
  request.
- `close_diagram({id?})` — closes the tab with that `id` and returns
  `{"id":"…","closed":true}` (closing the last tab closes the window);
  omitting `id` closes the window and all tabs and returns
  `{"closed":true}`. Both forms are idempotent: closing an unknown `id` or
  an already-closed window also succeeds. An empty `id` is a tool-level
  error (`isError: true`) and the server keeps running.

**On-demand rendering.** No rasterization happens on the `show_diagram`
caller path: the show renders Mermaid source to SVG only and stages the
tab, and until the tab's first raster lands the window shows a minimal
placeholder. A dedicated raster worker thread (pure Rust resvg with the
embedded Carlito font) rasterizes the SVG off the GUI thread, and the
window uploads the result as a GPU texture when the tab is active. The
first raster runs at `max(scale hint, fit × 1.25)` — the hint is a floor
on first-raster sharpness, never a failure parameter — and when a tab is
activated (or the user zooms) the worker re-rasters at the display scale:
the window re-rasters once the zoom has been stable for 200 ms and
deviates more than 1.5× from the live texture's scale, keeping the old
texture until the new raster lands. At most one tab's texture exists at
any instant: switching tabs frees the outgoing tab's texture (the new tab
rasterizes on demand), and every raster is clamped to 16,777,216 pixels
(4096 × 4096 ≈ 64 MiB of RGBA8), which bounds texture memory.

Markdown support (parsing Mermaid blocks out of Markdown) and multiple
simultaneous OS windows are unsupported: the server is one window with a
tab bar. The on-demand rendering above changes only the documented
`scale` meaning — the MCP wire contract (the two tools, their schemas,
statuses, and result bodies) and the CLI (`view`/`render`, one eager PNG
per diagram) are unchanged.

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
