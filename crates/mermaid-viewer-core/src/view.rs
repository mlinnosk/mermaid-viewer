//! Long-lived raylib diagram window: blocking and controller modes.
//!
//! This module is the `view` contract of `mermaid-viewer-core`:
//!
//! - [`run_blocking`] — a one-diagram window driven on the *current* thread,
//!   for the CLI `view` subcommand. Returns when the window is closed.
//! - [`WindowController`] — a dedicated, spawned GUI thread that owns the
//!   tabbed raylib window and is driven over a plain-data channel; for the
//!   MCP server, whose main thread runs a tokio runtime.
//!
//! ## Thread model
//!
//! Raylib initialization is single per process and its `RaylibThread` token
//! is `!Send`, so every real window lives on exactly one dedicated OS thread.
//! [`WindowController`] (which is `Send + Sync`) talks to that thread only
//! over a `std::sync::mpsc` channel carrying plain data (strings, PNG
//! images, commands, acknowledgements), with the open/closed state kept in
//! an `AtomicBool`.
//!
//! The `show_tab` acknowledgement wait is bounded
//! ([`WindowController::GUI_ACK_TIMEOUT`]): a GUI thread that wedges inside
//! the display system is reported as a typed [`crate::error::Error::View`]
//! (a dead one is re-spawned) instead of hanging every later call.
//!
//! ## State machine
//!
//! The real and the fake backend are observable-identical: the window is
//! open exactly while it holds at least one tab,
//!
//! ```text
//! CLOSED --show_tab(new id)--> OPEN       (adds + activates the tab)
//! OPEN   --show_tab(new id)--> OPEN       (adds + activates a tab)
//! OPEN   --show_tab(known id)--> OPEN     (replaces + activates the tab)
//! OPEN   --close_tab(id)  --> OPEN|CLOSED (removes a tab; the last closes)
//! any    --close        --> CLOSED        (idempotent; ESC/OS close too)
//! ```
//!
//! `MERMAID_VIEWER_FAKE_WINDOW=1` selects the fake backend at
//! [`WindowController::spawn`]: no display, no raylib init, state only — so
//! the full controller path is testable in CI without a display.
//!
//! ## Window features
//!
//! Fit-to-window initial scale (recomputed every frame, so resizing re-fits),
//! mouse-wheel zoom, left-button drag pan (both on the active tab), tabs
//! added/replaced/closed by ID (up to [`WindowController::MAX_TABS`]), and a
//! raygui tab bar across the top of the *controller* window: the user selects
//! the active tab and closes tabs from the bar (a bar close is reported to the
//! controller through `drain_tab_closes` for MCP registry reconciliation; the
//! last tab closed through the bar closes the window), close on ESC or window
//! close, and re-openable after close (raylib re-initializes on the same
//! thread). The single-diagram CLI [`run_blocking`] window keeps its full-window
//! frame with no tab strip.
//!
//! Window input is raster **PNG** only: an [`ImageFormat::Svg`]
//! [`RenderedImage`] is rejected with [`Error::View`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{Builder, JoinHandle};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::render::{ImageFormat, RenderedImage};

/// The active raylib backend: the OpenGL-3.3 build under the `view` feature,
/// the software-renderer build under `view-headless`. Exactly one of the two
/// features is ever enabled (a `compile_error!` in `lib.rs` forbids both), so
/// aliasing the selected crate as a single `raylib` keeps one code path for
/// both builds.
#[cfg(feature = "view")]
use raylib_ogl as raylib;
#[cfg(feature = "view-headless")]
use raylib_sw as raylib;

/// The raylib items used by both backends (identical safe API).
///
/// `RaylibGuiContainers` provides `gui_tab_bar` (the raygui tab-bar UI); the
/// `raygui` feature on both backend dependencies makes the FFI it binds to
/// available, so the import is valid on both the `view` and `view-headless`
/// builds.
use raylib::prelude::{
    set_trace_log_callback, Color, Image, KeyboardKey, MouseButton, PixelFormat, RaylibDraw,
    RaylibGuiContainers, RaylibHandle, RaylibThread, Rectangle, Texture2D, Vector2,
};

/// Default initial window size.
const WINDOW_WIDTH: i32 = 800;
const WINDOW_HEIGHT: i32 = 600;

/// Wheel-zoom factor per wheel notch.
const ZOOM_STEP: f64 = 1.2;
const MIN_ZOOM: f64 = 0.05;
const MAX_ZOOM: f64 = 128.0;

/// The fixed height (window pixels) of the tab strip drawn across the top of
/// the controller window. The strip is always present while the window is
/// open (a single tab included), so the content area's geometry — the window
/// minus this strip — is uniform and the fit layout is stable across tab
/// count changes.
const TAB_STRIP_HEIGHT: i32 = 36;

/// Window background (the clear color drawn outside the image rect).
pub const BACKGROUND: Color = Color::RAYWHITE;

/// Environment variable that selects the headless fake window backend.
const FAKE_WINDOW_ENV: &str = "MERMAID_VIEWER_FAKE_WINDOW";

/// True when the fake backend is requested via the environment.
fn fake_window_enabled() -> bool {
    std::env::var(FAKE_WINDOW_ENV).is_ok_and(|v| v == "1")
}

