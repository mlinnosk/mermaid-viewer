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
//! over a `std::sync::mpsc` channel carrying plain data (strings, SVG
//! documents, commands, acknowledgements), with the open/closed state kept
//! in an `AtomicBool`.
//!
//! Besides the GUI thread, every real controller also owns a raster worker
//! thread (`mermaid-viewer-raster`): the show path carries the SVG document
//! to the GUI thread *without rasterizing*; the GUI thread stages the tab
//! (a placeholder draws until the first raster lands) and hands a
//! [`RasterJob`] to the worker, which rasterizes it off the GUI thread and
//! reports the [`RasterOutcome`] back. The GUI thread applies the outcome
//! (commit or rollback, texture upload for the active tab) in a
//! non-blocking per-frame drain and sends the *deferred* `show_tab`
//! acknowledgement there — a `show_tab` call bounds the whole first-raster
//! path, but the GUI thread never stalls on rasterization.
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
//! Window input is split by path: the *controller* (tabbed, MCP) window
//! takes SVG documents — a non-SVG [`RenderedImage`] is rejected with
//! [`Error::View`] — and rasters them on the dedicated raster worker at
//! display-appropriate scales (a show's `initial_scale` is a floor on the
//! first raster's sharpness; the active tab's texture re-rasters at the
//! display scale as the user zooms — hysteresis, a settle debounce, and
//! the pixel cap, best effort with the live texture kept until the
//! re-raster lands), while the single-diagram CLI path
//! ([`run_blocking`]) keeps its eager raster-**PNG** behavior (a non-PNG
//! image is rejected with [`Error::View`] there).
//!
//! Controller textures are *on-demand* and at most one is live at any
//! instant (the one-live-texture invariant, the texture-memory bound
//! together with the per-raster pixel cap): only the active tab holds a
//! texture, a tab switch frees the outgoing tab's texture, and an active
//! tab whose slot is empty draws a centered-label placeholder until its
//! raster lands — a show's first raster, an activation raster requested
//! when the tab is activated (or a re-raster at a later scale), each
//! coalesced per tab to at most one in-flight job plus one latest pending.

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
    RaylibGuiContainers, RaylibHandle, RaylibTexture2D, RaylibThread, Rectangle, Texture2D,
    Vector2,
};

/// A deferred, one-shot acknowledgement queued by [`Window`] until the GUI
/// loop delivers it: the sender half of a `ShowTab`'s ack channel paired
/// with the outcome it carries. Factored out of [`Window::due_acks`] to keep
/// the tuple below clippy's complexity threshold.
type Ack = (Sender<Result<()>>, Result<()>);

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

/// Multiplicative headroom on the fit-derived display scale for every
/// raster target (the texture rasterizes slightly oversampled, so it stays
/// sharp at the fit scale; re-rasters track display-appropriate scales as
/// the user zooms).
const RASTER_HEADROOM: f64 = 1.25;

/// The maximum pixel count (`ceil(w·s) × ceil(h·s)`) a single rasterization
/// may produce: with at most one live texture per window, this is the
/// texture-memory bound (4096×4096 ≈ 64 MiB of RGBA8).
const MAX_RASTER_PIXELS: u64 = 16_777_216;

/// The re-raster hysteresis: the ratio between a tab's live slot scale and
/// the effective display scale `e` (`d = max(e/s, s/e)`) that a `Ready`
/// slot must deviate from — in either direction — before a re-raster is
/// worthwhile (a zoom within 1.5× of the slot's scale is served by
/// resizing the existing texture: no re-raster, no thrash).
const RASTER_HYSTERESIS: f64 = 1.5;

/// The re-raster settle time: after the last change of the effective
/// display scale `e` (a wheel notch, a window resize), `e` must stay
/// unchanged for this long before a re-raster is issued — one debounce
/// rule covers wheel bursts and resize drags identically; a pan leaves
/// `e` unchanged and never touches the settle clock.
const ZOOM_SETTLE: Duration = Duration::from_millis(200);

/// Window background (the clear color drawn outside the image rect).
pub const BACKGROUND: Color = Color::RAYWHITE;

/// Environment variable that selects the headless fake window backend.
const FAKE_WINDOW_ENV: &str = "MERMAID_VIEWER_FAKE_WINDOW";

/// True when the fake backend is requested via the environment.
fn fake_window_enabled() -> bool {
    std::env::var(FAKE_WINDOW_ENV).is_ok_and(|v| v == "1")
}

/// Rejects non-PNG window input on the single-diagram CLI path.
///
/// The CLI window displays raster images eagerly; SVG must be rendered to
/// PNG first (see [`crate::render::NativeRenderer`]). This is the CLI side
/// of the input split — the controller (tabbed) path is SVG-only
/// ([`ensure_svg`]).
fn ensure_png(image: &RenderedImage) -> Result<()> {
    if image.format != ImageFormat::Png {
        return Err(Error::View(
            "window input must be a PNG image; render the diagram to PNG first (SVG is rejected)"
                .to_string(),
        ));
    }
    Ok(())
}

