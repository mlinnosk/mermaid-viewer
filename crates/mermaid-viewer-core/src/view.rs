//! Long-lived raylib diagram window: blocking and controller modes.
//!
//! This module is the `view` contract of `mermaid-viewer-core`:
//!
//! - [`run_blocking`] — a diagram window driven on the *current* thread, for
//!   the CLI `view` subcommand. Returns when the window is closed.
//! - [`WindowController`] — a dedicated, spawned GUI thread that owns the
//!   raylib window and is driven over a plain-data channel; for the MCP
//!   server, whose main thread runs a tokio runtime.
//!
//! ## Thread model
//!
//! Raylib initialization is single per process and its `RaylibThread` token
//! is `!Send`, so every real window lives on exactly one dedicated OS thread.
//! [`WindowController`] (which is `Send + Sync`) talks to that thread only
//! over a `std::sync::mpsc` channel carrying plain data (PNG bytes +
//! commands), with the open/closed state kept in an `AtomicBool`.
//!
//! ## State machine
//!
//! The real and the fake backend are observable-identical:
//!
//! ```text
//! CLOSED --update--> OPEN     (first update opens the window)
//! OPEN   --update--> OPEN     (texture is hot-swapped)
//! any    --close  --> CLOSED  (idempotent)
//! ```
//!
//! `MERMAID_VIEWER_FAKE_WINDOW=1` selects the fake backend at
//! [`WindowController::spawn`]: no display, no raylib init, state only — so
//! the full controller path is testable in CI without a display.
//!
//! ## Window features
//!
//! Fit-to-window initial scale (recomputed every frame, so resizing re-fits),
//! mouse-wheel zoom, left-button drag pan, texture hot-swap on update, close
//! on ESC or window close, and re-openable after close (raylib re-initializes
//! on the same thread).
//!
//! Window input is raster **PNG** only: an [`ImageFormat::Svg`]
//! [`RenderedImage`] is rejected with [`Error::View`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
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
use raylib::prelude::{
    Color, Image, KeyboardKey, MouseButton, PixelFormat, RaylibDraw, RaylibHandle, RaylibThread,
    Texture2D, Vector2,
};

/// Default initial window size.
const WINDOW_WIDTH: i32 = 800;
const WINDOW_HEIGHT: i32 = 600;

/// Wheel-zoom factor per wheel notch.
const ZOOM_STEP: f64 = 1.2;
const MIN_ZOOM: f64 = 0.05;
const MAX_ZOOM: f64 = 128.0;

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

/// Draws one frame of the diagram window into the active raylib framebuffer:
/// clears the background and draws `texture` with the fit/zoom/pan layout.
///
/// This is the single draw routine of the window — the same code runs in
/// [`run_blocking`], in the [`WindowController`] GUI thread, and in the
/// headless pixel tests (with `view-headless`).
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
    let layout = compute_layout(
        win_w as f64,
        win_h as f64,
        image.width as f64,
        image.height as f64,
        zoom,
        pan_x,
        pan_y,
    );
    d.clear_background(BACKGROUND);
    d.draw_texture_ex(
        texture,
        Vector2 {
            x: layout.x as f32,
            y: layout.y as f32,
        },
        0.0,
        layout.scale as f32,
        Color::WHITE,
    );
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

/// The open window: its content texture plus the interactive view state
/// (zoom/pan). Lives only on the thread that owns the raylib handle.
struct Window {
    texture: Texture2D,
    /// Current content (kept for the drawn dimensions).
    image: RenderedImage,
    /// Zoom multiplier on top of the fit scale (`1.0` = exact fit).
    zoom: f64,
    /// User pan offset in window pixels.
    pan_x: f64,
    pan_y: f64,
}

impl Window {
    /// Initializes raylib on the *current* thread, opens the window titled
    /// `title`, and uploads `image` as the initial texture.
    fn open(title: &str, image: &RenderedImage) -> Result<(RaylibHandle, RaylibThread, Self)> {
        ensure_png(image)?;
        let (mut rl, thread) = raylib::init()
            .size(WINDOW_WIDTH, WINDOW_HEIGHT)
            .title(title)
            .resizable()
            .vsync()
            .log_level(raylib_log_level())
            .build();
        let texture = load_window_texture(&mut rl, &thread, image)?;
        Ok((
            rl,
            thread,
            Window {
                texture,
                image: image.clone(),
                zoom: 1.0,
                pan_x: 0.0,
                pan_y: 0.0,
            },
        ))
    }