/// Rejects non-PNG window input.
///
/// The window displays raster images; SVG must be rendered to PNG first
/// (see [`crate::render::NativeRenderer`]).
fn ensure_png(image: &RenderedImage) -> Result<()> {
    if image.format != ImageFormat::Png {
        return Err(Error::View(
            "window input must be a PNG image; render the diagram to PNG first (SVG is rejected)"
                .to_string(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Layout math (pure)
// ---------------------------------------------------------------------------

/// The on-screen geometry of the diagram for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ViewLayout {
    /// Scale factor applied to the texture when drawing (1.0 = native size).
    scale: f64,
    /// Displayed image size in window pixels.
    width: f64,
    height: f64,
    /// Top-left corner of the displayed image in window pixels.
    x: f64,
    y: f64,
}

/// Computes the fit/zoom/pan layout.
///
/// `fit = min(win_w / img_w, win_h / img_h)` and `scale = fit * zoom` — the
/// image is scaled to fit the window (aspect preserved, no overflow at
/// `zoom == 1.0`). While the drawn image fits the window it is centered and
/// shifted by the pan offset; once it overflows the window (zoom in past
/// the fit), the pan offset is the rect's absolute top-left position. A
/// non-finite or non-positive `zoom` is clamped to `1.0` (fit), never a
/// degenerate transform.
fn compute_layout(
    win_w: f64,
    win_h: f64,
    img_w: f64,
    img_h: f64,
    zoom: f64,
    pan_x: f64,
    pan_y: f64,
) -> ViewLayout {
    let img_w = img_w.max(1.0);
    let img_h = img_h.max(1.0);
    let zoom = if zoom.is_finite() && zoom > 0.0 {
        zoom
    } else {
        1.0
    };

    let fit = (win_w / img_w).min(win_h / img_h);
    let scale = fit * zoom;
    let width = img_w * scale;
    let height = img_h * scale;
    let x = if width <= win_w {
        (win_w - width) / 2.0 + pan_x
    } else {
        pan_x
    };
    let y = if height <= win_h {
        (win_h - height) / 2.0 + pan_y
    } else {
        pan_y
    };
    ViewLayout {
        scale,
        width,
        height,
        x,
        y,
    }
}

// ---------------------------------------------------------------------------
// Frame drawing + texture upload (shared by all window code paths)
// ---------------------------------------------------------------------------

/// Fits and draws `texture` into the content rect
/// `[(origin_x, origin_y) .. (origin_x + content_w, origin_y + content_h)]`
/// using the tab's `zoom`/`pan`. The fit is computed against the content rect
/// (so the image fills the area below the tab strip, not the whole window)
/// and the resulting rect is translated by `(origin_x, origin_y)`.
#[allow(clippy::too_many_arguments)]
fn draw_content(
    d: &mut impl RaylibDraw,
    texture: &Texture2D,
    origin_x: f64,
    origin_y: f64,
    content_w: f64,
    content_h: f64,
    image: &RenderedImage,
    zoom: f64,
    pan_x: f64,
    pan_y: f64,
) {
    let layout = compute_layout(
        content_w,
        content_h,
        image.width as f64,
        image.height as f64,
        zoom,
        pan_x,
        pan_y,
    );
    d.draw_texture_ex(
        texture,
        Vector2 {
            x: (layout.x + origin_x) as f32,
            y: (layout.y + origin_y) as f32,
        },
        0.0,
        layout.scale as f32,
        Color::WHITE,
    );
}

/// Draws one frame of the diagram window into the active raylib framebuffer:
/// clears the background and draws `texture` with the fit/zoom/pan layout.
///
/// This is the single-draw routine of the *single-diagram* (CLI) window — the
/// same code runs in [`run_blocking`] and in the headless pixel tests (with
/// `view-headless`). It always draws the image into the *full* window (no tab
/// strip): the tabbed controller frame goes through [`draw_tabbed_frame`]
/// instead, so the CLI `view` keeps its exact full-window frame.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub fn draw_frame(
    d: &mut impl RaylibDraw,
    texture: &Texture2D,
    win_w: i32,
    win_h: i32,
    image: &RenderedImage,
    zoom: f64,
    pan_x: f64,
    pan_y: f64,
) {
    d.clear_background(BACKGROUND);
    draw_content(
        d,
        texture,
        0.0,
        0.0,
        win_w as f64,
        win_h as f64,
        image,
        zoom,
        pan_x,
        pan_y,
    );
}

/// Draws one frame of the *tabbed* controller window: the raygui tab bar
/// across the top strip, then the active tab fitted into the content area
/// below it.
///
/// The tab bar is driven through [`RaylibGuiContainers::gui_tab_bar`], which
/// updates `active` in place (a user selection switches the drawn tab) and
/// returns the index of a tab the user requested to close (or `-1`). A close
/// removes that tab through the same [`TabbedState::close`] rules as
/// `close_tab`, pushes its ID onto `ui_closes` (drained by
/// [`WindowController::drain_tab_closes`] so the MCP registry can reconcile),
/// and — when it removed the last tab — closes the window, which the function
/// reports by returning `false`.
///
/// `#[doc(hidden)]` seam: the headless pixel tests (under `view-headless`)
/// drive the exact production tabbed frame through it, mirroring
/// [`draw_frame`] / [`load_window_texture`].
///
/// Returns `false` only when the last tab was closed via the bar (the window
/// should close); `true` otherwise (a tab remains).
#[doc(hidden)]
pub fn draw_tabbed_frame(
    d: &mut impl RaylibDraw,
    window: &mut Window,
    sw: i32,
    sh: i32,
    ui_closes: &mut Vec<String>,
) -> bool {
    d.clear_background(BACKGROUND);
    if window.state.is_empty() {
        return false;
    }

    let strip_h = TAB_STRIP_HEIGHT as f64;
    let content_w = sw as f64;
    let content_h = (sh as f64 - strip_h).max(0.0);

    // Draw the tab bar. `gui_tab_bar` clamps `active` to a valid index,
    // updates it on a selection, and returns the close-requested tab index.
    // `labels` borrows `window.state` for this call only; scoping it ends the
    // borrow before the (mutable) state updates below.
    let (close_idx, selected) = {
        let labels: Vec<&str> = window.state.tabs.iter().map(|t| t.label.as_str()).collect();
        let bar = Rectangle::new(0.0, 0.0, content_w as f32, strip_h as f32);
        let mut active = window.state.active_index() as i32;
        let close = d.gui_tab_bar(bar, &labels, &mut active);
        (close, active)
    };

    // A selection (no close) re-targets the active tab; wheel zoom and drag
    // pan then apply to it on the next frame, and each tab's zoom/pan persists
    // independently in its own `TabState`.
    if close_idx < 0 && selected as usize != window.state.active_index() {
        window.state.active = (selected as usize).min(window.state.len().saturating_sub(1));
    }

    // A close request removes the tab through the shared close rules (the same
    // active-index adjustment `close_tab` / `tab_close_index_updates_active`
    // exercise) and reports the ID for the MCP registry drain.
    if close_idx >= 0 {
        let pos = close_idx as usize;
        if pos < window.state.len() {
            let id = window.state.tabs[pos].id.clone();
            window.close_tab(&id);
            ui_closes.push(id);
            if window.state.is_empty() {
                // The bar-closed final tab: report it, then close the window.
                return false;
            }
        }
    }

    // Draw the active tab in the content area below the strip.
    let active = window.state.active_index();
    let tab = &window.state.tabs[active];
    draw_content(
        d,
        &window.textures[active],
        0.0,
        strip_h,
        content_w,
        content_h,
        &tab.image,
        tab.zoom,
        tab.pan_x,
        tab.pan_y,
    );
    true
}

/// Decodes `image` (which must be PNG) into a raylib texture for the window
/// owned by `(rl, thread)`.
///
/// The PNG is decoded in Rust (via the `png` crate) and uploaded as raw
/// RGBA8: the headless (`view-headless`) build compiles raylib with the PNG
/// codec disabled, so the texture path must not depend on raylib's file
/// loaders on either backend.
#[doc(hidden)]
pub fn load_window_texture(
    rl: &mut RaylibHandle,
    thread: &RaylibThread,
    image: &RenderedImage,
) -> Result<Texture2D> {
    ensure_png(image)?;
    let (rgba, w, h) = decode_png_rgba(&image.bytes)?;
    let raw_image = image_from_rgba(rgba, w, h);
    rl.load_texture_from_image(thread, &raw_image)
        .map_err(|e| Error::View(format!("failed to upload window texture: {e}")))
}

/// Decodes PNG bytes into straight (unassociated) RGBA8 rows.
fn decode_png_rgba(bytes: &[u8]) -> Result<(Vec<u8>, i32, i32)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // Normalise any source encoding (palette, grey, 16-bit) to 8-bit RGBA so
    // the upload path is uniform across backends.
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| Error::View(format!("invalid PNG data for window display: {e}")))?;
    let (w, h) = (reader.info().width as i32, reader.info().height as i32);
    let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
    let mut pos = 0;
    while let Some(row) = reader
        .next_row()
        .map_err(|e| Error::View(format!("PNG decode failed for window display: {e}")))?
    {
        let data = row.data();
        buf[pos..pos + data.len()].copy_from_slice(data);
        pos += data.len();
    }
    if pos != buf.len() {
        return Err(Error::View(format!(
            "PNG decode produced {} of {} bytes",
            pos,
            buf.len()
        )));
    }
    Ok((buf, w, h))
}

/// Wraps a straight RGBA8 buffer in a raylib [`raylib::Image`].
///
/// The buffer is allocated with the C allocator and ownership is transferred
/// to raylib: `raylib::Image`'s `Drop` calls `UnloadImage`, which releases
/// it with `RAYLIB_FREE` (the C `free`).
///
/// This is the crate's single `unsafe` block — the FFI boundary with
/// raylib's C `Image`. The crate-level `deny(unsafe_code)` permits it here
/// via the explicit `#[allow(unsafe_code)]`.
#[allow(unsafe_code)]
fn image_from_rgba(rgba: Vec<u8>, width: i32, height: i32) -> Image {
    use std::alloc::{alloc, Layout};
    let layout = Layout::array::<u8>(rgba.len()).expect("decoded PNG size fits in isize");
    // SAFETY: `layout` is non-zero-sized (a decoded PNG always has at least
    // one pixel), and `alloc` returns a properly aligned allocation of
    // `rgba.len()` bytes that we fully initialize immediately below.
    let ptr: *mut u8 = unsafe { alloc(layout) };
    if ptr.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    // SAFETY: `ptr` is valid for `rgba.len()` bytes and the copy initializes
    // the entire allocation.
    unsafe { ptr.copy_from_nonoverlapping(rgba.as_ptr(), rgba.len()) };
    let raw = raylib::ffi::Image {
        data: ptr as *mut std::ffi::c_void,
        width,
        height,
        mipmaps: 1,
        format: PixelFormat::PIXELFORMAT_UNCOMPRESSED_R8G8B8A8 as i32,
    };
    // SAFETY: `raylib::Image` is a single-field tuple wrapper around
    // `raylib::ffi::Image` with identical layout, so the transmutation is
    // valid; the result follows the same ownership rules (its `Drop` calls
    // `UnloadImage`, which frees `data`).
    unsafe { std::mem::transmute::<raylib::ffi::Image, Image>(raw) }
}

// ---------------------------------------------------------------------------
// Window (raylib side of the state machine)
// ---------------------------------------------------------------------------

/// The raylib trace-log threshold for windowed builds.
///
/// raylib's C-side default is `LOG_INFO`, and its init chatter (GL version,
/// font load, …) leaks straight into the CLI's stderr. Release builds keep
/// only `LOG_WARNING` and above; debug builds keep the crate default
/// (`LOG_INFO`) so the full init trace stays visible when developing
/// against a real window.
///
/// Release is detected as `not(debug_assertions)` (cargo's release profile
/// turns debug assertions off): cargo sets no `cfg(release)` of its own.
fn raylib_log_level() -> raylib::consts::TraceLogLevel {
    if cfg!(not(debug_assertions)) {
        raylib::consts::TraceLogLevel::LOG_WARNING
    } else {
        raylib::consts::TraceLogLevel::LOG_INFO
    }
}

/// The trace-log sink installed before every raylib window init.
///
/// raylib's C-side `TraceLog` writes to the process **stdout** by default —
/// and for the MCP server, stdout *is* the protocol channel (JSON-RPC over
/// stdin/stdout). Any raylib chatter (init banner, warnings, errors) would
/// therefore corrupt the protocol stream and hang strict MCP clients. This
/// sink routes every trace line to **stderr**, where it is harmless; the
/// level filter itself is unchanged ([`raylib_log_level`]).
fn trace_log_to_stderr(level: raylib::consts::TraceLogLevel, text: &str) {
    eprintln!("[raylib log {0}] {1}", level as i32, text);
}

// ---------------------------------------------------------------------------
// Tabbed state (pure: no raylib — testable under every feature pass)
// ---------------------------------------------------------------------------

/// The typed capacity error for a window that already holds
/// [`WindowController::MAX_TABS`] tabs.
fn tab_capacity_error() -> Error {
    Error::View(format!(
        "window is at the {}-tab limit and cannot add a new tab",
        WindowController::MAX_TABS
    ))
}

/// A diagram tab's state: its stable ID, display label, current image, and
/// the tab's independent zoom/pan. Pure (no raylib types).
#[derive(Debug, Clone)]
struct TabState {
    /// Stable identifier the tab is addressed by.
    id: String,
    /// The tab's display label.
    label: String,
    /// Current content (kept for the drawn dimensions).
    image: RenderedImage,
    /// Zoom multiplier on top of the fit scale (`1.0` = exact fit).
    zoom: f64,
    /// User pan offset in window pixels.
    pan_x: f64,
    /// User pan offset in window pixels.
    pan_y: f64,
}

/// The tabbed window's pure state: tabs in insertion order plus the active
/// index. The window is open exactly while this holds at least one tab.
///
/// Pure (no raylib): the ID-addressed add/replace/close rules and the
/// active-index adjustment are testable without a window.
#[derive(Debug, Clone)]
struct TabbedState {
    /// Tabs in insertion order.
    tabs: Vec<TabState>,
    /// Index into `tabs` of the active tab (0 when empty).
    active: usize,
}

impl TabbedState {
    /// No tabs: the window is closed.
    fn new() -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
        }
    }

    /// The number of tabs.
    fn len(&self) -> usize {
        self.tabs.len()
    }

    /// Whether the window is closed (no tabs).
    fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// The active tab's index (0 when empty).
    fn active_index(&self) -> usize {
        self.active
    }

    /// The active tab (`None` when the window has no tabs).
    fn active(&self) -> Option<&TabState> {
        self.tabs.get(self.active)
    }

    /// The index of the tab with ID `id` (`None` for unknown IDs).
    fn index_of(&self, id: &str) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    /// Adds the tab `id` (fresh zoom/pan, made active) or replaces it
    /// (content, label, and zoom/pan reset, made active). A *new* tab is
    /// rejected with a typed [`Error::View`] naming the limit when the
    /// window already holds [`WindowController::MAX_TABS`] tabs; replacing
    /// a known ID always succeeds, at capacity included. Returns the active
    /// index.
    fn show(&mut self, id: &str, label: &str, image: &RenderedImage) -> Result<usize> {
        match self.index_of(id) {
            Some(pos) => {
                let tab = &mut self.tabs[pos];
                tab.label = label.to_string();
                tab.image = image.clone();
                tab.zoom = 1.0;
                tab.pan_x = 0.0;
                tab.pan_y = 0.0;
                self.active = pos;
            }
            None => {
                if self.tabs.len() >= WindowController::MAX_TABS {
                    return Err(tab_capacity_error());
                }
                self.tabs.push(TabState {
                    id: id.to_string(),
                    label: label.to_string(),
                    image: image.clone(),
                    zoom: 1.0,
                    pan_x: 0.0,
                    pan_y: 0.0,
                });
                self.active = self.tabs.len() - 1;
            }
        }
        Ok(self.active)
    }

    /// Removes the tab with ID `id` and adjusts the active index: a
    /// removal before the active tab shifts it down, a removal of the
    /// active tab activates the tab that shifts into its place, and a
    /// removal of the last tab falls back to the previous one. Returns
    /// whether a tab was removed (unknown IDs are a no-op); an empty state
    /// means the window is closed.
    fn close(&mut self, id: &str) -> bool {
        let Some(pos) = self.index_of(id) else {
            return false;
        };
        self.tabs.remove(pos);
        if self.tabs.is_empty() {
            self.active = 0;
        } else if pos < self.active {
            self.active -= 1;
        } else if self.active >= self.tabs.len() {
            // The removed tab was the active (and last) one.
            self.active = self.tabs.len() - 1;
        }
        true
    }
}