/// Rejects non-SVG window input on the controller (tabbed) path.
///
/// The controller window carries SVG documents across its channel and
/// rasterizes them on the dedicated raster worker at display-appropriate
/// scales (see [`WindowController::show_tab`]); a PNG — or any other
/// format — is a typed [`Error::View`]. This is the controller side of the
/// input split — the single-diagram CLI path ([`run_blocking`]) is PNG-only
/// ([`ensure_png`]).
fn ensure_svg(image: &RenderedImage) -> Result<()> {
    if image.format != ImageFormat::Svg {
        return Err(Error::View(
            "controller window input must be an SVG image; the show path carries SVG and rasterizes on demand (PNG is rejected)"
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
// Raster-scale policy (pure: no raylib — testable under every feature pass)
// ---------------------------------------------------------------------------

/// The `ceil(w·s) × ceil(h·s)` pixel count a rasterization of an `w × h`
/// source at `scale` produces, with degenerate inputs mapping to the 1×1
/// minimum (rasterization clamps to at least 1 px per side).
fn raster_pixel_count(w: f64, h: f64, scale: f64) -> u128 {
    let pw = (w.max(1.0) * scale.max(0.0)).ceil();
    let ph = (h.max(1.0) * scale.max(0.0)).ceil();
    // Cap each side before the product so the u128 multiply can never
    // overflow (a side of more than 2^63 px is already astronomically past
    // [`MAX_RASTER_PIXELS`]).
    let cap = 2.0f64.powi(63);
    let (pw, ph) = (pw.min(cap) as u128, ph.min(cap) as u128);
    pw * ph
}

/// Clamps `scale` down so the output pixel count of an `w × h` source
/// stays ≤ [`MAX_RASTER_PIXELS`] (a no-op when the count already fits).
fn clamp_raster_scale(w: f64, h: f64, scale: f64) -> f64 {
    if scale.is_finite()
        && scale > 0.0
        && raster_pixel_count(w, h, scale) <= MAX_RASTER_PIXELS as u128
    {
        return scale;
    }
    // Bisect to the largest in-cap scale: `lo` always satisfies the cap,
    // `hi` never does, and 64 halvings drive the interval below double
    // precision.
    let mut lo = 0.0f64;
    let mut hi = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    for _ in 0..64 {
        let mid = (lo + hi) / 2.0;
        if raster_pixel_count(w, h, mid) <= MAX_RASTER_PIXELS as u128 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// The effective display scale `e` (logical SVG px → window px) of an
/// `svg_w × svg_h` source fitted into a `content_w × content_h` rect at
/// `zoom` — the fit times the zoom, recomputed per frame. A non-finite or
/// non-positive `zoom` is clamped to `1.0` (the same clamp
/// [`compute_layout`] applies).
fn display_scale(content_w: f64, content_h: f64, svg_w: f64, svg_h: f64, zoom: f64) -> f64 {
    let svg_w = svg_w.max(1.0);
    let svg_h = svg_h.max(1.0);
    let zoom = if zoom.is_finite() && zoom > 0.0 {
        zoom
    } else {
        1.0
    };
    (content_w / svg_w).min(content_h / svg_h) * zoom
}

/// The steady-state raster target for a tab: `e × RASTER_HEADROOM`,
/// clamped so the output pixel count stays ≤ [`MAX_RASTER_PIXELS`].
fn raster_target(content_w: f64, content_h: f64, svg_w: f64, svg_h: f64, zoom: f64) -> f64 {
    clamp_raster_scale(
        svg_w,
        svg_h,
        display_scale(content_w, content_h, svg_w, svg_h, zoom) * RASTER_HEADROOM,
    )
}

/// The show's initial raster target: `max(hint, raster_target)` — the
/// hint is a floor on the first raster's sharpness — clamped to
/// [`MAX_RASTER_PIXELS`]. A non-finite or non-positive hint is ignored
/// (never an error).
fn raster_show_target(
    content_w: f64,
    content_h: f64,
    svg_w: f64,
    svg_h: f64,
    zoom: f64,
    hint: f64,
) -> f64 {
    let base = raster_target(content_w, content_h, svg_w, svg_h, zoom);
    let hint = if hint.is_finite() && hint > 0.0 {
        hint
    } else {
        base
    };
    clamp_raster_scale(svg_w, svg_h, hint.max(base))
}

/// One tab's re-raster settle clock (pure: fake-time testable): the last
/// effective display scale observed and the time at which it last
/// changed.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct SettleClock {
    /// The last effective display scale `e` observed (the fit of the
    /// tab's logical dimensions into the content rect at its zoom).
    last_e: Option<f64>,
    /// The time (monotonic seconds) at which `last_e` last changed.
    changed_at: Option<f64>,
}

impl SettleClock {
    /// Observes `e` at time `now` (monotonic seconds): resets the clock
    /// when `e` changed (a wheel notch, a resize) and returns how long
    /// the current `e` has been unchanged (0.0 for a fresh observation).
    /// A pan leaves `e` unchanged — it is invisible to the clock.
    fn observe(&mut self, e: f64, now: f64) -> f64 {
        if self.last_e != Some(e) {
            self.last_e = Some(e);
            self.changed_at = Some(now);
            0.0
        } else {
            (now - self.changed_at.unwrap_or(now)).max(0.0)
        }
    }
}

/// Whether two scales rasterize an `w × h` source to exactly the same
/// output pixel dimensions: a re-raster at an identical output size is a
/// no-op. This is the semantic form of the no-duplicate guard — robust at
/// the [`MAX_RASTER_PIXELS`] clamp, where two "equal" clamped scales can
/// differ by an ulp yet produce the same raster.
fn same_raster_size(w: f64, h: f64, a: f64, b: f64) -> bool {
    let w = w.max(1.0);
    let h = h.max(1.0);
    (w * a.max(0.0)).ceil() == (w * b.max(0.0)).ceil()
        && (h * a.max(0.0)).ceil() == (h * b.max(0.0)).ceil()
}

/// The per-frame re-raster decision (pure — the M-3 trigger core):
/// `Some(target)` exactly when an active tab's `Ready` slot should be
/// re-rasterized —
///
/// - the display scale `e` deviates from the slot's scale by more than
///   [`RASTER_HYSTERESIS`] in either direction
///   (`d = max(e/s, s/e) > RASTER_HYSTERESIS`);
/// - `e` has been unchanged for [`ZOOM_SETTLE`] (`settled_for`, from the
///   settle clock the per-frame evaluation observes it through);
/// - the clamped target (`e × RASTER_HEADROOM`, capped to
///   [`MAX_RASTER_PIXELS`]) produces a raster of a *different* size than
///   the slot's scale and any in-flight job's scale (the no-duplicate
///   guard, including at the cap: a target that clamps to the slot's own
///   scale is a no-op — the re-raster-loop guard).
///
/// A non-finite or non-positive `e` (a degenerate content rect) is
/// `None`: there is nothing to display at that scale.
fn reraster_decision(
    e: f64,
    settled_for: f64,
    slot_scale: f64,
    inflight_scale: Option<f64>,
    svg_w: f64,
    svg_h: f64,
) -> Option<f64> {
    if !e.is_finite() || e <= f64::EPSILON {
        return None;
    }
    let slot_scale = slot_scale.max(f64::EPSILON);
    let d = (e / slot_scale).max(slot_scale / e);
    if d <= RASTER_HYSTERESIS {
        return None;
    }
    if settled_for < ZOOM_SETTLE.as_secs_f64() {
        return None;
    }
    let target = clamp_raster_scale(svg_w, svg_h, e * RASTER_HEADROOM);
    if same_raster_size(svg_w, svg_h, target, slot_scale) {
        return None;
    }
    if inflight_scale.is_some_and(|s| same_raster_size(svg_w, svg_h, target, s)) {
        return None;
    }
    Some(target)
}

// ---------------------------------------------------------------------------
// Frame drawing + texture upload (shared by all window code paths)
// ---------------------------------------------------------------------------

/// Fits and draws `texture` into the content rect
/// `[(origin_x, origin_y) .. (origin_x + content_w, origin_y + content_h)]`
/// using the tab's `zoom`/`pan`. The fit is computed against the content
/// rect (so the image fills the area below the tab strip, not the whole
/// window) and the resulting rect is translated by `(origin_x, origin_y)`.
///
/// The fit runs against the *live texture's pixel dimensions*
/// (`img_w`/`img_h`): for the CLI's eager PNG that is the image's own
/// pixel size (the original rule, unchanged), and for the controller's
/// on-demand SVG raster it is the raster scale's output — which makes the
/// on-screen rect invariant under the raster scale (a re-raster never
/// moves or resizes the drawn diagram).
#[allow(clippy::too_many_arguments)]
fn draw_content(
    d: &mut impl RaylibDraw,
    texture: &Texture2D,
    origin_x: f64,
    origin_y: f64,
    content_w: f64,
    content_h: f64,
    img_w: f64,
    img_h: f64,
    zoom: f64,
    pan_x: f64,
    pan_y: f64,
) {
    let layout = compute_layout(content_w, content_h, img_w, img_h, zoom, pan_x, pan_y);
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
    // The CLI texture is the PNG's own raster: texture pixels are the
    // image's pixels, so the fit runs against them (the unchanged rule).
    draw_content(
        d,
        texture,
        0.0,
        0.0,
        win_w as f64,
        win_h as f64,
        image.width as f64,
        image.height as f64,
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
/// `jobs` collects the on-demand raster a tab-bar selection requests for
/// the incoming tab (through [`Window::activate_tab`]; headless frames
/// produce no bar input, so the tests get an empty vec from it): the
/// caller — the GUI loop in production — hands each job to the raster
/// worker.
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
    jobs: &mut Vec<RasterJob>,
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

    // A selection (no close) re-targets the active tab: the outgoing tab's
    // texture is freed (only the active tab ever holds a live texture — the
    // one-live-texture invariant), and the incoming tab — when its slot is
    // `Empty` and no in-flight raster covers it — requests its on-demand
    // raster (surfaced through `jobs`; the tab draws the placeholder until
    // the outcome is applied). Wheel zoom and drag pan then apply to the
    // selected tab on the next frame, with each tab's zoom/pan persisting
    // independently in its own `TabState`.
    if close_idx < 0 && selected as usize != window.state.active_index() {
        *jobs = window.activate_tab(
            (selected as usize).min(window.state.len().saturating_sub(1)),
            content_w,
            content_h,
        );
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

    // Draw the active tab in the content area below the strip, through its
    // texture slot (the fit runs against the live texture's pixel
    // dimensions, keeping the on-screen rect invariant under the raster
    // scale).
    let active = window.state.active_index();
    let tab = &window.state.tabs[active];
    match &window.slots[active] {
        TextureSlot::Ready { texture, .. } => draw_content(
            d,
            texture,
            0.0,
            strip_h,
            content_w,
            content_h,
            texture.width() as f64,
            texture.height() as f64,
            tab.zoom,
            tab.pan_x,
            tab.pan_y,
        ),
        TextureSlot::Empty => draw_placeholder(d, tab, strip_h, content_w, content_h),
    }
    true
}

/// The active tab's minimal placeholder: the tab's label, centered in the
/// content area (the background clear already happened). Shown while the
/// tab has no live texture: the window's first show before its raster
/// outcome lands (the show path stages the tab and draws this while the
/// worker rasterizes — the GUI thread never stalls on rasterization), a
/// tab that lost its texture on a switch, and any tab whose raster cannot
/// be served.
fn draw_placeholder(
    d: &mut impl RaylibDraw,
    tab: &TabState,
    strip_h: f64,
    content_w: f64,
    content_h: f64,
) {
    const FONT_SIZE: i32 = 20;
    // `draw_text` wraps the label in a C string and panics on an interior
    // NUL: strip NULs so a hostile label can never panic the draw path.
    let label: String = tab.label.chars().filter(|c| *c != '\0').collect();
    // The default font is 10 px per glyph at base size 10, so at
    // `FONT_SIZE` each glyph advances ≈ `FONT_SIZE` px — close enough to
    // center a placeholder.
    let est_width = label.chars().count() as f64 * FONT_SIZE as f64;
    let x = ((content_w - est_width) / 2.0).max(0.0) as i32;
    let y = (strip_h + (content_h - FONT_SIZE as f64) / 2.0).max(strip_h) as i32;
    d.draw_text(&label, x, y, FONT_SIZE, Color::DARKGRAY);
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

/// One staged show awaiting its raster outcome: the show's content and
/// label (applied at commit), the deferred [`WindowController::show_tab`]
/// acknowledgement, the target scale the show's [`RasterJob`] was issued at
/// (recorded with the texture when the outcome lands), and whether the tab
/// was *provisionally added* by this show (a failed raster removes it
/// again).
struct StagedShow {
    /// The label the commit installs.
    label: String,
    /// The content the commit installs.
    image: RenderedImage,
    /// The staged show's acknowledgement (`Ok` on commit, a typed `Err` on
    /// rollback/supersede/cancellation).
    ack: Sender<std::result::Result<(), Error>>,
    /// The target scale the staged show's raster job was issued at.
    scale: f64,
    /// The tab was provisionally added by this show: a failed raster
    /// removes the tab again (a replace keeps the old content live and
    /// simply clears this).
    is_new: bool,
}

/// The cause a staged show is cancelled for out of band. The show's
/// acknowledgement is a typed [`Error::View`] naming the *actual* cause —
/// worker loss and window close are distinct reasons and must never be
/// misreported. (A latched `Close`/tab removal cancels through
/// [`Window::close_tab`] with the tab-close wording instead.)
#[derive(Clone, Copy)]
enum StagedShowCancelReason {
    /// The raster worker is gone (its result channel disconnected, or a job
    /// it can no longer take): no outcome can ever arrive for the show.
    WorkerGone,
    /// The window is going down (an ESC/OS close): the in-flight show goes
    /// with it.
    WindowClosed,
}

/// The typed [`Error::View`] a staged show of tab `id` is acked with when
/// cancelled for `reason`, naming the actual cause.
fn staged_cancel_error(reason: StagedShowCancelReason, id: &str) -> Error {
    match reason {
        StagedShowCancelReason::WorkerGone => Error::View(format!(
            "the raster worker is no longer running; the staged show for tab \"{}\" cannot complete",
            id
        )),
        StagedShowCancelReason::WindowClosed => Error::View(format!(
            "the staged show for tab \"{}\" was cancelled because the window closed",
            id
        )),
    }
}

/// A diagram tab's state: its stable ID, display label, current image, the
/// tab's independent zoom/pan, and — for the deferred-ack show path — the
/// tab's generation counter plus its staged show (if any). Not `Debug`/
/// `Clone`: a staged show carries an `mpsc` acknowledgement sender.
struct TabState {
    /// Stable identifier the tab is addressed by.
    id: String,
    /// The tab's display label.
    label: String,
    /// Current content (kept for the drawn dimensions). While a replace
    /// show is staged, this is the *old* content: the staged show's content
    /// lands at commit.
    image: RenderedImage,
    /// Zoom multiplier on top of the fit scale (`1.0` = exact fit).
    zoom: f64,
    /// User pan offset in window pixels.
    pan_x: f64,
    /// User pan offset in window pixels.
    pan_y: f64,
    /// The tab's generation: assigned by every staged show of this tab from
    /// the window's counter (`Window::next_generation`), which the GUI loop
    /// seeds from a watermark monotonic over the GUI thread's whole life,
    /// so generations are unique across tab recreations *and* window
    /// re-opens — a re-created tab (same ID after a close/rollback, in the
    /// same window or a later window cycle) never re-enters a generation an
    /// old [`RasterOutcome`] can still carry. A [`RasterOutcome`] commits
    /// only when its generation matches — the staleness discriminator.
    generation: u64,
    /// The staged show (if any) for the current `generation`, awaiting its
    /// raster outcome.
    staged: Option<StagedShow>,
}

/// The tabbed window's pure state: tabs in insertion order plus the active
/// index. The window is open exactly while this holds at least one tab.
///
/// Pure (no raylib): the ID-addressed add/replace/close rules and the
/// active-index adjustment are testable without a window.
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
                    // The CLI's eager path commits at insertion: the tab's
                    // generation starts at 1 with no staged show (there is
                    // no deferred show path on this side).
                    generation: 1,
                    staged: None,
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

/// The active tab's texture state in the window.
///
/// Only the active tab is ever drawn, so only it holds a live texture:
/// at most one [`TextureSlot::Ready`] per window at any instant (the
/// one-live-texture invariant — the texture-memory bound, together with
/// [`MAX_RASTER_PIXELS`]).
enum TextureSlot {
    /// No live texture: the tab was shown but is not active, or its on-
    /// demand raster has not landed. The active tab draws the placeholder
    /// while its slot is `Empty`.
    Empty,
    /// A live texture rasterized at `scale` (logical SVG px → raster px).
    /// The scale is recorded with the texture: the show path's
    /// [`Self::apply_raster_result`] writes it when the show's raster
    /// outcome lands for an active tab, and the activation-raster path
    /// (a tab that lost its texture on a switch) and the later re-raster
    /// policy compare a live slot's scale against the display target.
    Ready { texture: Texture2D, scale: f64 },
}

/// One tab's on-demand raster schedule (pure: no raylib — testable under
/// every feature pass): the coalescing bound, *at most one in-flight job
/// and one latest pending request per tab*.
///
/// A raster request (a show's initial job, an activation raster, or a
/// zoom/resize-driven re-raster) that arrives while one of the tab's jobs
/// is already in flight becomes the *pending* — a newer request replaces
/// any older pending (it is a latest request, never a queue) — and the
/// pending *fires* (becomes the in-flight job, to be sent to the worker
/// now) exactly when the in-flight job's outcome is applied. Each pending
/// fires at most once: an outcome the schedule no longer carries as
/// in-flight (a stale or duplicate delivery) leaves the schedule
/// untouched.
///
/// The *in-flight* job may live on after the tab is no longer active (its
/// outcome may land and be discarded while the tab is inactive); only the
/// *pending* is dropped when the tab loses the active
/// ([`Window::activate_tab`]).
///
/// The schedule also carries the re-raster policy state (M-3): the
/// settle clock (reset whenever the tab's effective display scale changes
/// — wheel or resize; pan never touches it) and the generation a failed
/// re-raster latched (one log, no retry, within the same content
/// generation — a new show bumps the generation and resets the scope).
#[derive(Debug, Default)]
struct RasterSlot {
    /// The job the raster worker is processing (sent, outcome not yet
    /// applied).
    inflight: Option<RasterJob>,
    /// The latest request that could not be issued while `inflight` was
    /// occupied (fires when the in-flight outcome is applied).
    pending: Option<RasterJob>,
    /// The re-raster settle clock (M-3): the last effective display scale
    /// observed by the per-frame evaluation and when it last changed.
    settle: SettleClock,
    /// The generation a failed re-raster latched (the M-3 no-retry rule):
    /// while `Some(g) == the tab's generation`, no re-raster is re-issued
    /// for the tab and its failure is not re-logged — a new show bumps
    /// the generation and the latch stops matching.
    failed_reraster_gen: Option<u64>,
}

impl RasterSlot {
    /// Requests a raster for this tab: issued immediately (it becomes the
    /// in-flight job and is returned for the worker) when nothing is in
    /// flight; otherwise it stays the latest pending (replacing any older
    /// one) and the return value is empty.
    fn request(&mut self, job: RasterJob) -> Vec<RasterJob> {
        if self.inflight.is_none() {
            self.inflight = Some(job.clone());
            vec![job]
        } else {
            self.pending = Some(job);
            Vec::new()
        }
    }

    /// The in-flight outcome has been applied (committed, rolled back, or
    /// discarded as stale): clears it and — when a pending request is
    /// waiting — fires it: the pending becomes the new in-flight job,
    /// returned for the worker. At most one pending ever fires; with no
    /// pending, a landing fires nothing. (A fired *re-raster* is still
    /// re-warranted by [`Window::rewarrant_fired`] against the landing
    /// live state before it is handed to the worker.)
    fn complete(&mut self) -> Vec<RasterJob> {
        self.inflight = None;
        match self.pending.take() {
            Some(job) => {
                self.inflight = Some(job.clone());
                vec![job]
            }
            None => Vec::new(),
        }
    }

    /// The tab lost the active: drop the pending request (no point
    /// rasterizing a tab that is not active) — the in-flight job is kept:
    /// its outcome may still land, and it is discarded there while the tab
    /// is inactive.
    fn drop_pending(&mut self) {
        self.pending = None;
    }

    /// A new show's generation bump discards the tab's in-flight and
    /// pending *re-rasters* (the M-3 rule): the show's own raster is the
    /// authoritative raster for the incoming content. An in-flight
    /// re-raster is only removed from the schedule (its outcome lands and
    /// is discarded by the generation rule there — the schedule no longer
    /// carries it as in-flight); a pending re-raster is dropped outright.
    /// An in-flight *show* job (a superseded show) is kept: its landing
    /// must still complete the schedule and fire the newer show's job
    /// through the normal path.
    fn discard_rerasters(&mut self) {
        if self
            .inflight
            .as_ref()
            .is_some_and(|j| j.kind == RasterJobKind::Reraster)
        {
            self.inflight = None;
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|j| j.kind == RasterJobKind::Reraster)
        {
            self.pending = None;
        }
    }

    /// Records a job's failure at `generation`: a re-raster failure
    /// latches the generation (the M-3 no-retry rule) and gates its log to
    /// once per generation; a non-re-raster failure records nothing (its
    /// log is unconditional, and at most one per generation by the
    /// schedule's bound — an activation's demand re-asserts on the tab's
    /// next activation). Returns whether the failure is log-worthy (the
    /// first at this generation).
    fn record_failure(&mut self, generation: u64, kind: RasterJobKind) -> bool {
        if kind == RasterJobKind::Reraster {
            if self.failed_reraster_gen == Some(generation) {
                return false;
            }
            self.failed_reraster_gen = Some(generation);
        }
        true
    }
}

/// The open window: its tabbed state, one texture slot per tab, and the
/// queued acknowledgements for staged shows (commit, typed failure,
/// supersede, cancellation — sent by the GUI loop, not the window).
///
/// Lives only on the thread that owns the raylib handle (it is not `Send`).
/// Exposed so the headless pixel tests (under `view-headless`) can build a
/// window of tabs and drive the exact production tabbed frame through
/// [`draw_tabbed_frame`] and the show path through [`Self::show_tab`] /
/// [`Self::apply_raster_result`]; the MCP-facing API is
/// [`WindowController`], which owns a `Window` on its dedicated GUI thread.
pub struct Window {
    /// The pure tabbed state (IDs, labels, images, zoom/pan, generations,
    /// staged shows, active index).
    state: TabbedState,
    /// One texture slot per tab, parallel to `state.tabs`.
    slots: Vec<TextureSlot>,
    /// One on-demand raster schedule per tab, parallel to `state.tabs`
    /// and `slots`: the coalescing bound (at most one in-flight job plus
    /// one latest pending request per tab; a pending fires at most once).
    schedules: Vec<RasterSlot>,
    /// Acknowledgements due to their waiting [`WindowController::show_tab`]
    /// callers: `(ack channel, the result to send)`, queued by the
    /// staging/commit/close/abort paths and drained by the GUI loop with
    /// [`Self::take_due_acks`].
    due_acks: Vec<Ack>,
    /// The next generation to hand to a staged show: monotonic over the
    /// *GUI thread's* whole life. The window is seeded from the GUI loop's
    /// watermark at construction (the raster worker and its result channel
    /// outlive window open/close cycles, so a counter scoped to one window
    /// would re-issue generations the worker can still deliver) and the GUI
    /// loop carries this value into every re-opened window. Every staged
    /// show — a re-created tab's first show included, across cycles — thus
    /// gets a generation no stale [`RasterOutcome`] can match.
    next_generation: u64,
}

impl Window {
    /// An empty window (no tabs): created right after raylib init and
    /// filled by the first `show_tab`.
    ///
    /// `next_generation` seeds the window's generation counter. The GUI
    /// loop passes the watermark it carries across window open/close cycles
    /// (the raster worker and its result channel outlive the window, so a
    /// queued outcome from a prior cycle must never match a show in this
    /// one — its generation was already issued and can never be re-issued);
    /// the windowless paths (the CLI, the headless seams) pass `0` (no
    /// worker, no prior cycle).
    pub fn new(next_generation: u64) -> Self {
        Self {
            state: TabbedState::new(),
            slots: Vec::new(),
            schedules: Vec::new(),
            due_acks: Vec::new(),
            next_generation,
        }
    }

    /// The generation watermark to carry into a re-opened window: the next
    /// generation this window will hand out. [`gui_loop`] records it before
    /// the window is dropped and seeds the next window with it, keeping
    /// generations monotonic for the GUI thread's whole lifetime — the
    /// raster worker and its result channel outlive window open/close
    /// cycles, so a generation issued in one cycle must never be re-issued
    /// in a later one (an old queued outcome for the same tab ID would
    /// match and commit stale content).
    ///
    /// `#[doc(hidden)]` seam: the headless pixel tests probe the window
    /// cycle the GUI loop drives.
    #[doc(hidden)]
    pub fn generation_watermark(&self) -> u64 {
        self.next_generation
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

    /// Stages the show: the GUI-thread half of the show path, with no
    /// rasterization and no texture upload.
    ///
    /// The ordering is *capacity check first*, then:
    ///
    /// - a **new** ID is *provisionally* added (label + SVG visible, slot
    ///   [`TextureSlot::Empty`]) and made active — the tab draws the
    ///   placeholder until its first raster outcome lands;
    /// - a **known** ID is *kept live until commit*: the old content,
    ///   label, zoom/pan, and texture slot all stay as they are; the
    ///   staged show's content and label land at commit
    ///   ([`Self::apply_raster_result`]), and a failed raster simply
    ///   leaves the old content in place.
    ///
    /// Staging assigns the tab a fresh generation (from the window's
    /// counter, which the GUI loop seeds from a watermark monotonic across
    /// window open/close cycles — unique across tab recreations *and*
    /// window re-opens, so a re-shown ID never re-matches an old outcome,
    /// even one the closed window's job still queued), supersedes any
    /// in-flight staged
    /// show of the same tab (its acknowledgement is queued as a typed
    /// [`Error::View`] naming the supersede), frees the previously active
    /// tab's texture slot and pending raster request (a show activates its
    /// tab — the one-live-texture invariant), and routes the show's
    /// [`RasterJob`] (at the show's initial target) through the tab's
    /// [`RasterSlot`]: it is returned for the caller to hand to the raster
    /// worker when nothing for this tab is in flight, and stays the latest
    /// pending — firing when the in-flight outcome is applied through
    /// [`Self::apply_raster_result`] — while one is.
    ///
    /// The image must be SVG (a non-SVG [`RenderedImage`] — PNG included —
    /// is a typed [`Error::View`], and non-UTF-8 bytes are a typed
    /// [`Error::Rasterize`]), so a staging failure leaves the window
    /// untouched. `initial_scale` is the show's raster scale hint: a floor
    /// on the first raster's sharpness (the target is
    /// `max(hint, fit × RASTER_HEADROOM)` at the content rect — the window
    /// minus the tab strip — clamped to [`MAX_RASTER_PIXELS`]); a
    /// non-finite or non-positive hint is ignored, never an error.
    ///
    /// The `ack` is the show's deferred acknowledgement: on success it is
    /// stored with the staged show and sent by the GUI loop when the
    /// outcome is applied (or the show is superseded/cancelled). A
    /// *staging* failure (non-SVG, non-UTF-8, capacity) sends the typed
    /// error on the acknowledgement itself, immediately: it is the show's
    /// only acknowledgement, and the window is left untouched.
    ///
    /// Public (with the struct) so the headless pixel tests build a window
    /// of tabs; the GUI loop reaches the same routine for every
    /// `ShowTab` command.
    pub fn show_tab(
        &mut self,
        rl: &RaylibHandle,
        id: &str,
        label: &str,
        image: &RenderedImage,
        initial_scale: f64,
        ack: Sender<std::result::Result<(), Error>>,
    ) -> Result<Vec<RasterJob>> {
        // A staging failure is decided here, off the worker: the show is
        // acked with the typed failure right now (its only ack). `Error`
        // is not `Clone`, so the acknowledgement gets a rebuilt copy of
        // the string payload (staging fails only as `View`/`Rasterize`).
        let ack_and = |ack: &Sender<std::result::Result<(), Error>>, e: &Error| {
            match e {
                Error::View(m) => {
                    let _ = ack.send(Err(Error::View(m.clone())));
                }
                Error::Rasterize(m) => {
                    let _ = ack.send(Err(Error::Rasterize(m.clone())));
                }
                other => {
                    // Defensive: unreachable on this path — keep the show
                    // acked rather than silent if it ever is.
                    let _ = ack.send(Err(Error::View(other.to_string())));
                }
            }
        };
        if let Err(e) = ensure_svg(image) {
            ack_and(&ack, &e);
            return Err(e);
        }
        // The job carries the SVG as text; reject non-UTF-8 bytes up front
        // with the same typed rasterization error the worker would report.
        let svg = match String::from_utf8(image.bytes.clone()) {
            Ok(svg) => svg,
            Err(_) => {
                let e = Error::Rasterize("SVG document is not valid UTF-8 text".to_string());
                ack_and(&ack, &e);
                return Err(e);
            }
        };
        let is_new = self.state.index_of(id).is_none();
        if is_new && self.state.len() >= WindowController::MAX_TABS {
            let e = tab_capacity_error();
            ack_and(&ack, &e);
            return Err(e);
        }
        // The show's fit-aware target: the tab commits at its reset zoom
        // (1.0), so the display scale is the fit of its logical SVG size
        // into the content rect (window minus tab strip). The policy takes
        // the content rect *first* and the SVG's logical size *second*:
        // the fit floor runs over the content rect / SVG size, and
        // `MAX_RASTER_PIXELS` must constrain the SVG's raster, never the
        // window dimensions.
        let content_w = rl.get_screen_width() as f64;
        let content_h = (rl.get_screen_height() as f64 - TAB_STRIP_HEIGHT as f64).max(0.0);
        let scale = raster_show_target(
            content_w,
            content_h,
            image.width as f64,
            image.height as f64,
            1.0,
            initial_scale,
        );
        let index = if is_new {
            // Provisional add: the tab is live (label + placeholder) while
            // its first raster is in flight; a failed raster removes it.
            self.state.tabs.push(TabState {
                id: id.to_string(),
                label: label.to_string(),
                image: image.clone(),
                zoom: 1.0,
                pan_x: 0.0,
                pan_y: 0.0,
                // Assigned from the window's counter just below (0 = not
                // staged yet).
                generation: 0,
                staged: None,
            });
            self.slots.push(TextureSlot::Empty);
            self.schedules.push(RasterSlot::default());
            self.state.tabs.len() - 1
        } else {
            // Replace: the old content stays live until commit — nothing is
            // swapped out here.
            self.state.index_of(id).expect("shown as existing")
        };
        // A show activates its tab: free the previously active tab's live
        // texture and drop its pending raster request *before* the active
        // index moves (the one-live-texture invariant; the freed tab's
        // in-flight job, if any, still commits its content when it lands,
        // just without a texture upload).
        if self.state.active != index {
            self.slots[self.state.active] = TextureSlot::Empty;
            self.schedules[self.state.active].drop_pending();
        }
        self.state.active = index;
        let tab = &mut self.state.tabs[index];
        // A newer show supersedes this tab's in-flight staged show: its
        // acknowledgement is queued as a typed error naming the supersede,
        // and its job, when it lands, is discarded by the generation rule.
        if let Some(prev) = tab.staged.take() {
            self.due_acks.push((
                prev.ack,
                Err(Error::View(format!(
                    "the staged show for tab \"{}\" was superseded by a newer show of the same tab",
                    id
                ))),
            ));
        }
        // Every staged show — a re-created tab's first show included — gets
        // a generation from the window's counter (seeded from the GUI
        // loop's watermark, monotonic over the GUI thread's whole life):
        // unique across tab recreations and window re-opens, so an old
        // outcome for a removed tab — even one a prior window cycle's job
        // still queued on the surviving worker channel — can never match a
        // later show of the same ID.
        self.next_generation += 1;
        tab.generation = self.next_generation;
        tab.staged = Some(StagedShow {
            label: label.to_string(),
            image: image.clone(),
            ack,
            scale,
            is_new,
        });
        // The show's raster request goes through the tab's schedule (the
        // coalescing bound): issued now when nothing for this tab is in
        // flight, else it stays the latest pending and fires when the
        // in-flight outcome is applied. The new show's generation bump
        // first discards the tab's in-flight and pending *re-rasters*
        // (the M-3 rule: a generation bump from a new show discards any
        // in-flight or pending re-raster — the show's own raster is the
        // authoritative raster for the incoming content).
        self.schedules[index].discard_rerasters();
        let job = RasterJob {
            id: id.to_string(),
            generation: tab.generation,
            svg,
            scale,
            kind: RasterJobKind::Show,
        };
        Ok(self.schedules[index].request(job))
    }

    /// Applies one raster outcome through the generation and schedule
    /// rules, delivering through the queued acknowledgements (the GUI loop
    /// sends what [`Self::take_due_acks`] then yields) and returning the
    /// [`RasterJob`]s the outcome *fired* from the tab's schedule (the
    /// latest pending, at most one): the caller hands them to the raster
    /// worker.
    ///
    /// - The tab is **gone** (closed while the raster was in flight): the
    ///   outcome is discarded without effect — the staged show was already
    ///   acked as cancelled at close.
    /// - The outcome is **not the tab's in-flight job** (a stale or
    ///   duplicate delivery the schedule no longer carries): it is
    ///   discarded without touching the schedule — the superseded staged
    ///   show, if any, was already acked at supersede.
    /// - A **failed show raster** (the outcome is the tab's current
    ///   generation and a staged show is waiting) rolls back: a
    ///   provisionally *added* tab is removed again; a *replaced* tab
    ///   keeps its old content live (it was never swapped out at staging)
    ///   — and the acknowledgement is queued with the typed failure.
    /// - A **successful show raster** commits the staged content, label,
    ///   and zoom/pan reset, and — only when the tab is *active* —
    ///   uploads the texture into its slot ([`TextureSlot::Ready`]; an
    ///   inactive commit keeps the slot `Empty`, and a failed upload keeps
    ///   the placeholder logged to stderr) — and the acknowledgement is
    ///   queued as `Ok`.
    /// - An outcome for a **committed tab** (no staged show: an
    ///   activation raster) uploads the texture when the tab is *active*
    ///   (at the in-flight job's scale); when the tab is not active — or
    ///   the outcome is stale — it is discarded without effect: the tab
    ///   re-requests its raster on its next activation. No acknowledgement
    ///   is involved (a re-raster is not a show).
    ///
    /// The tab's in-flight slot is cleared for whatever it was, and the
    /// latest pending request (if any) fires — the coalescing bound. At
    /// most one [`TextureSlot::Ready`] exists per window when this runs:
    /// an upload only happens for the active tab, and every activation
    /// (show or tab-bar switch) freed the outgoing slot.
    pub fn apply_raster_result(
        &mut self,
        rl: &mut RaylibHandle,
        thread: &RaylibThread,
        id: &str,
        generation: u64,
        result: Result<(Vec<u8>, u32, u32)>,
    ) -> Vec<RasterJob> {
        let Some(pos) = self.state.index_of(id) else {
            // The tab is gone: its staged show was acked at close.
            return Vec::new();
        };
        // The outcome must be this tab's in-flight job: anything else (a
        // stale delivery for a generation the schedule has moved past, a
        // duplicate) is a stray — discarded without touching the
        // schedule, so a pending behind the real in-flight job is not
        // fired early. The in-flight job's scale rides with the upload of
        // an activation raster.
        let in_flight_scale = {
            let schedule = &self.schedules[pos];
            match &schedule.inflight {
                Some(job) if job.generation == generation => job.scale,
                _ => return Vec::new(),
            }
        };
        // A show commits exactly when the outcome is the tab's current
        // generation (the staged show's generation, set together with it
        // at staging); otherwise the outcome is an activation raster (or
        // a stale show outcome) and no staged show is touched.
        let matching = self.state.tabs[pos].generation == generation;
        let is_active = self.state.active == pos;
        match result {
            Err(e) => {
                if matching {
                    let Some(staged) = self.state.tabs[pos].staged.take() else {
                        // A committed tab's (activation or re-raster)
                        // raster failed: best effort — the tab keeps what
                        // it has (the previous texture when `Ready`, the
                        // placeholder when `Empty`). A re-raster failure
                        // latches the generation: the log goes out once
                        // per generation and no re-raster retries within
                        // the same content generation (a new show bumps
                        // it); an activation's demand re-asserts on the
                        // tab's next activation.
                        let kind = self.schedules[pos]
                            .inflight
                            .as_ref()
                            .map(|j| j.kind)
                            .unwrap_or(RasterJobKind::Activation);
                        if self.schedules[pos].record_failure(generation, kind) {
                            eprintln!(
                                "mermaid-viewer: the tab's on-demand raster failed for tab \"{}\": {e}",
                                id
                            );
                        }
                        if kind == RasterJobKind::Reraster {
                            // No retry in the same content generation:
                            // the failure latch (above) suppresses new
                            // re-raster evaluations, and the request
                            // state is cleared — a pending re-raster is
                            // dropped with the failed one. The tab keeps
                            // what it has until the next show.
                            self.schedules[pos].inflight = None;
                            self.schedules[pos].pending = None;
                            return Vec::new();
                        }
                        let fired = self.schedules[pos].complete();
                        return self.rewarrant_fired(rl, pos, fired);
                    };
                    // Rollback: a provisional add is removed; a replace
                    // keeps the old content live (content, label,
                    // zoom/pan, and texture were never touched at
                    // staging).
                    if staged.is_new {
                        self.schedules.remove(pos);
                        self.slots.remove(pos);
                        self.state.close(id);
                    }
                    self.due_acks.push((staged.ack, Err(e)));
                } else {
                    // A stale show outcome (a newer show superseded it):
                    // discarded without effect — the superseded staged
                    // show was acked at supersede.
                }
            }
            Ok((rgba, w, h)) => {
                if matching {
                    if let Some(staged) = self.state.tabs[pos].staged.take() {
                        // A show's raster succeeded: commit the staged
                        // content, label, and zoom/pan reset…
                        let tab = &mut self.state.tabs[pos];
                        tab.image = staged.image;
                        tab.label = staged.label;
                        tab.zoom = 1.0;
                        tab.pan_x = 0.0;
                        tab.pan_y = 0.0;
                        if is_active {
                            // …and upload the texture into the slot —
                            // only for the active tab (an inactive commit
                            // stays `Empty`: the placeholder covers it
                            // until an activation raster lands).
                            let raw_image = image_from_rgba(rgba, w as i32, h as i32);
                            match rl.load_texture_from_image(thread, &raw_image) {
                                Ok(texture) => {
                                    self.slots[pos] = TextureSlot::Ready {
                                        texture,
                                        scale: staged.scale,
                                    };
                                }
                                Err(e) => eprintln!(
                                    "mermaid-viewer: failed to upload the tab's texture: {e}; the placeholder stays"
                                ),
                            }
                        }
                        self.due_acks.push((staged.ack, Ok(())));
                    } else if is_active {
                        // A committed tab's raster (activation or
                        // re-raster): upload it (at the in-flight job's
                        // scale — the on-screen rect stays fixed, the
                        // fit tracks the live pixel size). An inactive
                        // tab's outcome is discarded: it re-requests its
                        // raster on its next activation.
                        let raw_image = image_from_rgba(rgba, w as i32, h as i32);
                        match rl.load_texture_from_image(thread, &raw_image) {
                            Ok(texture) => {
                                self.slots[pos] = TextureSlot::Ready {
                                    texture,
                                    scale: in_flight_scale,
                                };
                            }
                            Err(e) => eprintln!(
                                "mermaid-viewer: failed to upload the tab's texture: {e}; the placeholder stays"
                            ),
                        }
                    }
                }
                // A stale (or inactive) outcome is discarded above without
                // any state change.
            }
        }
        // The in-flight outcome has landed: fire the latest pending
        // request (at most once) — it becomes the new in-flight job, which
        // the caller sends to the worker. A rollback of a provisional
        // add removed the tab (and its schedule) above: nothing to fire.
        // Fired re-raster jobs are re-warranted against the live landing
        // state before being issued (M-3.a).
        if self.state.index_of(id) == Some(pos) {
            let fired = self.schedules[pos].complete();
            self.rewarrant_fired(rl, pos, fired)
        } else {
            Vec::new()
        }
    }

    /// Inserts a tab whose texture is already uploaded: the single-diagram
    /// CLI path's eager PNG upload ([`run_blocking`]); the controller path
    /// stages through [`Self::show_tab`] and commits through
    /// [`Self::apply_raster_result`].
    fn add_ready_tab(
        &mut self,
        id: &str,
        label: &str,
        image: &RenderedImage,
        texture: Texture2D,
    ) -> Result<()> {
        let index = self.state.show(id, label, image)?;
        self.slots.insert(
            index,
            TextureSlot::Ready {
                texture,
                scale: 1.0,
            },
        );
        // The CLI's eager tab never issues raster jobs: its schedule stays
        // empty (a tab removed elsewhere keeps the parallel vectors in
        // lock-step).
        self.schedules.insert(index, RasterSlot::default());
        Ok(())
    }

    /// Rasterizes a tab's SVG document at `scale` into straight RGBA8
    /// (the crate's shared resvg primitive, so the window's textures and
    /// the PNG pipeline rasterize identically): malformed SVG is
    /// [`Error::Rasterize`]; a non-finite or non-positive `scale` is
    /// clamped to 1.0 inside the primitive, never an error.
    ///
    /// `#[doc(hidden)]` seam: the headless pixel tests feed it to
    /// [`Self::apply_raster_result`] to simulate the raster worker's
    /// outcomes (the worker runs exactly this primitive off the GUI
    /// thread).
    #[doc(hidden)]
    pub fn rasterize_tab_svg(image: &RenderedImage, scale: f64) -> Result<(Vec<u8>, u32, u32)> {
        let svg = std::str::from_utf8(&image.bytes)
            .map_err(|_| Error::Rasterize("SVG document is not valid UTF-8 text".to_string()))?;
        crate::render::rasterize_rgba(svg, scale)
    }

    /// Removes the tab `id` and unloads its texture (its slot). The tab's
    /// staged show, if any, is cancelled: its acknowledgement is queued as
    /// a typed [`Error::View`] naming the cancellation (and a provisionally
    /// added tab vanishes — its show never committed). Returns whether a
    /// tab was removed (unknown IDs are a no-op); the caller closes the
    /// window exactly when `state` becomes empty (the last tab went).
    ///
    /// `#[doc(hidden)]` seam: the headless pixel tests drive it directly;
    /// the production triggers are the GUI loop's `CloseTab` command and
    /// the tab-bar close in [`draw_tabbed_frame`].
    #[doc(hidden)]
    pub fn close_tab(&mut self, id: &str) -> bool {
        let Some(pos) = self.state.index_of(id) else {
            return false;
        };
        if let Some(staged) = self.state.tabs[pos].staged.take() {
            self.due_acks.push((
                staged.ack,
                Err(Error::View(format!(
                    "the staged show for tab \"{}\" was cancelled because the tab was closed",
                    id
                ))),
            ));
        }
        self.slots.remove(pos);
        self.schedules.remove(pos);
        self.state.close(id)
    }

    /// Closes the window's tabs one by one through [`Self::close_tab`]
    /// (each staged show cancelled), leaving an empty state. The
    /// `Cmd::Close` path.
    fn close_all(&mut self) {
        while let Some(id) = self.state.tabs.first().map(|t| t.id.clone()) {
            self.close_tab(&id);
        }
    }

    /// Cancels the tab's staged show for `reason`: a provisionally added
    /// tab is removed, and the acknowledgement is queued as a typed
    /// [`Error::View`] naming the actual cause. A no-op when the tab is
    /// gone or has no staged show.
    fn abort_staged_show(&mut self, id: &str, reason: StagedShowCancelReason) {
        let Some(pos) = self.state.index_of(id) else {
            return;
        };
        let Some(staged) = self.state.tabs[pos].staged.take() else {
            return;
        };
        if staged.is_new {
            self.slots.remove(pos);
            self.schedules.remove(pos);
            self.state.close(id);
        }
        self.due_acks
            .push((staged.ack, Err(staged_cancel_error(reason, id))));
    }

    /// Cancels *every* staged show for `reason`: one typed [`Error::View`]
    /// acknowledgement per staged show naming the cause, provisional adds
    /// removed.
    fn abort_all_staged(&mut self, reason: StagedShowCancelReason) {
        let ids: Vec<String> = self.state.tabs.iter().map(|t| t.id.clone()).collect();
        for id in ids {
            self.abort_staged_show(&id, reason);
        }
    }

    /// Takes the queued staged-show acknowledgements: `(ack channel, the
    /// result to send)` pairs in the order they were queued. The GUI loop
    /// sends each one (so no acknowledgement is ever forgotten — every
    /// `ShowTab` receives exactly one) and the headless pixel tests probe
    /// them.
    ///
    /// `#[doc(hidden)]` seam for the headless pixel tests; the production
    /// consumer is the GUI loop.
    #[doc(hidden)]
    pub fn take_due_acks(&mut self) -> Vec<Ack> {
        std::mem::take(&mut self.due_acks)
    }

    /// The number of live ([`TextureSlot::Ready`]) texture slots: the
    /// one-live-texture invariant, at most `1` per window at any instant.
    ///
    /// `#[doc(hidden)]` seam for the headless pixel tests; the production
    /// invariant is structural (an upload only happens for the active
    /// tab, and every activation frees the outgoing slot).
    #[doc(hidden)]
    pub fn ready_slot_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| matches!(slot, TextureSlot::Ready { .. }))
            .count()
    }

    /// The number of tabs (the window is open exactly while this is
    /// non-zero).
    ///
    /// `#[doc(hidden)]` seam for the headless pixel tests.
    #[doc(hidden)]
    pub fn tab_count(&self) -> usize {
        self.state.len()
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
        // The CLI window's single tab always holds a live texture (its
        // eager PNG upload); the `Empty` arm is a defensive contentless
        // frame, not a reachable state on this path.
        if let TextureSlot::Ready { texture, .. } = &self.slots[active] {
            draw_frame(
                &mut d,
                texture,
                sw,
                sh,
                &self.state.tabs[active].image,
                self.state.tabs[active].zoom,
                self.state.tabs[active].pan_x,
                self.state.tabs[active].pan_y,
            );
        }
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
    /// loop closes the window. A tab-bar selection that switches the active
    /// tab surfaces the incoming tab's on-demand raster (when it needs one)
    /// through `jobs`, for the GUI loop to hand to the raster worker.
    ///
    /// Returns `false` when the window should close (ESC/OS close, no tab
    /// left, or the last tab closed via the bar); `true` to keep the window
    /// open.
    fn step(
        &mut self,
        rl: &mut RaylibHandle,
        thread: &RaylibThread,
        ui_closes: &mut Vec<String>,
        jobs: &mut Vec<RasterJob>,
    ) -> bool {
        if rl.window_should_close() || rl.is_key_pressed(KeyboardKey::KEY_ESCAPE) {
            return false;
        }
        if self.state.active().is_none() {
            return false;
        }
        self.apply_input(rl);
        // No synchronous raster here: an `Empty` active slot (a staged show
        // whose outcome has not landed, a tab that lost its texture on a
        // switch, a tab whose activation raster is in flight) draws the
        // placeholder frame until its raster outcome is applied by the GUI
        // loop's per-frame result drain.
        let sw = rl.get_screen_width();
        let sh = rl.get_screen_height();
        let mut d = rl.begin_drawing(thread);
        let open = draw_tabbed_frame(&mut d, self, sw, sh, ui_closes, jobs);
        if open {
            // The per-frame zoom re-raster evaluation (M-3.a) runs *after*
            // the draw: `draw_tabbed_frame` may have replaced `jobs` with
            // this frame's activation jobs (a tab switch this frame), and
            // the re-raster job must ride alongside them, not overwrite
            // them.
            if let Some(job) = self.evaluate_reraster(
                sw as f64,
                (sh as f64 - TAB_STRIP_HEIGHT as f64).max(0.0),
                now_seconds(),
            ) {
                jobs.push(job);
            }
        }
        open
    }

    /// Switches the active tab to `index` — the on-demand half of a tab
    /// switch. The tab bar's selection in [`draw_tabbed_frame`] (the only
    /// production trigger) and the [`Self::set_active`] headless seam both
    /// route here, so the two are behaviorally identical:
    ///
    /// - the outgoing tab's live texture is freed and its pending raster
    ///   request is dropped (the one-live-texture invariant; its
    ///   in-flight job may still land and is discarded while the tab is
    ///   inactive);
    /// - the incoming tab — *only* when its slot is [`TextureSlot::Empty`]
    ///   and no raster job of its covers it (an in-flight *or* pending
    ///   job at its generation satisfies the demand: its outcome commits
    ///   and uploads for an active tab) — requests its on-demand raster at
    ///   the display target ([`raster_target`]: fit × `RASTER_HEADROOM` at
    ///   the tab's zoom, clamped to [`MAX_RASTER_PIXELS`]), and the
    ///   requested job is returned for the caller to hand to the raster
    ///   worker.
    ///
    /// `content_w`/`content_h` is the content rect (the window minus the
    /// tab strip) the raster target fits into. A no-op (no job) when
    /// `index` is not a valid tab or is already the active one.
    fn activate_tab(&mut self, index: usize, content_w: f64, content_h: f64) -> Vec<RasterJob> {
        if index >= self.state.len() || index == self.state.active_index() {
            return Vec::new();
        }
        let outgoing = self.state.active_index();
        self.slots[outgoing] = TextureSlot::Empty;
        self.schedules[outgoing].drop_pending();
        self.state.active = index;
        // The incoming tab's on-demand raster, if it needs one.
        let needs_raster = matches!(self.slots[index], TextureSlot::Empty)
            && self.schedules[index].inflight.is_none()
            && self.schedules[index].pending.is_none();
        if !needs_raster {
            return Vec::new();
        }
        let tab = &self.state.tabs[index];
        // A committed tab's content is SVG and UTF-8 by construction (the
        // show path rejects non-UTF-8 at staging); `from_utf8_lossy` keeps
        // the activation path total (unreachable, but never a panic).
        let svg = String::from_utf8_lossy(&tab.image.bytes).into_owned();
        let job = RasterJob {
            id: tab.id.clone(),
            generation: tab.generation,
            scale: raster_target(
                content_w,
                content_h,
                tab.image.width as f64,
                tab.image.height as f64,
                tab.zoom,
            ),
            svg,
            kind: RasterJobKind::Activation,
        };
        self.schedules[index].request(job)
    }

    /// The per-frame zoom re-raster evaluation (the M-3.a trigger core):
    /// for the *active* tab whose slot is `Ready`, observes the settle
    /// clock with the current effective display scale `e` (the fit of the
    /// tab's logical dimensions into `content_w × content_h` at its zoom —
    /// a pan leaves `e` unchanged and is invisible to the clock) and —
    /// when [`reraster_decision`] fires (deviation beyond
    /// [`RASTER_HYSTERESIS`], `e` settled for [`ZOOM_SETTLE`], and a
    /// clamped target of a *different* raster size than the slot's and any
    /// in-flight job's scale) — requests the re-raster at
    /// `e × RASTER_HEADROOM` (capped at [`MAX_RASTER_PIXELS`]) through the
    /// tab's schedule: issued now, or held as the pending (replacing any
    /// older pending) behind a busy schedule.
    ///
    /// Skips a tab whose slot is `Empty` (its own show/activation raster
    /// covers the demand), whose show is staged (a replace is underway —
    /// its generation bump already discarded the tab's re-rasters), and
    /// whose re-raster failed in the current content generation (the
    /// no-retry latch: one log, no re-issue until the next show).
    ///
    /// `now` is the monotonic clock in seconds (the settle-debounce base).
    /// Returns the job to hand to the raster worker, if the evaluation
    /// issued one.
    ///
    /// `#[doc(hidden)]` seam: production frames call it from
    /// [`Self::step`]; the headless pixel tests call it directly with fake
    /// times.
    #[doc(hidden)]
    pub fn evaluate_reraster(
        &mut self,
        content_w: f64,
        content_h: f64,
        now: f64,
    ) -> Option<RasterJob> {
        let pos = self.state.active_index();
        let (id, generation, svg_w, svg_h, zoom, staged) = {
            let tab = &self.state.tabs[pos];
            (
                tab.id.clone(),
                tab.generation,
                tab.image.width as f64,
                tab.image.height as f64,
                tab.zoom,
                tab.staged.is_some(),
            )
        };
        let slot_scale = match &self.slots[pos] {
            TextureSlot::Ready { scale, .. } => *scale,
            TextureSlot::Empty => return None,
        };
        if staged || self.schedules[pos].failed_reraster_gen == Some(generation) {
            return None;
        }
        let svg = String::from_utf8_lossy(&self.state.tabs[pos].image.bytes).into_owned();
        let e = display_scale(content_w, content_h, svg_w, svg_h, zoom);
        let settled_for = self.schedules[pos].settle.observe(e, now);
        let inflight = self.schedules[pos].inflight.as_ref().map(|j| j.scale);
        let target = reraster_decision(e, settled_for, slot_scale, inflight, svg_w, svg_h)?;
        let job = RasterJob {
            id,
            generation,
            svg,
            scale: target,
            kind: RasterJobKind::Reraster,
        };
        self.schedules[pos].request(job).into_iter().next()
    }

    /// Re-warrants a fired job against the *live landing state* (the M-3
    /// coalescing rule): a fired *re-raster* is issued only if it still
    /// deviates — the tab is active and unstaged, its slot is `Ready`, the
    /// effective display scale `e` deviates from the *freshly landed*
    /// slot's scale by more than [`RASTER_HYSTERESIS`], and the job's
    /// target still rasters to a *different* size than the slot's scale
    /// (the no-duplicate guard: the landing may have delivered the very
    /// raster the pending wanted). A re-raster that no longer deviates is
    /// dropped and its schedule slot cleared (it is in-flight on paper
    /// only). Fired show/activation jobs are issued as-is: their warrant
    /// stands from request time, and their landing rules own the outcome.
    fn rewarrant_fired(
        &mut self,
        rl: &RaylibHandle,
        pos: usize,
        fired: Vec<RasterJob>,
    ) -> Vec<RasterJob> {
        // The schedule's bound: at most one job fires.
        let Some(job) = fired.into_iter().next() else {
            return Vec::new();
        };
        if job.kind != RasterJobKind::Reraster {
            return vec![job];
        }
        let warranted = {
            let tab = &self.state.tabs[pos];
            let active = self.state.active_index() == pos;
            let unstaged = tab.staged.is_none();
            let slot_scale = match &self.slots[pos] {
                TextureSlot::Ready { scale, .. } => *scale,
                TextureSlot::Empty => return Vec::new(),
            };
            let content_w = rl.get_screen_width() as f64;
            let content_h = (rl.get_screen_height() as f64 - TAB_STRIP_HEIGHT as f64).max(0.0);
            let e = display_scale(
                content_w,
                content_h,
                tab.image.width as f64,
                tab.image.height as f64,
                tab.zoom,
            );
            let slot_scale = slot_scale.max(f64::EPSILON);
            let d = (e / slot_scale).max(slot_scale / e);
            active
                && unstaged
                && d > RASTER_HYSTERESIS
                && !same_raster_size(
                    tab.image.width as f64,
                    tab.image.height as f64,
                    job.scale,
                    slot_scale,
                )
        };
        if warranted {
            vec![job]
        } else {
            // The pending is no longer warranted: clear the schedule slot
            // `complete` gave it (nothing is really in flight).
            self.schedules[pos].inflight = None;
            Vec::new()
        }
    }

    // -----------------------------------------------------------------------
    // Test seams (`#[doc(hidden)]`, headless pixel tests only)
    //
    // The pure policy unit tests cannot see *how the window calls* the
    // policy (an argument-order swap at a call site is invisible to them),
    // so the headless tests drive the real `show_tab` / activation paths
    // and probe the resulting live raster through these seams.
    // -----------------------------------------------------------------------

    /// The active tab's live raster: its recorded scale and the texture's
    /// output pixel size, `None` while the active slot is empty.
    ///
    /// `#[doc(hidden)]` seam for the headless pixel tests (which build a
    /// window of tabs directly); the MCP-facing API is
    /// [`WindowController`].
    #[doc(hidden)]
    pub fn active_raster(&self) -> Option<(f64, u32, u32)> {
        match self.slots.get(self.state.active_index())? {
            TextureSlot::Ready { texture, scale } => {
                Some((*scale, texture.width() as u32, texture.height() as u32))
            }
            TextureSlot::Empty => None,
        }
    }

    /// Switches the active tab to `index` — mirroring the tab-bar
    /// selection in [`draw_tabbed_frame`] exactly (the bar is the only
    /// production trigger, and headless frames produce no bar input): it
    /// routes through [`Self::activate_tab`], freeing the outgoing tab's
    /// texture slot and requesting the incoming tab's on-demand raster
    /// when it needs one.
    ///
    /// `content_w`/`content_h` is the content rect (the window minus the
    /// tab strip) the activation raster target fits into.
    ///
    /// `#[doc(hidden)]` seam for the headless pixel tests; the MCP-facing
    /// API is [`WindowController`].
    #[doc(hidden)]
    pub fn set_active(&mut self, index: usize, content_w: f64, content_h: f64) -> Vec<RasterJob> {
        self.activate_tab(index, content_w, content_h)
    }

    /// Sets the active tab's zoom multiplier (the headless re-raster tests'
    /// wheel substitute; production zoom comes from the tab frame's wheel
    /// input in [`draw_tabbed_frame`]). A non-finite or non-positive value
    /// is a no-op, mirroring [`display_scale`]'s clamp so the stored zoom
    /// and the effective scale stay consistent.
    ///
    /// `#[doc(hidden)]` seam for the headless pixel tests; the MCP-facing
    /// API is [`WindowController`].
    #[doc(hidden)]
    pub fn set_zoom(&mut self, zoom: f64) {
        if zoom.is_finite() && zoom > 0.0 {
            let pos = self.state.active_index();
            self.state.tabs[pos].zoom = zoom;
        }
    }
}

impl Default for Window {
    /// An empty window (no tabs), matching [`Window::new`] at watermark 0
    /// (the CLI path has no raster worker, so no prior window cycle exists).
    fn default() -> Self {
        Self::new(0)
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
/// SVG input is rejected with [`Error::View`] (and no window is opened):
/// the CLI path keeps its eager single-diagram PNG behavior (decode +
/// upload up front, no SVG, no raster scale policy) — the controller
/// window is the SVG-on-demand side of the input split.
pub fn run_blocking(title: &str, image: &RenderedImage) -> Result<()> {
    if fake_window_enabled() {
        return ensure_png(image);
    }
    ensure_png(image)?;
    let (mut rl, thread) = Window::init_raylib(title)?;
    // The CLI path has no raster worker (no prior window cycle, no queued
    // outcomes to discriminate): the generation watermark starts at 0.
    let mut window = Window::new(0);
    // The CLI path's eager PNG upload (the controller's SVG rasterizing
    // seam is not involved): decode + upload, then insert the tab.
    let texture = load_window_texture(&mut rl, &thread, image)?;
    window.add_ready_tab("default", title, image, texture)?;
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
    /// and `image`, and activate it. `scale` is the show's raster scale
    /// hint (a floor on the first raster's sharpness; the target is
    /// `max(hint, fit × RASTER_HEADROOM)` clamped to [`MAX_RASTER_PIXELS`],
    /// so a non-finite or non-positive hint is simply ignored). `ack` is
    /// the show's *deferred* acknowledgement, taken out of the command by
    /// the GUI loop and stored with the staged show: it is sent when the
    /// show's raster outcome is applied (an `Ok` commit; an `Err` for a
    /// failed raster, a superseded staged show, a staged show cancelled by
    /// a tab/window close, or a worker-gone condition), so a `show_tab` is
    /// never dropped silently (one racing a just-closed window re-opens
    /// the window) and never acked before its first raster has been
    /// decided.
    ShowTab {
        id: String,
        label: String,
        image: RenderedImage,
        scale: f64,
        ack: Option<Sender<std::result::Result<(), Error>>>,
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
/// queue, and the GUI- and raster-worker-thread handles.
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
    /// The raster worker thread: it rasterizes [`RasterJob`]s off the GUI
    /// thread and reports [`RasterOutcome`]s back, which the GUI loop
    /// applies in its non-blocking per-frame drain (the show path). The
    /// GUI thread owns the channel ends and dies with its dead GUI
    /// thread's job-sender drop (the worker exits on the job channel
    /// disconnecting); this handle lets the controller's `Drop` stop it.
    /// A GUI-thread respawn replaces this whole `RealView` and
    /// provisions a fresh worker.
    worker: Option<JoinHandle<()>>,
}

impl RealView {
    /// Spawns (or re-spawns) the dedicated GUI thread *and* its sibling
    /// raster worker thread and returns the command channel plus the fresh
    /// open/stopped state for them.
    fn new(title: String) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let open = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let ui_closes = Arc::new(Mutex::new(Vec::new()));
        let (raster_jobs_tx, raster_jobs_rx) = mpsc::channel::<RasterJob>();
        let (raster_results_tx, raster_results_rx) = mpsc::channel::<RasterOutcome>();

        // The worker owns no display state, so it can start before any
        // window exists; the GUI thread (not the worker) is what the window
        // depends on.
        let worker_stopped = Arc::clone(&stopped);
        let worker = Builder::new()
            .name("mermaid-viewer-raster".into())
            .spawn(move || worker_loop(raster_jobs_rx, raster_results_tx, worker_stopped))
            .map_err(|e| Error::View(format!("failed to spawn raster worker thread: {e}")))?;

        let (open_shared, stopped_shared, ui_shared) = (
            Arc::clone(&open),
            Arc::clone(&stopped),
            Arc::clone(&ui_closes),
        );
        let gui_title = title.clone();
        let thread = Builder::new()
            .name("mermaid-viewer-gui".into())
            .spawn(move || {
                gui_loop(
                    rx,
                    open_shared,
                    stopped_shared,
                    ui_shared,
                    raster_jobs_tx,
                    raster_results_rx,
                    gui_title,
                )
            })
            .map_err(|e| Error::View(format!("failed to spawn GUI thread: {e}")))?;
        Ok(Self {
            title,
            tx,
            open,
            stopped,
            ui_closes,
            thread: Some(thread),
            worker: Some(worker),
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
    /// *apply the show's first raster* (commit it, or report the typed
    /// failure) before reporting a typed [`Error::View`]. The ack is
    /// deferred to that application — the GUI thread never stalls on
    /// rasterization, it stages the tab (placeholder until the commit) and
    /// hands the work to the raster worker — so the wait bounds the whole
    /// first-raster path. A GUI thread that wedges (e.g. the display server
    /// never delivers the window-visibility event, so window creation never
    /// returns) must not drag every subsequent MCP tool call down with it:
    /// the MCP layer turns this failure into a tool error and the server
    /// keeps serving.
    pub const GUI_ACK_TIMEOUT: Duration = Duration::from_secs(10);

    /// The grace period [`WindowController`] gives the GUI thread to finish
    /// on drop before the join is detached (a wedged GUI thread must not
    /// hang the process shutdown; process exit tears the thread down).
    const GUI_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

    /// Adds a tab with ID `id` (label `label`, content `image`) or replaces
    /// it when the ID is already known: the replacement takes the tab's
    /// label and content (at the commit) and resets its zoom/pan so it
    /// re-fits, and makes the tab active in both cases. Until the first
    /// raster commits, the tab is a *staged* show: a new ID is drawn as the
    /// placeholder, a replaced ID keeps its old content live.
    ///
    /// `image` must be SVG (`Error::View` otherwise, PNG included, and
    /// non-UTF-8 bytes are a typed [`Error::Rasterize`]).
    /// `initial_scale` is the raster scale hint: a floor on the first
    /// raster's sharpness — the raster worker rasters the SVG at
    /// `max(hint, fit × RASTER_HEADROOM)` (the content rect: window minus
    /// the tab strip), clamped to [`MAX_RASTER_PIXELS`]; a non-finite or
    /// non-positive hint is ignored, never an error.
    ///
    /// The window is opened when CLOSED, including a call that races a
    /// just-closed window (never silently dropped). A new tab beyond
    /// [`Self::MAX_TABS`] is rejected with a typed [`Error::View`].
    ///
    /// Blocks **at most [`Self::GUI_ACK_TIMEOUT`]** until the GUI thread
    /// has *applied the show's first raster* — the acknowledgement is
    /// deferred to that application: the GUI thread stages the tab (no
    /// rasterization on the GUI thread), the raster worker rasterizes it
    /// off-thread, and the GUI thread commits (or rolls back) the outcome.
    /// The call reports the outcome as a typed [`Error`] on any failure:
    /// staging (capacity, non-SVG, non-UTF-8), a failed raster
    /// ([`Error::Rasterize`]), a superseded or cancelled staged show, a
    /// gone raster worker, texture upload, a dead GUI thread (re-spawned),
    /// or a wedged one (reported, never hung).
    pub fn show_tab(
        &self,
        id: &str,
        label: &str,
        image: &RenderedImage,
        initial_scale: f64,
    ) -> Result<()> {
        ensure_svg(image)?;
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
                            scale: initial_scale,
                            ack: Some(ack_tx),
                        })
                        .map_err(|_| {
                            Error::View("window GUI thread is no longer running".to_string())
                        })?;
                    ack_rx
                };
                match ack_rx.recv_timeout(Self::GUI_ACK_TIMEOUT) {
                    Ok(Ok(())) => Ok(()),
                    // The GUI thread's typed failure (raster, upload, or
                    // capacity) propagates end-to-end, unchanged.
                    Ok(Err(e)) => Err(e),
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
            // controller tells both threads to stop via the one shared
            // flag (observed at most 200 ms later on an idle thread, next
            // frame while open); then joins them so the raylib window is
            // fully closed before this handle is gone.
            let (stopped, thread, worker) = {
                let view = view.get_mut().unwrap();
                (
                    Arc::clone(&view.stopped),
                    view.thread.take(),
                    view.worker.take(),
                )
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
            if let Some(h) = worker {
                // The worker owns no display state and cannot wedge the
                // way the GUI thread can — but the same bounded pattern
                // keeps a pathological worker from hanging the shutdown.
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
// Raster worker thread (the show path's off-GUI-thread rasterizer)
// ---------------------------------------------------------------------------

/// One unit of off-thread raster work: the SVG document to rasterize, its
/// tab ID and generation (the outcome's staleness discriminators), and the
/// raster scale. Plain data: the job crosses `std::sync::mpsc` from the GUI
/// thread to the worker thread.
///
/// Public (like [`Window`]) so the headless pixel tests can read the show's
/// initial target (`scale`) and staleness discriminators off the
/// [`Window::show_tab`] / [`Window::set_active`] results and feed a
/// simulated worker outcome into [`Window::apply_raster_result`]; in
/// production the GUI loop hands it to the raster worker and nothing else
/// sees it.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct RasterJob {
    /// The tab ID the raster belongs to.
    pub id: String,
    /// The generation the job's show was staged with: unique for the GUI
    /// thread's whole lifetime (the window's counter is seeded from the
    /// GUI loop's watermark, monotonic across window open/close cycles).
    pub generation: u64,
    /// The SVG document to rasterize.
    pub svg: String,
    /// The raster scale (logical px → device px).
    pub scale: f64,
    /// What the job rasters: the show's initial target, the activation
    /// raster of an empty committed tab, or a zoom-driven re-raster of a
    /// `Ready` tab (M-3.a). The kind steers landing semantics: re-raster
    /// outcomes are re-warranted against the live state and their failures
    /// are generation-latched.
    pub kind: RasterJobKind,
}

/// What a [`RasterJob`] rasters. Plain data: it crosses `std::sync::mpsc`
/// from the GUI thread to the worker thread.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RasterJobKind {
    /// The show's initial raster (staged by [`Window::show_tab`]).
    Show,
    /// The activation raster of a committed tab with an empty slot (staged
    /// by [`Window::activate_tab`]).
    Activation,
    /// A zoom-driven re-raster of a `Ready` tab (the per-frame M-3.a
    /// policy; the outcome is re-warranted on landing and a failure
    /// latches the tab for the content generation).
    Reraster,
}

/// The worker's report for one [`RasterJob`]: the straight RGBA8 buffer plus
/// the output pixel dimensions, or the typed rasterization failure. Plain
/// data: the outcome crosses `std::sync::mpsc` from the worker thread back
/// to the GUI thread, where [`Window::apply_raster_result`] applies it
/// through the generation rules.
#[derive(Debug)]
struct RasterOutcome {
    /// The tab ID the result belongs to (echoed from the job).
    id: String,
    /// The generation the result was computed for (echoed from the job).
    generation: u64,
    /// The rasterized straight RGBA8 buffer with its pixel dimensions, or
    /// the typed [`Error::Rasterize`] failure.
    result: Result<(Vec<u8>, u32, u32)>,
}

/// The worker's idle poll interval: bounds how long an *idle* worker takes
/// to observe the shared `stopped` flag (or a job-channel disconnect).
const RASTER_WORKER_POLL: Duration = Duration::from_millis(200);

/// Body of the dedicated raster worker thread (named
/// `mermaid-viewer-raster` at spawn).
///
/// Takes [`RasterJob`]s from `jobs`, rasterizes each one off the GUI thread
/// (`crate::render::rasterize_rgba` — never panicking), and reports each
/// [`RasterOutcome`] on `results`. A rasterization failure is a *reported*
/// `Err(Error::Rasterize)` outcome, never a panic.
///
/// The loop polls with a timeout so an idle worker observes the shared
/// `stopped` flag (the controller's `Drop` sets it; `std::sync::mpsc` has
/// no way to inject shutdown into a blocked `recv`). The loop returns on
/// the flag, on the job channel disconnecting (the GUI thread is gone and
/// no more jobs will arrive), and when an outcome can no longer be
/// delivered (the GUI thread dropped its result receiver).
fn worker_loop(
    jobs: Receiver<RasterJob>,
    results: Sender<RasterOutcome>,
    stopped: Arc<AtomicBool>,
) {
    loop {
        if stopped.load(Ordering::Acquire) {
            return;
        }
        match jobs.recv_timeout(RASTER_WORKER_POLL) {
            Ok(job) => {
                let outcome = RasterOutcome {
                    id: job.id,
                    generation: job.generation,
                    result: crate::render::rasterize_rgba(&job.svg, job.scale),
                };
                if results.send(outcome).is_err() {
                    // The GUI thread is gone (its result receiver was
                    // dropped): no one to report to anymore.
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // Idle: loop back and re-check `stopped`.
            }
            Err(RecvTimeoutError::Disconnected) => return,
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
/// last tab). It takes the command mutably because the show path *stages*
/// the tab here (storing the command's deferred acknowledgement with the
/// staged show) without sending it: the acknowledgement goes out only when
/// the show's raster outcome is applied (or the show is superseded /
/// cancelled / the worker is gone). The drain stops *immediately* on such a
/// latch — and on the channel disconnecting (the controller is gone) —
/// leaving every later queued command in the channel: the CLOSED wait loop
/// re-openes the window with a queued `ShowTab` (staging it there) and its
/// acknowledgement rides the deferred-ack machinery, so a `show_tab` racing
/// a just-closed window is executed and acknowledged exactly once, never
/// acknowledged as success here and dropped with the window.
fn run_drain(rx: &Receiver<Cmd>, apply: &mut dyn FnMut(&mut Cmd) -> bool) -> bool {
    let mut latched = false;
    while !latched {
        match rx.try_recv() {
            Ok(mut cmd) => latched = apply(&mut cmd),
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => return true,
        }
    }
    latched
}

/// Drains every raster outcome the worker has reported since the last
/// frame and applies it through the generation and schedule rules
/// ([`Window::apply_raster_result`]): the GUI thread never blocks on the
/// worker, and a drained outcome that no longer matches its tab (closed
/// tab, superseded generation) is discarded there, not here. An applied
/// outcome that *fires* the tab's latest pending request (the coalescing
/// bound) puts the fired job back on `raster_jobs` for the worker.
///
/// The result channel disconnecting is a worker-gone condition: every
/// remaining staged show is cancelled (each with its typed
/// worker-gone acknowledgement) and the drain stops — there will be no
/// more outcomes to apply.
fn drain_raster_results(
    window: &mut Window,
    rl: &mut RaylibHandle,
    thread: &RaylibThread,
    results: &Receiver<RasterOutcome>,
    raster_jobs: &Sender<RasterJob>,
) {
    loop {
        match results.try_recv() {
            Ok(outcome) => {
                let fired = window.apply_raster_result(
                    rl,
                    thread,
                    &outcome.id,
                    outcome.generation,
                    outcome.result,
                );
                // A fired pending becomes the tab's in-flight job now:
                // hand it to the worker. A worker-gone condition is
                // logged (the tab draws the placeholder until its next
                // activation — re-rasters are best effort, no
                // acknowledgement is involved).
                for job in fired {
                    let id = job.id.clone();
                    if raster_jobs.send(job).is_err() {
                        eprintln!(
                            "mermaid-viewer: the raster worker is gone; the pending raster for tab \"{}\" cannot be issued",
                            id
                        );
                    }
                }
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                eprintln!(
                    "mermaid-viewer: the raster worker is gone; cancelling the in-flight shows"
                );
                window.abort_all_staged(StagedShowCancelReason::WorkerGone);
                break;
            }
        }
    }
}

/// Monotonic seconds since process start (the re-raster settle debounce's
/// clock base, M-3.a): the settle clock only ever compares *durations*
/// between observations, so the absolute epoch is irrelevant — a process
/// start keeps the value bounded and the base stable for the GUI loop's
/// whole life.
fn now_seconds() -> f64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64()
}

/// Sends every acknowledgement the window has queued to its waiting
/// [`WindowController::show_tab`] caller: the GUI thread is the single
/// sender of deferred acknowledgements (commit, typed failure, supersede,
/// cancellation, worker-gone — queued by the staging/commit/close/abort
/// paths). A dropped receiver (the caller gave up on the `GUI_ACK_TIMEOUT`
/// wait) is a no-op, as it must be: the window stays consistent without it.
fn send_due_acks(window: &mut Window) {
    for (ack, result) in window.take_due_acks() {
        let _ = ack.send(result);
    }
}

/// Body of the dedicated GUI thread.
///
/// Loops forever: while CLOSED it waits on the channel (a `ShowTab` there
/// opens the window — titled `title` — by staging that tab, and a
/// `CloseTab`/`Close` is an idempotent no-op; a disconnected channel or a
/// stopped flag means the controller is gone); while OPEN it drains the
/// channel every frame (non-blocking, so events keep pumping), stopping the
/// drain exactly at a latched close so any queued `ShowTab` survives for
/// the CLOSED wait loop. The thread outlives `close` so the window can be
/// re-opened: raylib re-initializes on this same thread after `CloseWindow`.
///
/// The show path is *staging + deferred ack*: a `ShowTab` is staged on this
/// thread (no rasterization, no texture upload — a new tab draws the
/// placeholder, a replace keeps its old content live), its issued
/// [`RasterJob`] is handed to the raster worker on `raster_jobs`, and the
/// show's acknowledgement waits in the window's due-acks until a
/// [`RasterOutcome`] comes back on `raster_results` and is applied in the
/// per-frame non-blocking [`drain_raster_results`] (commit + texture upload
/// for the active tab, or rollback). [`send_due_acks`] then delivers every
/// queued acknowledgement — exactly one per show. A worker-gone condition
/// (a job the worker can no longer take, or the result channel
/// disconnecting) cancels the in-flight staged shows with typed
/// acknowledgements.
///
/// On-demand slots ride the same channels: a tab-bar selection frees the
/// outgoing tab's texture and — when the incoming tab holds none and no
/// in-flight raster covers it — requests its activation raster (the frame
/// surfaces it, the loop hands it to the worker); the outcome then
/// uploads for the active tab (or is discarded for an inactive one). The
/// per-tab schedules keep at most one in-flight job and one latest pending
/// per tab, and a pending fires (back onto `raster_jobs`) when the
/// in-flight outcome lands.
///
/// The *generation watermark* is owned here and carried across window
/// open/close cycles: each window is seeded from it (`Window::new`) and
/// the next window takes over the value the closed one left at its counter
/// (`Window::generation_watermark`). The raster worker and its result
/// channel outlive the window, so an outcome queued while the window is
/// down can still land after the next one opens — the watermark is what
/// keeps it from matching (every show's generation is unique for the GUI
/// thread's whole lifetime and is never re-issued), so a stale outcome
/// cannot commit stale content or consume a new show's acknowledgement.
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
    raster_jobs: Sender<RasterJob>,
    raster_results: Receiver<RasterOutcome>,
    title: String,
) {
    // The generation watermark: the next generation to hand out, kept
    // monotonic across window open/close cycles. The raster worker and its
    // result channel outlive any single window (both are created here,
    // once, for the GUI thread's life), so an outcome queued while one
    // window is down can still arrive after the next window opens — a
    // per-window counter would then re-issue the closed window's
    // generations, letting that outcome match and commit. The watermark is
    // what keeps every show's generation unique for the GUI thread's whole
    // lifetime.
    let mut generation_watermark: u64 = 0;
    loop {
        // CLOSED: wait until a ShowTab arrives; it re-opens the window with
        // the controller's spawn title as the OS title, so a show racing a
        // just-closed window is never dropped. (A `ShowTab` without its
        // acknowledgement cannot happen — the controller always sets it —
        // but a stray one is skipped, never a panic.)
        let (id, label, image, scale, ack) = loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Cmd::ShowTab {
                    id,
                    label,
                    image,
                    scale,
                    ack: Some(ack),
                }) => break (id, label, image, scale, ack),
                Ok(Cmd::ShowTab { ack: None, .. }) => continue,
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
                let _ = ack.send(Err(e));
                continue;
            }
        };

        // Stage the first show (no raster on the GUI thread) and hand the
        // issued job(s) to the worker. The acknowledgement defers to the
        // outcome — the window opens before the first raster has landed.
        // The window is seeded from the carried watermark: it must never
        // re-issue a generation a prior window cycle's queued outcome can
        // carry.
        let mut window = Window::new(generation_watermark);
        match window.show_tab(&rl, &id, &label, &image, scale, ack) {
            Ok(jobs) => {
                for job in jobs {
                    if raster_jobs.send(job).is_err() {
                        // The worker is already gone: cancel the staged
                        // show (typed worker-gone acknowledgement) instead
                        // of leaving it stranded.
                        eprintln!(
                            "mermaid-viewer: the raster worker is gone; the first show of tab \"{}\" cannot complete",
                            id
                        );
                        window.abort_staged_show(&id, StagedShowCancelReason::WorkerGone);
                    }
                }
            }
            Err(e) => {
                // Staging failed (non-SVG, non-UTF-8, capacity): the show
                // already acked the typed failure; the window is empty and
                // stays closed.
                eprintln!("mermaid-viewer: the window's first show failed: {e}");
            }
        }
        send_due_acks(&mut window);
        if !window.state.is_empty() {
            open.store(true, Ordering::Release);
        }

        let mut closed = false;
        while !closed {
            // A dropped controller stops the thread on the next frame.
            if stopped.load(Ordering::Acquire) {
                break;
            }
            // Drain all pending commands before drawing this frame so a
            // queued tab operation is never painted over by a stale frame.
            // The drain *stages* `ShowTab`s (deferring their
            // acknowledgements to the raster-result drain below) and stops
            // exactly at a latched close (an unknown-ID `CloseTab` in the
            // batch is not one: it is a no-op), leaving a queued `ShowTab`
            // behind for the CLOSED wait loop.
            let mut apply = |cmd: &mut Cmd| -> bool {
                match cmd {
                    Cmd::ShowTab {
                        id,
                        label,
                        image,
                        scale,
                        ack,
                    } => {
                        let Some(ack) = ack.take() else {
                            // Defensive: cannot happen (the controller
                            // always sets the acknowledgement).
                            return false;
                        };
                        match window.show_tab(&rl, id, label, image, *scale, ack) {
                            Ok(jobs) => {
                                for job in jobs {
                                    if raster_jobs.send(job).is_err() {
                                        // The worker is gone: cancel the
                                        // staged show (typed worker-gone
                                        // acknowledgement in the due-acks).
                                        eprintln!(
                                            "mermaid-viewer: the raster worker is gone; the staged show for tab \"{}\" cannot complete",
                                            id
                                        );
                                        window.abort_staged_show(
                                            id,
                                            StagedShowCancelReason::WorkerGone,
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                // Staging failed (capacity, non-SVG,
                                // non-UTF-8): the show already acked the
                                // typed failure; the window stays up.
                                eprintln!("mermaid-viewer: tab show failed: {e}");
                            }
                        }
                        false
                    }
                    Cmd::CloseTab { id } => {
                        // Removing the last tab closes the window (and
                        // cancels the removed tab's staged show, if any).
                        window.close_tab(id) && window.state.is_empty()
                    }
                    Cmd::Close => {
                        // Close the window and all its tabs (each staged
                        // show cancelled with its typed acknowledgement).
                        window.close_all();
                        true
                    }
                }
            };
            if run_drain(&rx, &mut apply) {
                closed = true;
            }

            if !closed {
                // The per-frame, non-blocking raster-result drain: apply
                // every outcome the worker has reported (commit + texture
                // upload for the active tab, rollback, stale-outcome
                // discard, activation-raster upload) and deliver the
                // acknowledgements that became due; the fired pending
                // jobs go back to the worker. The GUI thread never stalls
                // on the worker.
                drain_raster_results(&mut window, &mut rl, &thread, &raster_results, &raster_jobs);
                send_due_acks(&mut window);

                // The tabbed frame pushes any bar-closed tab ID for this frame into
                // a local buffer; it is published to the shared queue only *after*
                // the (potentially blocking) draw has returned. The shared queue
                // lock is therefore never held across `window.step`/`EndDrawing` —
                // a display server that jams inside the draw must not be able to
                // block `drain_tab_closes` (and, behind the State mutex, every MCP
                // commit) on a frame lock. A bar close cancels the tab's staged
                // show (if any) and queues its acknowledgement, delivered here
                // right after the draw.
                let mut frame_closes = Vec::new();
                // The tab bar's selection (if any this frame) surfaces the
                // incoming tab's on-demand raster here; it goes to the
                // worker only after the (potentially blocking) draw has
                // returned, like the close IDs above.
                let mut frame_jobs = Vec::new();
                if !window.step(&mut rl, &thread, &mut frame_closes, &mut frame_jobs) {
                    closed = true;
                }
                for job in frame_jobs {
                    let id = job.id.clone();
                    if raster_jobs.send(job).is_err() {
                        // The worker is gone: the tab draws the
                        // placeholder until its next activation (a
                        // re-raster is best effort, no acknowledgement).
                        eprintln!(
                            "mermaid-viewer: the raster worker is gone; the activation raster for tab \"{}\" cannot be issued",
                            id
                        );
                    }
                }
                if !frame_closes.is_empty() {
                    let mut q = ui_closes.lock().unwrap_or_else(|e| e.into_inner());
                    q.extend(frame_closes);
                }
                send_due_acks(&mut window);
            }
        }

        // The window is going down: cancel every staged show still in
        // flight (a `Close`/last-`CloseTab` latch already cancelled theirs
        // through `close_tab`/`close_all` with the tab-close wording; an
        // ESC/OS close does not) with the window-close cause, and deliver
        // what is owed before the window is dropped.
        window.abort_all_staged(StagedShowCancelReason::WindowClosed);
        send_due_acks(&mut window);
        // Carry the generations this window issued into the next one: a
        // re-opened window must never re-issue them (an old outcome queued
        // on the surviving worker channel would match).
        generation_watermark = window.generation_watermark();
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

    /// A small solid-color SVG document, built directly (no mermaid source
    /// needed): a full-rect SVG in `width` × `height` logical units.
    fn test_svg(width: u32, height: u32, hex: &str) -> RenderedImage {
        let doc = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}"><rect width="{width}" height="{height}" fill="{hex}"/></svg>"#
        );
        RenderedImage {
            format: ImageFormat::Svg,
            width,
            height,
            bytes: doc.into_bytes(),
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

        // The fake backend never rasterizes: the SVG fixture's content and
        // the show's scale hint are irrelevant here.
        let image = test_svg(4, 4, "#123456");
        let controller = WindowController::spawn("lifecycle").expect("fake spawn");
        assert!(!controller.is_open(), "spawned controller starts CLOSED");

        controller
            .show_tab("a", "A", &image, 2.0)
            .expect("first show_tab opens the window");
        assert!(controller.is_open(), "show_tab → OPEN");

        controller
            .show_tab("b", "B", &image, 2.0)
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
            .show_tab("a", "A", &image, 2.0)
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

        // The fake backend never rasterizes: the hint is ignored.
        let image = test_svg(4, 4, "#123456");
        let controller = WindowController::spawn("replace").expect("fake spawn");

        controller
            .show_tab("a", "A", &image, 2.0)
            .expect("first show adds the tab");
        controller
            .show_tab("a", "A again", &image, 2.0)
            .expect("a second show of the same ID replaces, never fails");
        assert!(controller.is_open(), "the window stays OPEN");

        // If the duplicate had counted against the limit, one of these 15
        // fresh IDs would have to fail: it does not, so exactly one "a"
        // tab exists.
        for i in 1..=15 {
            let id = format!("tab-{i}");
            controller
                .show_tab(&id, &id, &image, 2.0)
                .expect("fresh ID while capacity allows");
        }
        assert!(
            matches!(
                controller.show_tab("tab-16", "tab-16", &image, 2.0),
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

        // The fake backend never rasterizes: the hint is ignored.
        let image = test_svg(4, 4, "#123456");
        let controller = WindowController::spawn("cap").expect("fake spawn");

        for i in 0..WindowController::MAX_TABS {
            let id = format!("cap-{i}");
            controller
                .show_tab(&id, &id, &image, 2.0)
                .expect("tab within the capacity limit");
        }
        assert!(controller.is_open(), "a full window is OPEN");

        let err = controller
            .show_tab("cap-16", "cap-16", &image, 2.0)
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
                controller.show_tab("cap-16", "cap-16", &image, 2.0),
                Err(Error::View(_))
            ),
            "the rejected tab was not added"
        );
        // …and replacement at capacity succeeds.
        controller
            .show_tab("cap-0", "cap-0 again", &image, 2.0)
            .expect("replacement at capacity succeeds");
        controller
            .show_tab("cap-15", "cap-15 again", &image, 2.0)
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

        // The fake backend never rasterizes: the hint is ignored.
        let image = test_svg(4, 4, "#123456");
        let controller = WindowController::spawn("unknown-close").expect("fake spawn");

        controller.show_tab("a", "A", &image, 2.0).expect("add a");
        controller.show_tab("b", "B", &image, 2.0).expect("add b");

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
            .show_tab("a", "A", &image, 2.0)
            .expect("re-open with a");
        controller
            .show_tab("b", "B", &image, 2.0)
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
    /// loop, which re-opens the window by staging it (its deferred
    /// acknowledgement rides the due-ack machinery) — a `show_tab`
    /// racing a just-closed window is never acknowledged as success and
    /// dropped with the window. The drain also mirrors the deferred-ack
    /// model: a `ShowTab` *stages* (the acknowledgement is taken, nothing
    /// is sent), and a latched close cancels the in-flight staged show —
    /// its acknowledgement is sent with a typed [`Error::View`] naming
    /// the cancellation. The same latch must hold when the channel
    /// disconnects (the controller is gone): the drain stops, it does not
    /// spin.
    #[test]
    fn drain_stops_at_latched_close_and_leaves_queued_show_tab() {
        // The drain itself never rasterizes, but the queued command is a
        // real show-path command: SVG fixture + a scale hint.
        let image = test_svg(2, 2, "#123456");

        /// A fake window model standing in for the live window, mirroring
        /// the deferred-ack show path for the parts the drain can observe:
        /// a `ShowTab` stages (the deferred ack is taken, nothing is sent
        /// until an outcome is applied), a latched close (a `Close`, or a
        /// `CloseTab` that removed the last tab) cancels the in-flight
        /// staged show — its ack is sent with a typed [`Error::View`]
        /// naming the cancellation — and a `ShowTab` queued behind the
        /// latch is neither executed nor acked.
        struct FakeWindow {
            tabs: Vec<String>,
            staged: bool,
            applied: Vec<String>,
            sent: Vec<std::result::Result<(), Error>>,
        }
        impl FakeWindow {
            fn new(tabs: Vec<String>) -> Self {
                Self {
                    tabs,
                    staged: false,
                    applied: Vec::new(),
                    sent: Vec::new(),
                }
            }
            fn apply(&mut self, cmd: &mut Cmd) -> bool {
                match cmd {
                    Cmd::ShowTab { id, ack, .. } => {
                        self.applied.push(format!("show:{id}"));
                        if !self.tabs.iter().any(|t| t == id) {
                            self.tabs.push(id.clone());
                        }
                        // Staging: the deferred ack is stored with the
                        // staged show; nothing is sent here.
                        let _ = ack
                            .take()
                            .expect("a production ShowTab always carries its ack");
                        self.staged = true;
                        false
                    }
                    Cmd::CloseTab { id } => {
                        self.applied.push(format!("close_tab:{id}"));
                        let removed = self.tabs.iter().position(|t| t == id).is_some();
                        if removed {
                            self.tabs.retain(|t| t != id);
                        }
                        if removed && self.tabs.is_empty() {
                            self.cancel_staged();
                        }
                        removed && self.tabs.is_empty()
                    }
                    Cmd::Close => {
                        self.applied.push("close".to_string());
                        self.tabs.clear();
                        self.cancel_staged();
                        true
                    }
                }
            }
            fn cancel_staged(&mut self) {
                if self.staged {
                    self.staged = false;
                    self.sent.push(Err(Error::View(
                        "the staged show was cancelled because the tab was closed".to_string(),
                    )));
                }
            }
        }

        // (1) `Close` followed by a queued `ShowTab`: the drain latches on
        // the `Close` and the `ShowTab` survives in the channel unacked.
        {
            let mut fake = FakeWindow::new(vec!["a".to_string()]);
            let (tx, rx) = mpsc::channel();
            let (ack, _ack_rx) = mpsc::channel();
            tx.send(Cmd::Close).expect("send Close");
            tx.send(Cmd::ShowTab {
                id: "b".into(),
                label: "B".into(),
                image: image.clone(),
                scale: 1.0,
                ack: Some(ack),
            })
            .expect("send queued ShowTab");
            drop(tx);
            assert!(
                run_drain(&rx, &mut |cmd: &mut Cmd| fake.apply(cmd)),
                "Close latches the drain"
            );
            assert_eq!(
                fake.applied,
                vec!["close"],
                "a ShowTab queued behind a Close is never executed by the drain"
            );
            assert!(
                fake.sent.is_empty(),
                "no staged show was in flight: nothing is acked by the latch"
            );
            let queued = rx
                .recv()
                .expect("the queued ShowTab survives for the CLOSED wait loop");
            assert!(
                matches!(&queued, Cmd::ShowTab { id, .. } if id == "b"),
                "the surviving command is the queued ShowTab"
            );
            assert!(
                matches!(&queued, Cmd::ShowTab { ack: Some(_), .. }),
                "the surviving ShowTab keeps its deferred acknowledgement"
            );
        }

        // (2) An in-flight staged show followed by a latched `Close`: the
        // staged show is cancelled — its deferred acknowledgement is sent
        // with a typed [`Error::View`] naming the cancellation.
        {
            let mut fake = FakeWindow::new(vec!["a".to_string()]);
            let (tx, rx) = mpsc::channel();
            let (ack, ack_rx) = mpsc::channel();
            tx.send(Cmd::ShowTab {
                id: "a".into(),
                label: "A".into(),
                image: image.clone(),
                scale: 1.0,
                ack: Some(ack),
            })
            .expect("send ShowTab");
            tx.send(Cmd::Close).expect("send Close");
            drop(tx);
            assert!(
                run_drain(&rx, &mut |cmd: &mut Cmd| fake.apply(cmd)),
                "Close latches the drain"
            );
            assert_eq!(
                fake.applied,
                vec!["show:a", "close"],
                "the staged show and the close both run"
            );
            let [result] = fake.sent.as_slice() else {
                panic!("the in-flight staged show is acked exactly once by the latch");
            };
            assert!(
                matches!(result, std::result::Result::Err(Error::View(_))),
                "the cancelled staged show is a typed View error, got: {result:?}"
            );
            let _ = ack_rx.try_recv(); // the ack rides `fake.sent`, not the channel
            assert!(
                rx.try_recv().is_err(),
                "the channel is empty behind the latch"
            );
        }

        // (3) An in-flight staged show followed by a last-tab `CloseTab`:
        // same contract — the drain latches and the staged show is
        // cancelled.
        {
            let mut fake = FakeWindow::new(vec!["a".to_string()]);
            let (tx, rx) = mpsc::channel();
            let (ack, _ack_rx) = mpsc::channel();
            tx.send(Cmd::ShowTab {
                id: "a".into(),
                label: "A".into(),
                image: image.clone(),
                scale: 1.0,
                ack: Some(ack),
            })
            .expect("send ShowTab");
            tx.send(Cmd::CloseTab { id: "a".into() })
                .expect("send CloseTab");
            drop(tx);
            assert!(
                run_drain(&rx, &mut |cmd: &mut Cmd| fake.apply(cmd)),
                "removing the last tab latches the drain"
            );
            assert_eq!(
                fake.applied,
                vec!["show:a", "close_tab:a"],
                "the staged show and the close both run"
            );
            let [result] = fake.sent.as_slice() else {
                panic!("the in-flight staged show is acked exactly once by the latch");
            };
            assert!(
                matches!(result, std::result::Result::Err(Error::View(_))),
                "the cancellation is a typed View error, got: {result:?}"
            );
        }

        // (4) A non-last-tab `CloseTab` does not latch: the batch runs to
        // completion.
        {
            let mut fake = FakeWindow::new(vec!["a".to_string(), "b".to_string()]);
            let (tx, rx) = mpsc::channel();
            tx.send(Cmd::CloseTab { id: "a".into() })
                .expect("send CloseTab");
            tx.send(Cmd::Close).expect("send Close");
            drop(tx);
            assert!(
                run_drain(&rx, &mut |cmd: &mut Cmd| fake.apply(cmd)),
                "Close latches the drain"
            );
            assert_eq!(
                fake.applied,
                vec!["close_tab:a", "close"],
                "a non-latching CloseTab does not stop the drain"
            );
        }

        // (5) A disconnected channel latches the drain immediately: nothing
        // is applied and nothing is acked (and the drain does not spin).
        {
            let mut fake = FakeWindow::new(Vec::new());
            let (tx, rx) = mpsc::channel::<Cmd>();
            drop(tx);
            assert!(
                run_drain(&rx, &mut |cmd: &mut Cmd| fake.apply(cmd)),
                "disconnection latches the drain"
            );
            assert!(
                fake.applied.is_empty(),
                "nothing is applied on a disconnected drain"
            );
            assert!(
                fake.sent.is_empty(),
                "nothing is acked on a disconnected drain"
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
        let image = test_svg(2, 2, "#123456");

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

    /// Shared verification plan: `controller_rejects_png_image`.
    ///
    /// A PNG [`RenderedImage`] is rejected with [`Error::View`] by
    /// `show_tab` — in both the CLOSED and OPEN states, leaving the window
    /// state unchanged — and the CLI's [`run_blocking`] keeps rejecting
    /// SVG input (its contract is PNG-only) under the fake backend.
    #[test]
    fn controller_rejects_png_image() {
        std::env::set_var(FAKE_WINDOW_ENV, "1");

        let svg = test_svg(100, 50, "#123456");
        let png = test_png(4, 4, [0, 0, 0, 255]);

        let controller = WindowController::spawn("png-reject").expect("fake spawn");
        assert!(
            matches!(
                controller.show_tab("diag", "diag", &png, 2.0),
                Err(Error::View(_))
            ),
            "PNG rejected while CLOSED"
        );
        assert!(
            !controller.is_open(),
            "a rejected show_tab did not open the window"
        );

        controller
            .show_tab("diag", "diag", &svg, 2.0)
            .expect("SVG opens the window");
        assert!(
            matches!(
                controller.show_tab("diag2", "diag2", &png, 2.0),
                Err(Error::View(_))
            ),
            "PNG rejected while OPEN"
        );
        assert!(
            controller.is_open(),
            "the window stays open after a rejection"
        );

        // The window state is unchanged by the rejection: the known tab is
        // still replaceable, and 15 fresh IDs still fit — had the rejected
        // tab taken a slot, these would already be at the capacity limit.
        controller
            .show_tab("diag", "diag", &svg, 2.0)
            .expect("the known tab is still replaceable");
        for i in 0..15 {
            let id = format!("ok-{i}");
            controller
                .show_tab(&id, &id, &svg, 2.0)
                .expect("capacity is unchanged by the rejection");
        }

        assert!(
            matches!(run_blocking("png-reject", &svg), Err(Error::View(_))),
            "run_blocking rejects SVG input (the CLI path stays PNG-only)"
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

    /// Shared verification plan: `raster_target_respects_hint_floor_and_cap`.
    ///
    /// Pure show-scale policy (no raylib, runs under every feature pass):
    /// the show's raster target is `max(hint, fit × RASTER_HEADROOM)`
    /// clamped to [`MAX_RASTER_PIXELS`] — the hint is a *floor* on
    /// sharpness (a smaller hint never shrinks the fit-based target), a
    /// larger hint applies only while the pixel budget holds, the pixel
    /// cap wins over the hint, and degenerate hints (non-finite, ≤ 0) are
    /// ignored, never an error.
    #[test]
    fn raster_target_respects_hint_floor_and_cap() {
        // Content 800×564 (the 800×600 window minus the 36 px tab strip),
        // SVG 100×50: the fit e = min(8.0, 11.28) = 8.0, base = 8.0 × 1.25
        // = 10.0.
        let base = raster_target(800.0, 564.0, 100.0, 50.0, 1.0);
        assert!((base - 10.0).abs() < 1e-9, "fit × headroom base: {base}");

        // A hint below the base never shrinks the target (it is a floor,
        // not a ceiling).
        assert_eq!(
            raster_show_target(800.0, 564.0, 100.0, 50.0, 1.0, 2.0),
            10.0,
            "a hint below the base is ignored"
        );

        // A hint above the base applies while the pixel budget holds:
        // 100×20 × 50×20 = 2.0e6 px, under the cap.
        let shown = raster_show_target(800.0, 564.0, 100.0, 50.0, 1.0, 20.0);
        assert_eq!(shown, 20.0, "a hint within the pixel budget is honored");
        assert!(
            raster_pixel_count(100.0, 50.0, shown) <= MAX_RASTER_PIXELS as u128,
            "the hinted raster fits the pixel budget"
        );

        // The pixel cap wins over the hint: a 10000×10000 SVG at the
        // hinted 2.0 would be 4e8 px, far past the cap — the clamped
        // target stays under the cap and strictly below the hint.
        let clamped = raster_show_target(800.0, 564.0, 10_000.0, 10_000.0, 1.0, 2.0);
        assert!(
            (clamped - 2.0).abs() > 1e-12,
            "the hint is not honored beyond the pixel cap"
        );
        assert!(
            clamped < 2.0 && clamped > 0.0,
            "the clamped target stays below the hint: {clamped}"
        );
        assert!(
            raster_pixel_count(10_000.0, 10_000.0, clamped) <= MAX_RASTER_PIXELS as u128,
            "the clamped raster fits the pixel budget"
        );

        // Degenerate hints (non-finite, ≤ 0) are ignored, never an error:
        // the target falls back to the plain base.
        for hint in [0.0, f64::NAN, -5.0] {
            assert_eq!(
                raster_show_target(800.0, 564.0, 100.0, 50.0, 1.0, hint),
                10.0,
                "degenerate hint {hint:?} falls back to the base"
            );
        }
    }

    /// Shared verification plan: `reraster_hysteresis_threshold`.
    ///
    /// Pure re-raster decision (no raylib, runs under every feature
    /// pass): a re-raster is warranted only when the display scale `e`
    /// deviates from the slot's scale by *more than* [`RASTER_HYSTERESIS`]
    /// (`d = max(e/s, s/e) > 1.5`) — the band the show's own
    /// `RASTER_HEADROOM` of 1.25 sits inside (a fresh show never triggers
    /// its own re-raster), the exact boundary `d == 1.5` does not fire
    /// (no oscillation at the band's edge), and both directions
    /// (zoom-in and zoom-out) are symmetric.
    #[test]
    fn reraster_hysteresis_threshold() {
        // Content 800×564 (the 800×600 window minus the 36 px tab strip),
        // 8×8 SVG: the fit e = min(100, 70.5) × zoom = 70.5 × zoom. The
        // show's own target is e × 1.25 = 88.125 at zoom 1.
        let (svg_w, svg_h) = (8.0, 8.0);
        let slot = 88.125;

        // The show's headroom deviation (1.25 at rest) is inside the band:
        // a fresh, settled show never re-rasters itself.
        let e = display_scale(800.0, 564.0, svg_w, svg_h, 1.0);
        assert!((e - 70.5).abs() < 1e-12, "fit at zoom 1: {e}");
        assert!(
            reraster_decision(e, 10.0, slot, None, svg_w, svg_h).is_none(),
            "the show's own headroom (d = 1.25) is within the hysteresis band"
        );

        // A small zoom-in stays inside the band (d < 1.5).
        let e = display_scale(800.0, 564.0, svg_w, svg_h, 1.5);
        assert!(
            reraster_decision(e, 10.0, slot, None, svg_w, svg_h).is_none(),
            "d = 1.2 at zoom 1.5 is within the band"
        );

        // The exact boundary d == 1.5 does not fire (strict `>`, no
        // oscillation at the band's edge): 88.125 × 1.5 = 132.1875 is
        // exactly representable, so the division is exact.
        let e = display_scale(800.0, 564.0, svg_w, svg_h, 1.875);
        assert!(
            (e - 132.1875).abs() < 1e-12,
            "fit at the boundary zoom: {e}"
        );
        assert!(
            reraster_decision(e, 10.0, slot, None, svg_w, svg_h).is_none(),
            "d == RASTER_HYSTERESIS exactly is not a crossing"
        );

        // Zoom-in past the band fires at the clamped headroom target:
        // 141.0 × 1.25 = 176.25 (2115 → 8 × 176.25 = 1410 px, under the
        // pixel cap, so unclamped).
        let e = display_scale(800.0, 564.0, svg_w, svg_h, 2.0);
        assert_eq!(
            reraster_decision(e, 10.0, slot, None, svg_w, svg_h),
            Some(176.25),
            "d = 1.6 crosses the band zoom-in"
        );

        // Zoom-out past the band fires symmetrically: 35.25 × 1.25 =
        // 44.0625 (352.5 → 8 × 44.0625 = 352.5 px).
        let e = display_scale(800.0, 564.0, svg_w, svg_h, 0.5);
        assert_eq!(
            reraster_decision(e, 10.0, slot, None, svg_w, svg_h),
            Some(44.0625),
            "d = 2.5 crosses the band zoom-out"
        );

        // The no-duplicate guard: a target rasters to the *same output
        // size* as an in-flight job's scale — even at a scale the clamped
        // value is not bit-equal to (a ulp apart) — is a no-op. (The
        // zoom-0.5 case: product 8 × 44.0625 = 352.5 sits mid-cell, so a
        // ulp shift cannot change the ceiling.)
        let e = display_scale(800.0, 564.0, svg_w, svg_h, 0.5);
        assert!(
            reraster_decision(e, 10.0, slot, Some(44.0625), svg_w, svg_h).is_none(),
            "a target equal to the in-flight raster size is not re-requested"
        );
        assert!(
            reraster_decision(
                e,
                10.0,
                slot,
                Some(44.0625 * (1.0 - f64::EPSILON)),
                svg_w,
                svg_h
            )
            .is_none(),
            "the guard is by output size, not by scale equality"
        );

        // Degenerate `e` (a degenerate content rect) is never a crossing.
        assert!(reraster_decision(f64::NAN, 10.0, slot, None, svg_w, svg_h).is_none());
        assert!(reraster_decision(0.0, 10.0, slot, None, svg_w, svg_h).is_none());
    }

    /// Shared verification plan: `reraster_settles_after_scale_pause`.
    ///
    /// Pure settle clock (no raylib, fake time): the clock resets exactly
    /// when the observed display scale `e` *changes* (a wheel notch, a
    /// resize) — a pan leaves `e` untouched and never resets it — and the
    /// decision requires `e` to have been unchanged for [`ZOOM_SETTLE`]
    /// (200 ms), so a crossing that is still in motion at 199.99 ms does
    /// not fire and the same crossing settles into a fire at 200 ms.
    #[test]
    fn reraster_settles_after_scale_pause() {
        let (svg_w, svg_h) = (8.0, 8.0);
        let slot = 88.125;
        let settle = ZOOM_SETTLE.as_secs_f64();
        assert!((settle - 0.2).abs() < 1e-12, "ZOOM_SETTLE is 200 ms");

        let mut clock = SettleClock::default();

        // A first observation is a change: nothing is settled yet.
        assert_eq!(
            clock.observe(70.5, 0.0),
            0.0,
            "a fresh observation settles nothing"
        );

        // Repeated observations of the *same* `e` accumulate settle time
        // (per-frame evaluation at rest; a pan is invisible to the clock).
        assert_eq!(
            clock.observe(70.5, 0.1),
            0.1,
            "an unchanged `e` accumulates"
        );
        assert_eq!(
            clock.observe(70.5, 100.0),
            100.0,
            "a long pause stays settled"
        );

        // A *changed* `e` (a wheel notch) resets the clock to zero — even
        // after a long settle — and the crossing must re-settle.
        let e_cross = 141.0; // zoom 2.0: d = 1.6 > 1.5
        assert_eq!(
            clock.observe(e_cross, 100.1),
            0.0,
            "a change in `e` resets the settle clock"
        );
        assert!(
            reraster_decision(
                e_cross,
                clock.observe(e_cross, 100.19999),
                slot,
                None,
                svg_w,
                svg_h
            )
            .is_none(),
            "a crossing at 199.99 ms of settle does not fire"
        );
        let settled = clock.observe(e_cross, 100.3);
        assert!(
            (settled - 0.2).abs() < 1e-9,
            "exactly ZOOM_SETTLE after the change is settled (f64): {settled}"
        );
        assert_eq!(
            reraster_decision(e_cross, settled, slot, None, svg_w, svg_h),
            Some(176.25),
            "a settled crossing fires at the clamped headroom target"
        );

        // A second notch interrupts a *settling* crossing and restarts
        // the clock from the new value (the old target's settle time does
        // not carry over).
        let e_cross2 = 211.5; // zoom 3.0
        clock.observe(e_cross2, 100.4);
        assert!(
            reraster_decision(
                e_cross2,
                clock.observe(e_cross2, 100.5),
                slot,
                None,
                svg_w,
                svg_h
            )
            .is_none(),
            "the new crossing's settle time is 0.1 s, not the old 0.2 s"
        );
    }

    /// Shared verification plan: `reraster_target_respects_pixel_cap`.
    ///
    /// Pure re-raster target (no raylib): the target is
    /// `e × RASTER_HEADROOM` clamped to [`MAX_RASTER_PIXELS`] — a zoom-in
    /// past the budget clamps to the cap's side (4096×4096 for an 8×8
    /// source, exactly the budget), and at the cap the decision is a
    /// no-op even past the hysteresis crossing: the slot already holds
    /// the largest raster the budget allows (the re-raster-loop guard).
    #[test]
    fn reraster_target_respects_pixel_cap() {
        let (svg_w, svg_h) = (8.0, 8.0);

        // Zoom 6.0: e = 423.0, raw target 423 × 1.25 = 528.75 would be
        // 4230² px — past the cap. The clamped target is the cap's side:
        // ceil(8 × 512) = 4096, and 4096² == MAX_RASTER_PIXELS. (The
        // cap's bisection lands exactly on 512.0: it is the largest scale
        // whose raster fits, and it is representable.)
        let e = display_scale(800.0, 564.0, svg_w, svg_h, 6.0);
        assert!((e - 423.0).abs() < 1e-12, "fit at zoom 6: {e}");
        let slot = 88.125;
        assert!(
            !same_raster_size(svg_w, svg_h, 528.75, 512.0),
            "the unclamped target is a bigger raster than the cap's side"
        );
        let target = clamp_raster_scale(svg_w, svg_h, e * RASTER_HEADROOM);
        assert_eq!(target, 512.0, "the target clamps to the cap's side exactly");
        assert_eq!(
            raster_pixel_count(svg_w, svg_h, target),
            MAX_RASTER_PIXELS as u128,
            "the clamped raster uses the whole pixel budget (no more)"
        );
        assert_eq!(
            reraster_decision(e, 10.0, slot, None, svg_w, svg_h),
            Some(512.0),
            "a settled crossing past the budget fires at the clamped target"
        );

        // At the cap: the slot already holds 512.0 (4096×4096). A further
        // zoom-in crosses the hysteresis band (d = 846/512 > 1.5) but the
        // clamped target is the *same raster* — the decision is a no-op,
        // never a re-raster at the same size (the loop guard).
        assert_eq!(
            reraster_decision(
                display_scale(800.0, 564.0, svg_w, svg_h, 12.0),
                10.0,
                512.0,
                None,
                svg_w,
                svg_h,
            ),
            None,
            "past the cap the target is the slot's own raster: no re-raster"
        );

        // The no-duplicate guard's semantic form: a scale a ulp *below*
        // the cap's side rasters to the same 4096² (product 8 × 511.999…
        // = 4095.999… — the ceiling is unchanged), while a ulp *above* it
        // would not (the boundary's one-sidedness).
        assert!(
            same_raster_size(svg_w, svg_h, 512.0, 512.0 - 512.0 * f64::EPSILON),
            "cap-side scales a ulp apart are the same raster"
        );
    }

    /// Shared verification plan: `reraster_pending_coalesces_and_fires_once`.
    ///
    /// Pure schedule coalescing (no raylib): while a job is in flight, the
    /// per-frame policy's re-raster requests become the tab's *latest
    /// pending* (each newer crossing replaces the older one — never a
    /// queue), the pending fires at most once when the in-flight outcome
    /// is applied, and a crossing within the hysteresis band produces no
    /// request at all.
    #[test]
    fn reraster_pending_coalesces_and_fires_once() {
        let (svg_w, svg_h) = (8.0, 8.0);
        let (content_w, content_h) = (800.0, 564.0);

        // The show lands as the in-flight job (scale 88.125, kind Show —
        // the authoritative raster, not a re-raster).
        let mut s = RasterSlot::default();
        let show = RasterJob {
            id: "t".into(),
            generation: 1,
            svg: "<svg/>".into(),
            scale: 88.125,
            kind: RasterJobKind::Show,
        };
        let issued = s.request(show.clone());
        assert_eq!(issued.len(), 1, "the show's job is issued");

        // While the show is in flight, a settled zoom-3.0 crossing
        // (e = 211.5, d = 2.4) requests a re-raster at 264.375 — it
        // cannot be issued and becomes the pending.
        let e3 = display_scale(content_w, content_h, svg_w, svg_h, 3.0);
        let t3 = reraster_decision(e3, 10.0, 88.125, Some(88.125), svg_w, svg_h);
        assert_eq!(t3, Some(264.375), "the zoom-3.0 crossing is warranted");
        let pending1 = s.request(RasterJob {
            id: "t".into(),
            generation: 1,
            svg: "<svg/>".into(),
            scale: t3.expect("warranted above"),
            kind: RasterJobKind::Reraster,
        });
        assert!(
            pending1.is_empty(),
            "a request behind an in-flight job is not issued"
        );
        assert_eq!(
            s.pending.as_ref().map(|j| j.scale),
            Some(264.375),
            "the crossing is held as the pending"
        );

        // A newer, steeper settling (zoom 6.0) *replaces* the pending
        // (coalescing: the latest crossing wins, no queue): e = 423, target
        // clamped to 512.0.
        let e6 = display_scale(content_w, content_h, svg_w, svg_h, 6.0);
        let t6 = reraster_decision(e6, 10.0, 88.125, Some(88.125), svg_w, svg_h);
        assert_eq!(
            t6,
            Some(512.0),
            "the zoom-6.0 crossing clamps to the cap's side"
        );
        assert!(
            s.request(RasterJob {
                id: "t".into(),
                generation: 1,
                svg: "<svg/>".into(),
                scale: t6.expect("warranted above"),
                kind: RasterJobKind::Reraster,
            })
            .is_empty(),
            "the newer crossing is not issued while the show is in flight"
        );
        assert_eq!(
            s.pending.as_ref().map(|j| j.scale),
            Some(512.0),
            "the newer crossing replaced the older pending (no queue)"
        );

        // The show's outcome lands: exactly the latest pending fires (at
        // most once — the older 264.375 crossing is gone).
        let fired = s.complete();
        assert_eq!(fired.len(), 1, "the landing fires exactly one job");
        assert_eq!(
            fired[0].scale, 512.0,
            "the fired job is the latest crossing"
        );
        assert_eq!(
            fired[0].kind,
            RasterJobKind::Reraster,
            "the fired job is a re-raster"
        );
        assert_eq!(
            s.inflight.as_ref().map(|j| j.scale),
            Some(512.0),
            "the fired job is now the in-flight job"
        );

        // The fired re-raster lands: nothing is pending, nothing fires
        // (no double fire of the same crossing).
        assert!(
            s.complete().is_empty(),
            "no second fire of the coalesced crossing"
        );
        assert!(
            s.inflight.is_none() && s.pending.is_none(),
            "the schedule is empty"
        );

        // A crossing *within* the band (zoom 6.0: e = 423 against the
        // landed cap-side slot 512.0 — d = 512/423 ≈ 1.21) is no request
        // at all — there is nothing to coalesce.
        let e_mid = display_scale(content_w, content_h, svg_w, svg_h, 6.0);
        let t_mid = reraster_decision(e_mid, 10.0, 512.0, None, svg_w, svg_h);
        assert!(t_mid.is_none(), "an in-band deviation produces no request");
    }

    /// Shared verification plan: `worker_loop_rasterizes_jobs`.
    ///
    /// A worker thread fed two jobs over the plain-data channel rasterizes
    /// both off the calling thread and reports both outcomes in order: a
    /// 10×20 SVG at scale 2.0 comes back as an `Ok` outcome carrying
    /// 20×40 straight RGBA8 data, and a malformed SVG comes back as an
    /// `Err(Error::Rasterize)` outcome — reported, never panicked.
    #[test]
    fn worker_loop_rasterizes_jobs() {
        let (jobs_tx, jobs_rx) = mpsc::channel::<RasterJob>();
        let (results_tx, results_rx) = mpsc::channel::<RasterOutcome>();
        let stopped = Arc::new(AtomicBool::new(false));

        let worker = Builder::new()
            .name("mermaid-viewer-raster-test".into())
            .spawn(move || worker_loop(jobs_rx, results_tx, stopped))
            .expect("the worker thread spawns");

        jobs_tx
            .send(RasterJob {
                id: "a".into(),
                generation: 1,
                svg: r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"><rect width="10" height="20" fill="#123456"/></svg>"##
                    .into(),
                scale: 2.0,
                kind: RasterJobKind::Activation,
            })
            .expect("the job channel is open");
        jobs_tx
            .send(RasterJob {
                id: "b".into(),
                generation: 2,
                svg: "<svg".into(),
                scale: 2.0,
                kind: RasterJobKind::Activation,
            })
            .expect("the job channel is open");
        drop(jobs_tx);

        let first = results_rx
            .recv()
            .expect("the worker delivers the first outcome");
        assert_eq!(first.id, "a");
        assert_eq!(first.generation, 1);
        let (rgba, w, h) = first.result.expect("the 10×20 SVG rasterizes");
        assert_eq!((w, h), (20, 40), "10×20 at scale 2.0 rasters to 20×40");
        assert_eq!(rgba.len(), 20 * 40 * 4, "straight RGBA8: len = w·h·4");

        let second = results_rx
            .recv()
            .expect("the worker delivers the second outcome");
        assert_eq!(second.id, "b");
        assert_eq!(second.generation, 2);
        let err = second
            .result
            .expect_err("a malformed SVG is a typed Rasterize outcome, never a panic");
        assert!(
            matches!(err, Error::Rasterize(_)),
            "the raster failure is a typed Rasterize error, got: {err:?}"
        );

        worker
            .join()
            .expect("the worker exits cleanly on a disconnected job channel");
    }

    /// Shared verification plan: `worker_loop_stops_on_disconnect_and_flag`.
    ///
    /// The worker returns on the job channel disconnecting (the GUI thread
    /// is gone; no more jobs will arrive) and on the shared `stopped` flag
    /// (an idle worker observes it within its poll interval) — in both
    /// cases it exits, never hangs.
    #[test]
    fn worker_loop_stops_on_disconnect_and_flag() {
        // (1) A disconnected job channel: the worker returns immediately.
        {
            let (tx, rx) = mpsc::channel::<RasterJob>();
            let (results_tx, _results_rx) = mpsc::channel::<RasterOutcome>();
            let stopped = Arc::new(AtomicBool::new(false));
            let worker = Builder::new()
                .name("mermaid-viewer-raster-disconnect".into())
                .spawn(move || worker_loop(rx, results_tx, stopped))
                .expect("the worker thread spawns");
            drop(tx);
            worker
                .join()
                .expect("a disconnected job channel ends the worker");
        }

        // (2) The `stopped` flag: an idle worker observes it within the
        // poll interval and returns (the job channel stays open, so only
        // the flag can end the loop here).
        {
            let (_tx, rx) = mpsc::channel::<RasterJob>();
            let (results_tx, _results_rx) = mpsc::channel::<RasterOutcome>();
            let stopped = Arc::new(AtomicBool::new(false));
            let stopped_shared = Arc::clone(&stopped);
            let worker = Builder::new()
                .name("mermaid-viewer-raster-stopped".into())
                .spawn(move || worker_loop(rx, results_tx, stopped_shared))
                .expect("the worker thread spawns");
            // Let the worker reach its idle loop before raising the flag
            // (the flag is also checked before the first poll, so this
            // only guards the "observed while idle" path).
            std::thread::sleep(Duration::from_millis(300));
            stopped.store(true, Ordering::Release);
            worker.join().expect("the stopped flag ends an idle worker");
        }
    }

    /// Shared verification plan: `coalescing_bounds_one_inflight_plus_pending`.
    ///
    /// Pure schedule logic (no raylib, runs under every feature pass): one
    /// tab's on-demand raster schedule keeps *at most one in-flight job and
    /// one latest pending request* — a request that arrives while a job is
    /// in flight becomes the pending (a newer request replaces the older
    /// one; it is never a queue) — and a pending *fires*, becoming the
    /// in-flight job to be sent to the worker, exactly when the in-flight
    /// outcome is applied: at most once, and never by an outcome the
    /// schedule no longer carries.
    #[test]
    fn coalescing_bounds_one_inflight_plus_pending() {
        fn job(generation: u64) -> RasterJob {
            RasterJob {
                id: "t".into(),
                generation,
                svg: format!("<svg gen={generation}>"),
                scale: 2.0,
                kind: RasterJobKind::Activation,
            }
        }

        // (1) An empty schedule issues the request immediately: it becomes
        // the in-flight job (returned for the worker).
        let mut s = RasterSlot::default();
        let issued = s.request(job(1));
        assert_eq!(
            issued.len(),
            1,
            "a request with nothing in flight is issued"
        );
        assert_eq!(issued[0].generation, 1);
        assert_eq!(s.inflight.as_ref().map(|j| j.generation), Some(1));
        assert!(
            s.pending.is_none(),
            "nothing is pending until a request is refused"
        );

        // (2) A request while one is in flight stays pending (not issued)…
        assert!(
            s.request(job(2)).is_empty(),
            "a request behind an in-flight job is not issued"
        );
        assert_eq!(s.pending.as_ref().map(|j| j.generation), Some(2));
        // …and a newer request replaces it: the pending is the *latest*
        // request, never a queue (the bound at every point: ≤1 in-flight
        // + ≤1 pending).
        assert!(
            s.request(job(3)).is_empty(),
            "a second pending is not issued"
        );
        assert_eq!(
            s.pending.as_ref().map(|j| j.generation),
            Some(3),
            "the pending is the latest request (the older one is superseded)"
        );
        assert_eq!(s.inflight.as_ref().map(|j| j.generation), Some(1));

        // (3) The in-flight outcome is applied: the pending fires *exactly
        // once* — it becomes the new in-flight job, returned for the
        // worker — and is consumed.
        let fired = s.complete();
        assert_eq!(
            fired.len(),
            1,
            "the pending fires when the in-flight outcome lands"
        );
        assert_eq!(fired[0].generation, 3, "the *latest* pending fires");
        assert_eq!(s.inflight.as_ref().map(|j| j.generation), Some(3));
        assert!(s.pending.is_none(), "the fired pending is consumed");

        // (4) Fire-at-most-once: with no pending, a landing fires nothing.
        assert!(
            s.complete().is_empty(),
            "without a pending, a landing fires nothing"
        );
        assert!(s.inflight.is_none(), "the empty schedule is cleared");

        // (5) Losing the active drops only the pending: the in-flight job
        // lives on (its outcome may land and be discarded while the tab is
        // inactive), so a later landing still fires nothing pending.
        let mut s = RasterSlot::default();
        assert_eq!(s.request(job(1)).len(), 1);
        assert!(s.request(job(2)).is_empty()); // pending
        s.drop_pending();
        assert!(s.pending.is_none(), "drop_pending clears the pending");
        assert_eq!(
            s.inflight.as_ref().map(|j| j.generation),
            Some(1),
            "the in-flight job is kept"
        );
        assert!(
            s.complete().is_empty(),
            "the kept in-flight job fires nothing pending"
        );
    }
}