    /// Hot-swaps the window content: new texture, zoom/pan reset so the new
    /// diagram re-fits. The previous texture is unloaded when replaced.
    fn swap_content(
        &mut self,
        rl: &mut RaylibHandle,
        thread: &RaylibThread,
        image: &RenderedImage,
    ) -> Result<()> {
        let texture = load_window_texture(rl, thread, image)?;
        self.texture = texture;
        self.image = image.clone();
        self.zoom = 1.0;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
        Ok(())
    }

    /// Pumps input, applies zoom/pan, and draws one frame.
    ///
    /// Returns `false` when the window should close (ESC pressed or the
    /// window-close requested by the OS).
    fn step(&mut self, rl: &mut RaylibHandle, thread: &RaylibThread) -> bool {
        // Input events are polled by raylib inside `EndDrawing` (the draw
        // handle's drop each frame), which is the standard event pump.
        if rl.window_should_close() || rl.is_key_pressed(KeyboardKey::KEY_ESCAPE) {
            return false;
        }

        let wheel = rl.get_mouse_wheel_move();
        if wheel != 0.0 {
            self.zoom *= if wheel > 0.0 {
                ZOOM_STEP
            } else {
                1.0 / ZOOM_STEP
            };
            self.zoom = self.zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        }
        if rl.is_mouse_button_down(MouseButton::MOUSE_BUTTON_LEFT) {
            let delta = rl.get_mouse_delta();
            self.pan_x += delta.x as f64;
            self.pan_y += delta.y as f64;
        }

        let sw = rl.get_screen_width();
        let sh = rl.get_screen_height();
        let mut d = rl.begin_drawing(thread);
        draw_frame(
            &mut d,
            &self.texture,
            sw,
            sh,
            &self.image,
            self.zoom,
            self.pan_x,
            self.pan_y,
        );
        true
    }
}

// ---------------------------------------------------------------------------
// Public contract
// ---------------------------------------------------------------------------

/// Shows `image` (PNG) in a GUI window titled `title`, on the *current*
/// thread. Blocks until the window is closed (ESC or window close), then
/// returns.
///
/// With `MERMAID_VIEWER_FAKE_WINDOW=1` this validates the image and returns
/// immediately (no display, no raylib init).
///
/// SVG input is rejected with [`Error::View`].
pub fn run_blocking(title: &str, image: &RenderedImage) -> Result<()> {
    if fake_window_enabled() {
        return ensure_png(image);
    }
    let (mut rl, thread, mut window) = Window::open(title, image)?;
    while window.step(&mut rl, &thread) {}
    Ok(())
}

/// Commands to the GUI thread. Plain data only: nothing that is `!Send`
/// ever crosses the channel (the `RaylibThread` token stays on its thread).
enum Cmd {
    /// Open (or re-open) the window with new content. `ack` reports whether
    /// the window actually opened (window/GL init can fail at runtime).
    Open {
        title: String,
        image: RenderedImage,
        ack: Sender<std::result::Result<(), String>>,
    },
    /// Hot-swap the content of the open window.
    Update { image: RenderedImage },
    /// Close the window (idempotent).
    Close,
}

/// A long-lived diagram window driven from any thread.
///
/// `Send + Sync`: the raylib window lives on a dedicated OS thread; this
/// handle talks to it over a plain-data channel. The state machine is
/// `CLOSED --update--> OPEN --update--> OPEN`, `any --close--> CLOSED`, and
/// the window is re-openable after close.
///
/// With `MERMAID_VIEWER_FAKE_WINDOW=1` a fake backend with the identical
/// observable state is used (no display, no raylib init).
pub struct WindowController {
    inner: Inner,
}