/// The open window: its tabbed state plus one raylib texture per tab.
///
/// Lives only on the thread that owns the raylib handle (it is not `Send`).
/// Exposed so the headless pixel tests (under `view-headless`) can build a
/// window of tabs and drive the exact production tabbed frame through
/// [`draw_tabbed_frame`]; the MCP-facing API is [`WindowController`], which
/// owns a `Window` on its dedicated GUI thread.
pub struct Window {
    /// The pure tabbed state (IDs, labels, images, zoom/pan, active index).
    state: TabbedState,
    /// One texture per tab, parallel to `state.tabs`.
    textures: Vec<Texture2D>,
}

impl Window {
    /// An empty window (no tabs): created right after raylib init and
    /// filled by the first `show_tab`.
    pub fn new() -> Self {
        Self {
            state: TabbedState::new(),
            textures: Vec::new(),
        }
    }

    /// Initializes raylib on the *current* thread and opens the window
    /// titled `title` (no tabs yet — the first `show_tab` adds one).
    fn init_raylib(title: &str) -> Result<(RaylibHandle, RaylibThread)> {
        // Route raylib's C-side `TraceLog` — which by default writes to
        // stdout, the MCP server's protocol channel — onto stderr before
        // the window inits. Re-opening the window re-installs the same
        // callback (the setter is idempotent).
        if let Err(e) = set_trace_log_callback(trace_log_to_stderr) {
            eprintln!("mermaid-viewer: could not install the raylib log sink: {e}");
        }
        let (rl, thread) = raylib::init()
            .size(WINDOW_WIDTH, WINDOW_HEIGHT)
            .title(title)
            .resizable()
            .vsync()
            .log_level(raylib_log_level())
            .build();
        Ok((rl, thread))
    }

    /// Adds the tab `id` (unknown ID: fresh zoom/pan) or replaces it (known
    /// ID: content, label, and zoom/pan reset so it re-fits), uploads its
    /// texture, and makes it active.
    ///
    /// A capacity error (the window already holds
    /// [`WindowController::MAX_TABS`] tabs) is reported before any texture
    /// upload, so it costs nothing and leaves the window untouched.
    ///
    /// Public (with the struct) so the headless pixel tests build a window of
    /// tabs; the MCP path reaches the same routine through
    /// [`WindowController::show_tab`].
    pub fn show_tab(
        &mut self,
        rl: &mut RaylibHandle,
        thread: &RaylibThread,
        id: &str,
        label: &str,
        image: &RenderedImage,
    ) -> Result<()> {
        ensure_png(image)?;
        let is_new = self.state.index_of(id).is_none();
        if is_new && self.state.len() >= WindowController::MAX_TABS {
            return Err(tab_capacity_error());
        }
        let texture = load_window_texture(rl, thread, image)?;
        let index = self.state.show(id, label, image)?;
        if is_new {
            self.textures.insert(index, texture);
        } else {
            // Unload the tab's previous texture.
            let old = self.textures.remove(index);
            self.textures.insert(index, texture);
            drop(old);
        }
        Ok(())
    }

    /// Removes the tab `id` and unloads its texture. Returns whether a tab
    /// was removed (unknown IDs are a no-op); the caller closes the window
    /// exactly when `state` becomes empty (the last tab went).
    fn close_tab(&mut self, id: &str) -> bool {
        let Some(pos) = self.state.index_of(id) else {
            return false;
        };
        self.textures.remove(pos);
        self.state.close(id)
    }

    /// Applies wheel-zoom and drag-pan to the *active* tab. Each tab's
    /// zoom/pan lives in its own [`TabState`], so the values persist
    /// independently across tab switches — switching tabs re-targets this
    /// input, never losing the other tabs' state.
    fn apply_input(&mut self, rl: &mut RaylibHandle) {
        let active = self.state.active_index();
        let tab = &mut self.state.tabs[active];
        let wheel = rl.get_mouse_wheel_move();
        if wheel != 0.0 {
            tab.zoom *= if wheel > 0.0 {
                ZOOM_STEP
            } else {
                1.0 / ZOOM_STEP
            };
            tab.zoom = tab.zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        }
        if rl.is_mouse_button_down(MouseButton::MOUSE_BUTTON_LEFT) {
            let delta = rl.get_mouse_delta();
            tab.pan_x += delta.x as f64;
            tab.pan_y += delta.y as f64;
        }
    }

    /// One frame of the *single-diagram* (CLI) window: full-window frame, no
    /// tab strip. This is the [`run_blocking`] path and is unchanged from the
    /// original one-diagram viewer (the image fills the whole window).
    ///
    /// Returns `false` when the window should close (ESC pressed or the
    /// window-close requested by the OS) or when no tab remains.
    fn step_cli(&mut self, rl: &mut RaylibHandle, thread: &RaylibThread) -> bool {
        // Input events are polled by raylib inside `EndDrawing` (the draw
        // handle's drop each frame), which is the standard event pump.
        if rl.window_should_close() || rl.is_key_pressed(KeyboardKey::KEY_ESCAPE) {
            return false;
        }
        // No active tab (the last one went): the caller closes the window.
        if self.state.active().is_none() {
            return false;
        }
        self.apply_input(rl);

        let sw = rl.get_screen_width();
        let sh = rl.get_screen_height();
        let active = self.state.active_index();
        let mut d = rl.begin_drawing(thread);
        draw_frame(
            &mut d,
            &self.textures[active],
            sw,
            sh,
            &self.state.tabs[active].image,
            self.state.tabs[active].zoom,
            self.state.tabs[active].pan_x,
            self.state.tabs[active].pan_y,
        );
        true
    }

    /// One frame of the *tabbed* controller window: the raygui tab bar across
    /// the top strip plus the active tab in the remaining content area.
    ///
    /// Wheel-zoom and drag-pan apply to the active tab (which a tab-bar
    /// selection may change this frame). A tab the user closes through the
    /// bar is removed through the same rules as `close_tab`, its ID is pushed
    /// onto `ui_closes` (for [`WindowController::drain_tab_closes`]), and —
    /// when it was the last tab — the function returns `false` so the GUI
    /// loop closes the window.
    ///
    /// Returns `false` when the window should close (ESC/OS close, no tab
    /// left, or the last tab closed via the bar); `true` to keep the window
    /// open.
    fn step(
        &mut self,
        rl: &mut RaylibHandle,
        thread: &RaylibThread,
        ui_closes: &mut Vec<String>,
    ) -> bool {
        if rl.window_should_close() || rl.is_key_pressed(KeyboardKey::KEY_ESCAPE) {
            return false;
        }
        if self.state.active().is_none() {
            return false;
        }
        self.apply_input(rl);

        let sw = rl.get_screen_width();
        let sh = rl.get_screen_height();
        let mut d = rl.begin_drawing(thread);
        draw_tabbed_frame(&mut d, self, sw, sh, ui_closes)
    }
}

impl Default for Window {
    /// An empty window (no tabs), matching [`Window::new`].
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Public contract
// ---------------------------------------------------------------------------

/// Shows `image` (PNG) in a GUI window titled `title`, on the *current*
/// thread, as one full-window tab. Blocks until the window is closed (ESC
/// or window close), then returns.
///
/// With `MERMAID_VIEWER_FAKE_WINDOW=1` this validates the image and returns
/// immediately (no display, no raylib init).
///
/// SVG input is rejected with [`Error::View`] (and no window is opened).
pub fn run_blocking(title: &str, image: &RenderedImage) -> Result<()> {
    if fake_window_enabled() {
        return ensure_png(image);
    }
    ensure_png(image)?;
    let (mut rl, thread) = Window::init_raylib(title)?;
    let mut window = Window::new();
    window.show_tab(&mut rl, &thread, "default", title, image)?;
    // The CLI view is a single-diagram, full-window frame (no tab strip) —
    // `step_cli` draws exactly that, independent of the controller's tabbed
    // frame path.
    while window.step_cli(&mut rl, &thread) {}
    Ok(())
}

/// Commands to the GUI thread. Plain data only: nothing that is `!Send`
/// ever crosses the channel (the `RaylibThread` token stays on its thread).
enum Cmd {
    /// Add the tab `id` (unknown ID) or replace it (known ID) with `label`
    /// and `image`, and activate it. `ack` reports the GUI-thread result —
    /// window init, texture upload, and the tab-capacity limit included —
    /// for both add and replace, so a `show_tab` is never dropped silently
    /// (one racing a just-closed window re-opens the window).
    ShowTab {
        id: String,
        label: String,
        image: RenderedImage,
        ack: Sender<std::result::Result<(), String>>,
    },
    /// Close the tab `id` (idempotent for unknown IDs; a no-op while the
    /// window is closed; removing the last tab closes the window).
    CloseTab { id: String },
    /// Close the window and all its tabs (idempotent).
    Close,
}

/// A long-lived tabbed diagram window driven from any thread.
///
/// `Send + Sync`: the raylib window lives on a dedicated OS thread; this
/// handle talks to it over a plain-data channel. The window is open exactly
/// while it holds at least one tab: [`show_tab`](Self::show_tab) adds a tab
/// (new ID) or replaces one (known ID) and activates it,
/// [`close_tab`](Self::close_tab) removes one (closing the window with the
/// last tab), and [`close`](Self::close) closes the window and all tabs.
/// The window is re-openable after close.
///
/// With `MERMAID_VIEWER_FAKE_WINDOW=1` a fake backend with the identical
/// observable state is used (no display, no raylib init).
pub struct WindowController {
    inner: Inner,
}

enum Inner {
    /// Real backend: dedicated GUI thread + channel + UI-close queue.
    ///
    /// Behind a single mutex because `show_tab` may *replace* a dead GUI
    /// thread (fresh channel + fresh open/stopped state) under the lock.
    /// The lock is only ever held across non-blocking channel operations,
    /// never across the acknowledgement wait.
    Real(Mutex<RealView>),
    /// Fake backend: the tabbed state machine only (tab IDs).
    Fake { tabs: Mutex<Vec<String>> },
}

/// The real backend's state: the window title (kept for GUI-thread
/// respawns), the command channel, the open/stopped state, the UI-close
/// queue, and the GUI-thread handle.
struct RealView {
    /// The OS window title used when the window is opened (kept so a dead
    /// GUI thread can be re-spawned with the same title).
    title: String,
    tx: Sender<Cmd>,
    open: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    /// Tab IDs closed through the window's tab bar, drained by
    /// [`WindowController::drain_tab_closes`] (the fake backend
    /// synthesizes none). Filled by the tab-bar UI (M-1.b); empty until
    /// then. Drain-safe from any thread.
    ui_closes: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl RealView {
    /// Spawns (or re-spawns) the dedicated GUI thread and returns the
    /// channel plus the fresh open/stopped state for it.
    fn new(title: String) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let open = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let ui_closes = Arc::new(Mutex::new(Vec::new()));
        let (open_shared, stopped_shared, ui_shared) = (
            Arc::clone(&open),
            Arc::clone(&stopped),
            Arc::clone(&ui_closes),
        );
        let gui_title = title.clone();
        let thread = Builder::new()
            .name("mermaid-viewer-gui".into())
            .spawn(move || gui_loop(rx, open_shared, stopped_shared, ui_shared, gui_title))
            .map_err(|e| Error::View(format!("failed to spawn GUI thread: {e}")))?;
        Ok(Self {
            title,
            tx,
            open,
            stopped,
            ui_closes,
            thread: Some(thread),
        })
    }
}

impl WindowController {
    /// The maximum number of tabs a single window holds. A `show_tab` with
    /// an unknown ID is rejected with a typed [`Error::View`] when the
    /// window already holds `MAX_TABS` tabs; replacing a known ID at
    /// capacity always succeeds.
    pub const MAX_TABS: usize = 16;

    /// Spawns the controller for a window titled `title` (initially CLOSED:
    /// no tabs).
    ///
    /// The backend is chosen from `MERMAID_VIEWER_FAKE_WINDOW` at this call.
    pub fn spawn(title: &str) -> Result<Self> {
        if fake_window_enabled() {
            return Ok(WindowController {
                inner: Inner::Fake {
                    tabs: Mutex::new(Vec::new()),
                },
            });
        }
        Ok(WindowController {
            inner: Inner::Real(Mutex::new(RealView::new(title.to_string())?)),
        })
    }

    /// How long [`WindowController::show_tab`] waits for the GUI thread to
    /// acknowledge a tab before reporting a typed [`Error::View`]. A GUI
    /// thread that wedges (e.g. the display server never delivers the
    /// window-visibility event, so window creation never returns) must not
    /// drag every subsequent MCP tool call down with it: the MCP layer
    /// turns this failure into a tool error and the server keeps serving.
    pub const GUI_ACK_TIMEOUT: Duration = Duration::from_secs(10);

    /// The grace period [`WindowController`] gives the GUI thread to finish
    /// on drop before the join is detached (a wedged GUI thread must not
    /// hang the process shutdown; process exit tears the thread down).
    const GUI_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