enum Inner {
    /// Real backend: dedicated GUI thread + channel.
    Real {
        tx: Sender<Cmd>,
        open: Arc<AtomicBool>,
        stopped: Arc<AtomicBool>,
        title: String,
        thread: Option<JoinHandle<()>>,
    },
    /// Fake backend: the state machine only.
    Fake { open: AtomicBool },
}

impl WindowController {
    /// Spawns the controller for a window titled `title` (initially CLOSED).
    ///
    /// The backend is chosen from `MERMAID_VIEWER_FAKE_WINDOW` at this call.
    pub fn spawn(title: &str) -> Result<Self> {
        if fake_window_enabled() {
            return Ok(WindowController {
                inner: Inner::Fake {
                    open: AtomicBool::new(false),
                },
            });
        }
        let (tx, rx) = mpsc::channel();
        let open = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let (open_shared, stopped_shared) = (Arc::clone(&open), Arc::clone(&stopped));
        let thread = Builder::new()
            .name("mermaid-viewer-gui".into())
            .spawn(move || gui_loop(rx, open_shared, stopped_shared))
            .map_err(|e| Error::View(format!("failed to spawn GUI thread: {e}")))?;
        Ok(WindowController {
            inner: Inner::Real {
                tx,
                open,
                stopped,
                title: title.to_string(),
                thread: Some(thread),
            },
        })
    }

    /// Shows `image` (PNG) in the window: opens it when CLOSED, hot-swaps
    /// the content when OPEN.
    ///
    /// The first call on a CLOSED window blocks until the window has
    /// actually opened on the GUI thread (which acks back); later calls
    /// return as soon as the message is queued.
    pub fn update(&self, image: &RenderedImage) -> Result<()> {
        ensure_png(image)?;
        match &self.inner {
            Inner::Fake { open } => {
                open.store(true, Ordering::Release);
                Ok(())
            }
            Inner::Real {
                tx, open, title, ..
            } => {
                if open.load(Ordering::Acquire) {
                    tx.send(Cmd::Update {
                        image: image.clone(),
                    })
                    .map_err(|_| Error::View("window GUI thread is no longer running".to_string()))
                } else {
                    let (ack_tx, ack_rx) = mpsc::channel();
                    tx.send(Cmd::Open {
                        title: title.clone(),
                        image: image.clone(),
                        ack: ack_tx,
                    })
                    .map_err(|_| {
                        Error::View("window GUI thread is no longer running".to_string())
                    })?;
                    match ack_rx.recv() {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(msg)) => Err(Error::View(msg)),
                        Err(_) => Err(Error::View(
                            "window GUI thread exited before the window opened".to_string(),
                        )),
                    }
                }
            }
        }
    }

    /// Closes the window. Idempotent: closing a closed window is a no-op.
    pub fn close(&self) {
        match &self.inner {
            Inner::Fake { open } => open.store(false, Ordering::Release),
            Inner::Real { tx, .. } => {
                let _ = tx.send(Cmd::Close);
            }
        }
    }

    /// Whether the window is currently open.
    pub fn is_open(&self) -> bool {
        match &self.inner {
            Inner::Fake { open } => open.load(Ordering::Acquire),
            Inner::Real { open, .. } => open.load(Ordering::Acquire),
        }
    }
}