    /// Adds a tab with ID `id` (label `label`, content `image`) or replaces
    /// it when the ID is already known: the replacement takes the tab's
    /// label and content, resets its zoom/pan so it re-fits, and makes the
    /// tab active in both cases.
    ///
    /// `image` must be PNG (`Error::View` otherwise). The window is opened
    /// when CLOSED, including a call that races a just-closed window (never
    /// silently dropped). A new tab beyond [`Self::MAX_TABS`] is rejected
    /// with a typed [`Error::View`].
    ///
    /// Blocks **at most [`Self::GUI_ACK_TIMEOUT`]** until the GUI thread has
    /// processed the operation — both add and replace — and reports its
    /// failure (window init, texture upload, capacity limit, a dead GUI
    /// thread — re-spawned, — or a wedged one — reported, never hung) as a
    /// typed [`Error::View`].
    pub fn show_tab(&self, id: &str, label: &str, image: &RenderedImage) -> Result<()> {
        ensure_png(image)?;
        match &self.inner {
            Inner::Fake { tabs } => {
                let mut tabs = tabs.lock().unwrap_or_else(|e| e.into_inner());
                if tabs.iter().any(|t| t.as_str() == id) {
                    // Replace: the ID already occupies its single slot.
                    Ok(())
                } else if tabs.len() >= Self::MAX_TABS {
                    Err(tab_capacity_error())
                } else {
                    tabs.push(id.to_string());
                    Ok(())
                }
            }
            Inner::Real(view) => {
                let ack_rx = {
                    let view = view.lock().unwrap_or_else(|e| e.into_inner());
                    let (ack_tx, ack_rx) = mpsc::channel();
                    view.tx
                        .send(Cmd::ShowTab {
                            id: id.to_string(),
                            label: label.to_string(),
                            image: image.clone(),
                            ack: ack_tx,
                        })
                        .map_err(|_| {
                            Error::View("window GUI thread is no longer running".to_string())
                        })?;
                    ack_rx
                };
                match ack_rx.recv_timeout(Self::GUI_ACK_TIMEOUT) {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(msg)) => Err(Error::View(msg)),
                    // The GUI thread never acknowledged: it is wedged (alive
                    // but blocked in a call the display server never
                    // completes) or it died after taking the command. In
                    // both cases this is a typed tool-level failure, never
                    // a hang.
                    Err(RecvTimeoutError::Timeout) => Err(Error::View(self.recover_dead_gui(view))),
                    Err(RecvTimeoutError::Disconnected) => Err(Error::View(
                        "window GUI thread exited before the tab was shown".to_string(),
                    )),
                }
            }
        }
    }

    /// Ack-wait timeout recovery: called when the GUI thread did not
    /// acknowledge a `show_tab` within [`Self::GUI_ACK_TIMEOUT`].
    ///
    /// If the GUI thread **died** without acknowledging, its raylib state is
    /// gone with it, so a fresh GUI thread (new channel + fresh open/stopped
    /// state) is re-spawned — the next `show_tab` can then open the window.
    /// If the thread is **alive but wedged** (e.g. `glfwCreateWindow`
    /// blocked on a visibility event the display server never delivers), it
    /// is *not* re-spawned: two live GUI threads would both own raylib's
    /// process-global state and a zombie second window could open. The
    /// wedge is reported as-is: the MCP server answers the tool call with
    /// this error and stays responsive.
    ///
    /// Returns the message for the caller's [`Error::View`].
    fn recover_dead_gui(&self, view: &Mutex<RealView>) -> String {
        let (dead, title) = {
            let view = view.lock().unwrap_or_else(|e| e.into_inner());
            (
                view.thread.as_ref().is_some_and(JoinHandle::is_finished),
                view.title.clone(),
            )
        };
        if !dead {
            return format!(
                "window GUI thread did not acknowledge the tab within {} (the GUI thread appears stuck, e.g. the display server is unresponsive); the window stays closed until the display recovers",
                Self::GUI_ACK_TIMEOUT.as_secs()
            );
        }
        let mut view = view.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = view.thread.take() {
            // Reap the dead handle: joining a finished thread cannot block,
            // and a panic payload is captured, not re-raised.
            let _ = t.join();
        }
        match RealView::new(title) {
            Ok(fresh) => {
                *view = fresh;
                "window GUI thread died while opening the tab; a fresh GUI thread was started — retry the show".to_string()
            }
            Err(e) => e.to_string(),
        }
    }

    /// Closes the tab with ID `id`. Idempotent: an unknown ID and a closed
    /// window are no-ops. On the real backend, removing the last tab closes
    /// the window.
    pub fn close_tab(&self, id: &str) -> Result<()> {
        match &self.inner {
            Inner::Fake { tabs } => {
                let mut tabs = tabs.lock().unwrap_or_else(|e| e.into_inner());
                tabs.retain(|t| t.as_str() != id);
                Ok(())
            }
            Inner::Real(view) => {
                let view = view.lock().unwrap_or_else(|e| e.into_inner());
                view.tx
                    .send(Cmd::CloseTab { id: id.to_string() })
                    .map_err(|_| Error::View("window GUI thread is no longer running".to_string()))
            }
        }
    }

    /// Returns the IDs of tabs closed through the window's tab bar since
    /// the previous drain, so an MCP caller can reconcile its registry.
    ///
    /// Controller-initiated `close_tab`/`close` are already known to the
    /// caller and are not reported here. The fake backend has no UI and
    /// always returns an empty vec.
    pub fn drain_tab_closes(&self) -> Vec<String> {
        match &self.inner {
            Inner::Real(view) => {
                let view = view.lock().unwrap_or_else(|e| e.into_inner());
                let mut queue = view.ui_closes.lock().unwrap_or_else(|e| e.into_inner());
                queue.drain(..).collect()
            }
            Inner::Fake { .. } => Vec::new(),
        }
    }

    /// Closes the window and all its tabs. Idempotent: closing a closed
    /// window is a no-op.
    pub fn close(&self) {
        match &self.inner {
            Inner::Fake { tabs } => {
                let mut tabs = tabs.lock().unwrap_or_else(|e| e.into_inner());
                tabs.clear();
            }
            Inner::Real(view) => {
                let view = view.lock().unwrap_or_else(|e| e.into_inner());
                let _ = view.tx.send(Cmd::Close);
            }
        }
    }

    /// Whether the window is currently open (it has at least one tab).
    pub fn is_open(&self) -> bool {
        match &self.inner {
            Inner::Fake { tabs } => !tabs.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            Inner::Real(view) => {
                let view = view.lock().unwrap_or_else(|e| e.into_inner());
                view.open.load(Ordering::Acquire)
            }
        }
    }
}

impl Drop for WindowController {
    fn drop(&mut self) {
        if let Inner::Real(view) = &mut self.inner {
            // `std::sync::mpsc` has no injected-shutdown sender, so the
            // controller tells the GUI thread to stop via the flag (observed
            // at most 200 ms later on an idle thread, next frame while open);
            // then joins it so the raylib window is fully closed before this
            // handle is gone.
            let (stopped, thread) = {
                let view = view.get_mut().unwrap();
                (Arc::clone(&view.stopped), view.thread.take())
            };
            stopped.store(true, Ordering::Release);
            if let Some(h) = thread {
                // A wedged GUI thread (stuck inside a display-server call
                // that never returns) would hang an unbounded join — and
                // with it the process shutdown — so the join is bounded:
                // after the grace period the joiner is detached and the
                // process exit tears the thread down with its window.
                bounded_join(h, Self::GUI_SHUTDOWN_GRACE);
            }
        }
    }
}

/// Joins `h` for at most `timeout`. If the thread has not finished in
/// time, the join is *detached* so the caller is never blocked; the
/// joiner thread then keeps waiting in the background, harmlessly (at
/// process exit it is torn down with the thread it joins).
fn bounded_join(h: JoinHandle<()>, timeout: Duration) {
    let (done_tx, done_rx) = mpsc::channel();
    let joiner = Builder::new()
        .name("mermaid-viewer-gui-join".into())
        .spawn(move || {
            let _ = h.join();
            let _ = done_tx.send(());
        });
    if let Ok(joiner) = joiner {
        if done_rx.recv_timeout(timeout).is_err() {
            std::mem::forget(joiner);
        }
    }
}

// ---------------------------------------------------------------------------
// GUI thread (owns the raylib handle for its whole lifetime)
// ---------------------------------------------------------------------------

/// The OPEN-state drain: executes the queued commands in order, one at a
/// time, until the channel is empty or the window latches closed.
///
/// `apply` executes one command on the live window and reports whether it
/// latched the window closed (a `Close`, or a `CloseTab` that removed the
/// last tab). The drain stops *immediately* on such a latch — and on the
/// channel disconnecting (the controller is gone) — leaving every later
/// queued command in the channel: the CLOSED wait loop re-openes the window
/// with a queued `ShowTab` and acknowledges it there, so a `show_tab`
/// racing a just-closed window is executed and acknowledged exactly once,
/// never acknowledged as success here and dropped with the window.
fn run_drain(rx: &Receiver<Cmd>, apply: &mut dyn FnMut(&Cmd) -> bool) -> bool {
    let mut latched = false;
    while !latched {
        match rx.try_recv() {
            Ok(cmd) => latched = apply(&cmd),
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => return true,
        }
    }
    latched
}