impl Drop for WindowController {
    fn drop(&mut self) {
        if let Inner::Real {
            stopped, thread, ..
        } = &mut self.inner
        {
            // `std::sync::mpsc` has no injected-shutdown sender, so the
            // controller tells the GUI thread to stop via the flag (observed
            // at most 200 ms later on an idle thread, next frame while open);
            // then joins it so the raylib window is fully closed before this
            // handle is gone.
            stopped.store(true, Ordering::Release);
            if let Some(h) = thread.take() {
                let _ = h.join();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GUI thread (owns the raylib handle for its whole lifetime)
// ---------------------------------------------------------------------------

/// Body of the dedicated GUI thread.
///
/// Loops forever: while CLOSED it waits on the channel (a `Close` there is
/// an idempotent no-op, an `Update` that races a just-closed window is
/// discarded — the controller's next update re-opens with fresh content, a
/// disconnected channel or a stopped flag means the controller is gone);
/// while OPEN it drains the channel every frame (non-blocking, so events
/// keep pumping). The thread outlives `close` so the window can be
/// re-opened: raylib re-initializes on this same thread after `CloseWindow`.
///
/// The wait uses a timeout so the thread can observe `stopped` even while
/// idle (the controller's `Drop` sets it; `std::sync::mpsc` has no way to
/// inject shutdown into a blocked `recv`).
fn gui_loop(rx: Receiver<Cmd>, open: Arc<AtomicBool>, stopped: Arc<AtomicBool>) {
    loop {
        // CLOSED: wait until an Open arrives.
        let (title, image, ack) = loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Cmd::Open { title, image, ack }) => break (title, image, ack),
                Ok(Cmd::Close) | Ok(Cmd::Update { .. }) => continue,
                Err(RecvTimeoutError::Timeout) => {
                    if stopped.load(Ordering::Acquire) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };

        let (mut rl, thread, mut window) = match Window::open(&title, &image) {
            Ok(w) => w,
            Err(e) => {
                let _ = ack.send(Err(e.to_string()));
                continue;
            }
        };

        open.store(true, Ordering::Release);
        let _ = ack.send(Ok(()));

        let mut closed = false;
        while !closed {
            // A dropped controller stops the thread on the next frame.
            if stopped.load(Ordering::Acquire) {
                break;
            }
            // Drain pending commands before drawing this frame so a queued
            // update is never painted over by a stale frame.
            match rx.try_recv() {
                Ok(Cmd::Update { image }) => {
                    if let Err(e) = window.swap_content(&mut rl, &thread, &image) {
                        // The window stays up; report to stderr, never panic.
                        eprintln!("mermaid-viewer: texture update failed: {e}");
                    }
                }
                Ok(Cmd::Close) | Err(TryRecvError::Disconnected) => closed = true,
                Ok(Cmd::Open { .. }) => {
                    // Unreachable: `update` sends Open only while !open.
                }
                Err(TryRecvError::Empty) => {}
            }

            if !closed && !window.step(&mut rl, &thread) {
                closed = true;
            }
        }

        open.store(false, Ordering::Release);
        // Drop `window` (unloads the texture) and `rl` (CloseWindow) before
        // looping back to wait for the next Open.
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

    /// Shared verification plan: `fake_window_lifecycle`.
    ///
    /// With the fake env var and a spawned controller:
    /// update/update/close/update → `is_open` true, true, false, true. The
    /// fake backend never touches raylib, so no display and no GL context is
    /// initialized for any of it (there is no window in this process at all).
    #[test]
    fn fake_window_lifecycle() {
        std::env::set_var(FAKE_WINDOW_ENV, "1");

        let image = test_png(4, 4, [10, 20, 30, 255]);
        let controller = WindowController::spawn("lifecycle").expect("fake spawn");
        assert!(!controller.is_open(), "spawned controller starts CLOSED");

        controller
            .update(&image)
            .expect("first update opens the window");
        assert!(controller.is_open(), "update → OPEN");

        controller.update(&image).expect("second update hot-swaps");
        assert!(controller.is_open(), "update while OPEN stays OPEN");

        controller.close();
        assert!(!controller.is_open(), "close → CLOSED");

        controller
            .update(&image)
            .expect("update after close re-opens");
        assert!(controller.is_open(), "re-open works");

        controller.close();
        controller.close();
        assert!(!controller.is_open(), "close is idempotent");
    }

    /// Shared verification plan: `view_rejects_svg_image`.
    ///
    /// An SVG [`RenderedImage`] is rejected with [`Error::View`] by
    /// `update` — in both the CLOSED and OPEN states — and by
    /// [`run_blocking`], under the fake backend.
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
            matches!(controller.update(&svg), Err(Error::View(_))),
            "SVG rejected while CLOSED"
        );
        assert!(
            !controller.is_open(),
            "rejected update did not open the window"
        );

        controller.update(&png).expect("PNG opens the window");
        assert!(
            matches!(controller.update(&svg), Err(Error::View(_))),
            "SVG rejected while OPEN"
        );
        assert!(controller.is_open(), "window stays open after rejection");

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