/// Body of the dedicated GUI thread.
///
/// Loops forever: while CLOSED it waits on the channel (a `ShowTab` there
/// opens the window — titled `title` — with that tab, and a `CloseTab`/
/// `Close` is an idempotent no-op; a disconnected channel or a stopped flag
/// means the controller is gone); while OPEN it drains the channel every
/// frame (non-blocking, so events keep pumping), stopping the drain exactly
/// at a latched close so any queued `ShowTab` survives for the CLOSED wait
/// loop. The thread outlives `close` so the window can be re-opened: raylib
/// re-initializes on this same thread after `CloseWindow`.
///
/// The wait uses a timeout so the thread can observe `stopped` even while
/// idle (the controller's `Drop` sets it; `std::sync::mpsc` has no way to
/// inject shutdown into a blocked `recv`).
///
/// `ui_closes` receives the IDs of tabs the user closes through the window's
/// tab bar: the tabbed frame pushes them into a per-frame local buffer, and
/// the loop publishes that buffer to `ui_closes` only *after* the (possibly
/// blocking) draw has returned, so the shared queue's lock is never held
/// across the draw; the controller drains `ui_closes` with
/// `drain_tab_closes` so the MCP registry can reconcile. Controller
/// -initiated `close_tab`/`close` are not reported there.
fn gui_loop(
    rx: Receiver<Cmd>,
    open: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    ui_closes: Arc<Mutex<Vec<String>>>,
    title: String,
) {
    loop {
        // CLOSED: wait until a ShowTab arrives; it re-opens the window with
        // the controller's spawn title as the OS title, so a show racing a
        // just-closed window is never dropped.
        let (id, label, image, ack) = loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Cmd::ShowTab {
                    id,
                    label,
                    image,
                    ack,
                }) => break (id, label, image, ack),
                Ok(Cmd::Close) | Ok(Cmd::CloseTab { .. }) => continue,
                Err(RecvTimeoutError::Timeout) => {
                    if stopped.load(Ordering::Acquire) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };

        let (mut rl, thread) = match Window::init_raylib(&title) {
            Ok(w) => w,
            Err(e) => {
                let _ = ack.send(Err(e.to_string()));
                continue;
            }
        };

        let mut window = Window::new();
        if let Err(e) = window.show_tab(&mut rl, &thread, &id, &label, &image) {
            // Window/GL init or the texture upload failed: report it, close
            // the freshly opened window, and loop back to wait for the next
            // ShowTab.
            let _ = ack.send(Err(e.to_string()));
            continue;
        }

        open.store(true, Ordering::Release);
        let _ = ack.send(Ok(()));

        let mut closed = false;
        while !closed {
            // A dropped controller stops the thread on the next frame.
            if stopped.load(Ordering::Acquire) {
                break;
            }
            // Drain all pending commands before drawing this frame so a
            // queued tab operation is never painted over by a stale frame.
            // The drain stops exactly at a latched close (an unknown-ID
            // `CloseTab` in the batch is not one: it is a no-op), leaving a
            // queued `ShowTab` behind for the CLOSED wait loop, which
            // re-opens the window with it and acknowledges it there.
            let mut apply = |cmd: &Cmd| -> bool {
                match cmd {
                    Cmd::ShowTab {
                        id,
                        label,
                        image,
                        ack,
                    } => {
                        if let Err(e) = window.show_tab(&mut rl, &thread, id, label, image) {
                            // The window stays up; ack the failure and
                            // report to stderr, never panic.
                            eprintln!("mermaid-viewer: tab show failed: {e}");
                            let _ = ack.send(Err(e.to_string()));
                        } else {
                            let _ = ack.send(Ok(()));
                        }
                        false
                    }
                    Cmd::CloseTab { id } => {
                        // Removing the last tab closes the window.
                        window.close_tab(id) && window.state.is_empty()
                    }
                    Cmd::Close => true,
                }
            };
            if run_drain(&rx, &mut apply) {
                closed = true;
            }

            if !closed {
                // The tabbed frame pushes any bar-closed tab ID for this frame into
                // a local buffer; it is published to the shared queue only *after*
                // the (potentially blocking) draw has returned. The shared queue
                // lock is therefore never held across `window.step`/`EndDrawing` —
                // a display server that jams inside the draw must not be able to
                // block `drain_tab_closes` (and, behind the State mutex, every MCP
                // commit) on a frame lock.
                let mut frame_closes = Vec::new();
                if !window.step(&mut rl, &thread, &mut frame_closes) {
                    closed = true;
                }
                if !frame_closes.is_empty() {
                    let mut q = ui_closes.lock().unwrap_or_else(|e| e.into_inner());
                    q.extend(frame_closes);
                }
            }
        }

        open.store(false, Ordering::Release);
        // Drop `window` (unloads every tab texture) and `rl` (CloseWindow)
        // before looping back to wait for the next ShowTab.
    }
}

// ---------------------------------------------------------------------------
// Tests (fake backend + layout math: run under every feature pass)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A small solid-color PNG, built directly (no mermaid source needed).
    fn test_png(width: u32, height: u32, rgba: [u8; 4]) -> RenderedImage {
        let bytes = {
            let mut bytes = Vec::new();
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("valid PNG header");
            // png 0.18's `write_image_data` takes the *entire* image buffer
            // in one call (per-row writing is not an API in 0.18).
            let mut data: Vec<u8> = Vec::with_capacity((width * height) as usize * 4);
            for _ in 0..(width * height) {
                data.extend_from_slice(&rgba);
            }
            writer
                .write_image_data(&data)
                .expect("valid PNG image data");
            // The writer's `Drop` impl keeps the borrow on `bytes` alive
            // until it is explicitly dropped.
            drop(writer);
            bytes
        };
        RenderedImage {
            format: ImageFormat::Png,
            width,
            height,
            bytes,
        }
    }

    /// The `WindowController` is the MCP server's handle: it must be
    /// shareable across threads (tokio runtime workers + controller).
    #[test]
    fn controller_is_send_sync() {
        fn requires_send_sync<T: Send + Sync>() {}
        requires_send_sync::<WindowController>();
    }

    /// Release builds (no debug assertions) suppress raylib's INFO-level
    /// window chatter (threshold `LOG_WARNING`); debug builds keep the
    /// crate default (`LOG_INFO`) so the init trace stays visible for
    /// development.
    #[test]
    fn log_level_is_warning_in_release() {
        use raylib::consts::TraceLogLevel;
        if cfg!(not(debug_assertions)) {
            assert_eq!(raylib_log_level(), TraceLogLevel::LOG_WARNING);
        } else {
            assert_eq!(raylib_log_level(), TraceLogLevel::LOG_INFO);
        }
    }

    /// Shared verification plan: `fake_tab_lifecycle`.
    ///
    /// With the fake env var and a spawned controller: show "a" (OPEN),
    /// show "b" (OPEN), close "a" while "b" keeps the window OPEN, close
    /// the last tab "b" (CLOSED), then show again (re-open). The fake
    /// backend never touches raylib, so no display and no GL context is
    /// initialized for any of it (there is no window in this process at
    /// all).
    #[test]
    fn fake_tab_lifecycle() {
        std::env::set_var(FAKE_WINDOW_ENV, "1");

        let image = test_png(4, 4, [10, 20, 30, 255]);
        let controller = WindowController::spawn("lifecycle").expect("fake spawn");
        assert!(!controller.is_open(), "spawned controller starts CLOSED");

        controller
            .show_tab("a", "A", &image)
            .expect("first show_tab opens the window");
        assert!(controller.is_open(), "show_tab → OPEN");

        controller
            .show_tab("b", "B", &image)
            .expect("a second ID adds a tab");
        assert!(controller.is_open(), "a second tab keeps the window OPEN");

        controller
            .close_tab("a")
            .expect("close_tab of one tab while another remains");
        assert!(
            controller.is_open(),
            "closing a tab while another remains keeps the window OPEN"
        );

        controller
            .close_tab("b")
            .expect("close_tab of the last tab");
        assert!(!controller.is_open(), "closing the last tab → CLOSED");

        controller
            .show_tab("a", "A", &image)
            .expect("show_tab after close re-opens");
        assert!(controller.is_open(), "re-open works");

        controller.close();
        controller.close();
        assert!(!controller.is_open(), "close is idempotent");
    }

    /// Shared verification plan: `fake_tab_replace`.
    ///
    /// Showing the same ID twice replaces the tab rather than duplicating
    /// it: with the window at capacity, the duplicate must not have taken
    /// a second slot (the 15 fresh IDs below all fit).
    #[test]
    fn fake_tab_replace() {
        std::env::set_var(FAKE_WINDOW_ENV, "1");

        let image = test_png(4, 4, [10, 20, 30, 255]);
        let controller = WindowController::spawn("replace").expect("fake spawn");

        controller
            .show_tab("a", "A", &image)
            .expect("first show adds the tab");
        controller
            .show_tab("a", "A again", &image)
            .expect("a second show of the same ID replaces, never fails");
        assert!(controller.is_open(), "the window stays OPEN");

        // If the duplicate had counted against the limit, one of these 15
        // fresh IDs would have to fail: it does not, so exactly one "a"
        // tab exists.
        for i in 1..=15 {
            let id = format!("tab-{i}");
            controller
                .show_tab(&id, &id, &image)
                .expect("fresh ID while capacity allows");
        }
        assert!(
            matches!(
                controller.show_tab("tab-16", "tab-16", &image),
                Err(Error::View(_))
            ),
            "17th distinct ID is at the limit"
        );
    }

    /// Shared verification plan: `fake_tab_cap`.
    ///
    /// 16 distinct IDs succeed; the 17th fails with a typed [`Error::View`]
    /// naming the 16-tab limit; the 16 tabs remain (the failed call is not
    /// silently dropped and does not evict anything); and replacement at
    /// capacity succeeds.
    #[test]
    fn fake_tab_cap() {
        std::env::set_var(FAKE_WINDOW_ENV, "1");

        let image = test_png(4, 4, [10, 20, 30, 255]);
        let controller = WindowController::spawn("cap").expect("fake spawn");

        for i in 0..WindowController::MAX_TABS {
            let id = format!("cap-{i}");
            controller
                .show_tab(&id, &id, &image)
                .expect("tab within the capacity limit");
        }
        assert!(controller.is_open(), "a full window is OPEN");

        let err = controller
            .show_tab("cap-16", "cap-16", &image)
            .expect_err("the 17th distinct ID exceeds the limit");
        let Error::View(msg) = &err else {
            panic!("capacity failure is a typed View error, got: {err:?}")
        };
        assert!(
            msg.contains("16"),
            "the capacity error names the 16-tab limit: {msg}"
        );

        // The 16 tabs remain: the same ID is still rejected…
        assert!(
            matches!(
                controller.show_tab("cap-16", "cap-16", &image),
                Err(Error::View(_))
            ),
            "the rejected tab was not added"
        );
        // …and replacement at capacity succeeds.
        controller
            .show_tab("cap-0", "cap-0 again", &image)
            .expect("replacement at capacity succeeds");
        controller
            .show_tab("cap-15", "cap-15 again", &image)
            .expect("any known ID replaces at capacity");

        // And the window is still there: closing the last tab closes it.
        for i in (0..WindowController::MAX_TABS).rev() {
            let id = format!("cap-{i}");
            controller.close_tab(&id).expect("close_tab of a known tab");
            if i > 0 {
                assert!(controller.is_open(), "other tabs keep the window OPEN");
            }
        }
        assert!(
            !controller.is_open(),
            "closing the last tab closed the window"
        );
    }

    /// Shared verification plan: `fake_close_unknown_tab_is_noop`.
    ///
    /// Closing an unknown ID is an idempotent no-op — on an open window
    /// (the current tabs are left unchanged) and on a closed window alike
    /// — and keeps returning `Ok`.
    #[test]
    fn fake_close_unknown_tab_is_noop() {
        std::env::set_var(FAKE_WINDOW_ENV, "1");

        let image = test_png(4, 4, [10, 20, 30, 255]);
        let controller = WindowController::spawn("unknown-close").expect("fake spawn");

        controller.show_tab("a", "A", &image).expect("add a");
        controller.show_tab("b", "B", &image).expect("add b");

        // Closing an unknown ID on an open window changes nothing…
        controller
            .close_tab("nope")
            .expect("closing an unknown tab is a no-op");
        assert!(
            controller.is_open(),
            "the open window is untouched by an unknown close"
        );

        // …and so is closing an unknown ID on a closed window.
        controller.close();
        assert!(!controller.is_open(), "close → CLOSED");
        controller
            .close_tab("nope")
            .expect("closing an unknown tab on a closed window is a no-op");
        assert!(!controller.is_open(), "the closed window stays CLOSED");

        // The current tabs survived: re-open with both and close them one
        // by one — both "a" and "b" are still there.
        controller
            .show_tab("a", "A", &image)
            .expect("re-open with a");
        controller
            .show_tab("b", "B", &image)
            .expect("b is still there");
        controller.close_tab("a").expect("close a");
        assert!(controller.is_open(), "b keeps the window OPEN");
        controller.close_tab("b").expect("close b");
        assert!(!controller.is_open(), "b was the last tab");
    }

    /// Regression (no raylib, no timing): the OPEN-state drain stops
    /// *immediately* at a latched close and never executes the commands
    /// queued behind it. A `ShowTab` queued after a `Close`, after a
    /// last-tab `CloseTab`, must stay in the channel for the CLOSED wait
    /// loop, which re-opens the window with it and acknowledges it there —
    /// a `show_tab` racing a just-closed window is never acknowledged as
    /// success and dropped with the window. The same latch must hold when
    /// the channel disconnects (the controller is gone): the drain stops,
    /// it does not spin.
    #[test]
    fn drain_stops_at_latched_close_and_leaves_queued_show_tab() {
        let image = test_png(2, 2, [1, 2, 3, 255]);

        // Each sequence owns its fake window model + recorder standing in
        // for the live window: `apply` mirrors the real rules (add/replace,
        // remove with an unknown-ID no-op, `Close` latches) and records
        // every execution.

        // (1) `Close` followed by a queued `ShowTab`: the drain latches on
        // the `Close` and the `ShowTab` survives in the channel.
        {
            let mut tabs = vec!["a".to_string()];
            let mut applied: Vec<String> = Vec::new();
            let mut apply = |cmd: &Cmd| -> bool {
                match cmd {
                    Cmd::ShowTab { id, .. } => {
                        applied.push(format!("show:{id}"));
                        if !tabs.iter().any(|t| t == id) {
                            tabs.push(id.clone());
                        }
                        false
                    }
                    Cmd::CloseTab { id } => {
                        applied.push(format!("close_tab:{id}"));
                        let removed = tabs.iter().position(|t| t == id).is_some();
                        if removed {
                            tabs.retain(|t| t != id);
                        }
                        removed && tabs.is_empty()
                    }
                    Cmd::Close => {
                        applied.push("close".to_string());
                        true
                    }
                }
            };
            let (tx, rx) = mpsc::channel();
            let (ack, _ack_rx) = mpsc::channel();
            tx.send(Cmd::Close).expect("send Close");
            tx.send(Cmd::ShowTab {
                id: "b".into(),
                label: "B".into(),
                image: image.clone(),
                ack,
            })
            .expect("send queued ShowTab");
            drop(tx);
            assert!(run_drain(&rx, &mut apply), "Close latches the drain");
            assert_eq!(
                applied,
                vec!["close"],
                "a ShowTab queued behind a Close is never executed by the drain"
            );
            let queued = rx
                .recv()
                .expect("the queued ShowTab survives for the CLOSED wait loop");
            assert!(
                matches!(&queued, Cmd::ShowTab { id, .. } if id == "b"),
                "the surviving command is the queued ShowTab"
            );
        }

        // (2) A last-tab `CloseTab` followed by a queued `ShowTab`: same
        // contract — the drain latches and the `ShowTab` stays behind.
        {
            let mut tabs = vec!["a".to_string()];
            let mut applied: Vec<String> = Vec::new();
            let mut apply = |cmd: &Cmd| -> bool {
                match cmd {
                    Cmd::ShowTab { id, .. } => {
                        applied.push(format!("show:{id}"));
                        if !tabs.iter().any(|t| t == id) {
                            tabs.push(id.clone());
                        }
                        false
                    }
                    Cmd::CloseTab { id } => {
                        applied.push(format!("close_tab:{id}"));
                        let removed = tabs.iter().position(|t| t == id).is_some();
                        if removed {
                            tabs.retain(|t| t != id);
                        }
                        removed && tabs.is_empty()
                    }
                    Cmd::Close => {
                        applied.push("close".to_string());
                        true
                    }
                }
            };
            let (tx, rx) = mpsc::channel();
            let (ack, _ack_rx) = mpsc::channel();
            tx.send(Cmd::CloseTab { id: "a".into() })
                .expect("send CloseTab");
            tx.send(Cmd::ShowTab {
                id: "c".into(),
                label: "C".into(),
                image: image.clone(),
                ack,
            })
            .expect("send queued ShowTab");
            drop(tx);
            assert!(
                run_drain(&rx, &mut apply),
                "removing the last tab latches the drain"
            );
            assert_eq!(
                applied,
                vec!["close_tab:a"],
                "a ShowTab queued behind a last-tab CloseTab is never executed"
            );
            assert!(
                matches!(
                    &rx.recv().expect("queued ShowTab survives"),
                    Cmd::ShowTab { .. }
                ),
                "the surviving command is the queued ShowTab"
            );
        }

        // (3) A non-last-tab `CloseTab` does not latch: the batch runs to
        // completion.
        {
            let mut tabs = vec!["a".to_string(), "b".to_string()];
            let mut applied: Vec<String> = Vec::new();
            let mut apply = |cmd: &Cmd| -> bool {
                match cmd {
                    Cmd::ShowTab { id, .. } => {
                        applied.push(format!("show:{id}"));
                        if !tabs.iter().any(|t| t == id) {
                            tabs.push(id.clone());
                        }
                        false
                    }
                    Cmd::CloseTab { id } => {
                        applied.push(format!("close_tab:{id}"));
                        let removed = tabs.iter().position(|t| t == id).is_some();
                        if removed {
                            tabs.retain(|t| t != id);
                        }
                        removed && tabs.is_empty()
                    }
                    Cmd::Close => {
                        applied.push("close".to_string());
                        true
                    }
                }
            };
            let (tx, rx) = mpsc::channel();
            tx.send(Cmd::CloseTab { id: "a".into() })
                .expect("send CloseTab");
            tx.send(Cmd::Close).expect("send Close");
            drop(tx);
            assert!(run_drain(&rx, &mut apply), "Close latches the drain");
            assert_eq!(
                applied,
                vec!["close_tab:a", "close"],
                "a non-latching CloseTab does not stop the drain"
            );
        }

        // (4) A disconnected channel latches the drain immediately: nothing
        // is applied and the drain does not spin.
        {
            let mut applied: Vec<String> = Vec::new();
            let mut apply = |_cmd: &Cmd| -> bool {
                applied.push("applied".to_string());
                false
            };
            let (tx, rx) = mpsc::channel::<Cmd>();
            drop(tx);
            assert!(
                run_drain(&rx, &mut apply),
                "disconnection latches the drain"
            );
            assert!(
                applied.is_empty(),
                "nothing is applied on a disconnected drain"
            );
        }
    }

    /// Shared verification plan: `tab_close_index_updates_active`.
    ///
    /// Pure state logic (no raylib, no fake env, runs under every feature
    /// pass): closing a tab adjusts the active index — a removal before
    /// the active tab shifts it down, a removal of the active tab
    /// activates the tab that shifts into its place, and a removal of the
    /// last tab falls back to the previous one — and closing the last tab
    /// empties the window (which is closed).
    #[test]
    fn tab_close_index_updates_active() {
        let image = test_png(2, 2, [1, 2, 3, 255]);

        let mut tabs = TabbedState::new();
        tabs.show("a", "A", &image).expect("add a");
        tabs.show("b", "B", &image).expect("add b");
        tabs.show("c", "C", &image).expect("add c");
        assert_eq!(tabs.active_index(), 2, "the last shown tab is active");
        assert!(tabs.index_of("a").is_some(), "a is present");
        assert!(tabs.index_of("b").is_some(), "b is present");
        assert!(tabs.index_of("c").is_some(), "c is present");

        // Removing the last (active) tab activates the previous one.
        assert!(tabs.close("c"), "c is removed");
        assert_eq!(tabs.len(), 2, "two tabs remain");
        assert_eq!(tabs.active_index(), 1, "the previous tab becomes active");
        assert_eq!(tabs.active().expect("an active tab").id, "b");

        // Removing a tab before the active one shifts the index down.
        assert!(tabs.close("a"), "a is removed");
        assert_eq!(tabs.len(), 1, "one tab remains");
        assert_eq!(tabs.active_index(), 0, "the index shifts down");
        assert_eq!(tabs.active().expect("an active tab").id, "b");

        // Removing a tab that is not the active one keeps the index: with
        // [b, c] and c active, removing b leaves c active.
        tabs.show("c", "C", &image).expect("re-add c");
        assert_eq!(tabs.active_index(), 1, "c is active again");
        assert!(tabs.close("b"), "b is removed");
        assert_eq!(tabs.len(), 1, "one tab remains");
        assert_eq!(tabs.active_index(), 0, "the sole remaining tab is active");
        assert_eq!(tabs.active().expect("an active tab").id, "c");

        // Closing the last tab empties the window (which is closed).
        assert!(tabs.close("c"), "the last tab is removed");
        assert!(tabs.is_empty(), "no tabs left: the window is closed");
        assert_eq!(
            tabs.active_index(),
            0,
            "the empty state keeps a valid index"
        );
        assert!(!tabs.close("c"), "closing an unknown ID is a no-op");
    }

    /// Shared verification plan: `view_rejects_svg_image`.
    ///
    /// An SVG [`RenderedImage`] is rejected with [`Error::View`] by
    /// `show_tab` — in both the CLOSED and OPEN states, leaving the window
    /// state unchanged — and by [`run_blocking`], under the fake backend.
    #[test]
    fn view_rejects_svg_image() {
        std::env::set_var(FAKE_WINDOW_ENV, "1");

        let svg = RenderedImage {
            format: ImageFormat::Svg,
            width: 100,
            height: 50,
            bytes: b"<?xml version=\"1.0\"?><svg width=\"100\" height=\"50\"></svg>".to_vec(),
        };
        let png = test_png(4, 4, [0, 0, 0, 255]);

        let controller = WindowController::spawn("svg-reject").expect("fake spawn");
        assert!(
            matches!(
                controller.show_tab("diag", "diag", &svg),
                Err(Error::View(_))
            ),
            "SVG rejected while CLOSED"
        );
        assert!(
            !controller.is_open(),
            "a rejected show_tab did not open the window"
        );

        controller
            .show_tab("diag", "diag", &png)
            .expect("PNG opens the window");
        assert!(
            matches!(
                controller.show_tab("diag2", "diag2", &svg),
                Err(Error::View(_))
            ),
            "SVG rejected while OPEN"
        );
        assert!(
            controller.is_open(),
            "the window stays open after a rejection"
        );

        // The window state is unchanged by the rejection: the known tab is
        // still replaceable, and 15 fresh IDs still fit — had the rejected
        // tab taken a slot, these would already be at the capacity limit.
        controller
            .show_tab("diag", "diag", &png)
            .expect("the known tab is still replaceable");
        for i in 0..15 {
            let id = format!("ok-{i}");
            controller
                .show_tab(&id, &id, &png)
                .expect("capacity is unchanged by the rejection");
        }

        assert!(
            matches!(run_blocking("svg-reject", &svg), Err(Error::View(_))),
            "run_blocking rejects SVG input"
        );
        controller.close();
    }

    /// `run_blocking` under the fake backend validates and returns
    /// immediately (no display, no blocking).
    #[test]
    fn run_blocking_is_immediate_under_fake_env() {
        std::env::set_var(FAKE_WINDOW_ENV, "1");
        let png = test_png(2, 2, [255, 255, 255, 255]);
        assert!(run_blocking("fake", &png).is_ok());
    }

    /// Shared verification plan: `fit_layout_math`.
    ///
    /// A 100×50 image in an 800×600 window at zoom 1: scaled to fit (8×),
    /// exactly 800×400, centered, aspect preserved, no overflow.
    #[test]
    fn fit_layout_math() {
        let l = compute_layout(800.0, 600.0, 100.0, 50.0, 1.0, 0.0, 0.0);

        // Scaled to fit without overflow.
        assert!(
            (l.width - 800.0).abs() < f64::EPSILON,
            "width fits exactly: {}",
            l.width
        );
        assert!(
            (l.height - 400.0).abs() < f64::EPSILON,
            "height: {}",
            l.height
        );
        assert!(l.width <= 800.0 && l.height <= 600.0, "no overflow");

        // Centered in the window.
        assert_eq!((l.x + l.width / 2.0, l.y + l.height / 2.0), (400.0, 300.0));

        // Aspect preserved.
        assert!((l.height / l.width - 0.5).abs() < 1e-9, "aspect ratio");

        // Zoom multiplies the fit scale; pan shifts the rect.
        let zoomed = compute_layout(800.0, 600.0, 100.0, 50.0, 2.0, 10.0, -5.0);
        assert_eq!(zoomed.scale, 16.0);
        assert_eq!((zoomed.x, zoomed.y), (10.0, -5.0));
        assert!(zoomed.width > 800.0, "zooming in may overflow the window");

        // Degenerate inputs are clamped, never NaN.
        let degenerate = compute_layout(800.0, 600.0, 0.0, 0.0, f64::NAN, 0.0, 0.0);
        assert!(degenerate.scale.is_finite() && degenerate.scale > 0.0);
        assert!(degenerate.x.is_finite() && degenerate.y.is_finite());
    }

    /// The PNG decode helper round-trips straight RGBA8.
    #[test]
    fn decode_png_rgba_roundtrip() {
        let png = test_png(3, 2, [1, 2, 3, 255]);
        let (rgba, w, h) = decode_png_rgba(&png.bytes).expect("decode");
        assert_eq!((w, h), (3, 2));
        assert_eq!(rgba.len(), 24, "3x2 RGBA8 = 24 bytes");
        assert_eq!(&rgba[0..4], &[1, 2, 3, 255]);
        assert_eq!(&rgba[20..24], &[1, 2, 3, 255]);
    }

    /// `compute_layout` stays finite at the zoom extremes.
    #[test]
    fn layout_is_finite_for_extreme_zoom() {
        let l = compute_layout(1000.0, 1000.0, 2.0, 2.0, MAX_ZOOM, 0.0, 0.0);
        assert!(l.width.is_finite() && l.height.is_finite());
    }
}
