//! Headless pixel tests for the diagram window.
//!
//! This file compiles to **empty** unless the `view-headless` feature is
//! enabled, and then drives the window's real draw path through raylib's
//! software-renderer test harness (the CI pass runs:
//! `cargo test -p mermaid-viewer-core --no-default-features --features view-headless`).
//!
//! The window module's texture upload and frame draw routines
//! (`mermaid_viewer_core::view::{load_window_texture, draw_frame,
//! draw_tabbed_frame, Window}`) are the exact code the real window runs — the
//! pixel assertions below validate window init, texture upload, fit layout,
//! the single-diagram frame's output, and the tabbed controller frame (tab
//! bar + active tab's content).
//!
//! Raylib is single-init per process and the harness enforces it, so the
//! tests serialize on a global lock (each test owns the init for its body).

#![cfg(feature = "view-headless")]

use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use mermaid_viewer_core::error::Error;
use mermaid_viewer_core::render::{ImageFormat, RenderedImage};
use mermaid_viewer_core::view::{
    draw_frame, draw_tabbed_frame, load_window_texture, RasterJob, RasterJobKind, Window,
    WindowController, BACKGROUND,
};
use raylib_sw::prelude::*;
use raylib_sw::test_harness::{assert_pixel, pixel_at, render_frame, with_headless};

/// Serializes the tests: raylib initialization is single per process and
/// `with_headless` panics on a second init in the same process.
static HEADLESS_INIT_LOCK: Mutex<()> = Mutex::new(());

/// The env var that selects the fake window backend (private in the view
/// module; mirrored here so the spawn/drop probe can force the real
/// backend).
const FAKE_WINDOW_ENV: &str = "MERMAID_VIEWER_FAKE_WINDOW";

/// The fixed height (window pixels) of the tab strip the controller window
/// draws across the top — `TAB_STRIP_HEIGHT` in the view module (private
/// there; mirrored here so the probes know the strip's geometry).
const TAB_STRIP: i32 = 36;

/// The maximum pixel count (`ceil(w·s) × ceil(h·s)`) the view module's
/// raster-scale policy allows a single raster to produce —
/// `MAX_RASTER_PIXELS` in the view module (private there; mirrored here so
/// the seam probe can check the cap).
const MAX_RASTER_PIXELS: u128 = 16_777_216;

/// A solid-color PNG (built with the same `png` encoder the renderer uses).
fn solid_png(width: u32, height: u32, rgba: [u8; 4]) -> RenderedImage {
    let bytes = {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("valid PNG header");
        // png 0.18's `write_image_data` takes the *entire* image buffer in
        // one call (per-row writing is not an API in 0.18).
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

/// A solid-color SVG document (a full-rect SVG in `width` × `height`
/// logical units): the show path's fixture, rasterized on demand by
/// `Window::show_tab`.
fn solid_svg(width: u32, height: u32, hex: &str) -> RenderedImage {
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

/// Renders one frame of the 8×8 solid-magenta diagram in an 800×600 window
/// at zoom 1 (the exact window code path: texture upload + fit layout +
/// clear + draw).
fn render_magenta_frame(rl: &mut RaylibHandle, thread: &RaylibThread) -> Image {
    let image = solid_png(8, 8, [255, 0, 255, 255]);
    let texture = load_window_texture(rl, thread, &image).expect("window texture loads");
    render_frame(rl, thread, |d| {
        draw_frame(d, &texture, 800, 600, &image, 1.0, 0.0, 0.0);
    })
}

/// Shared verification plan: `headless_pixel_center`.
///
/// An 8×8 solid-magenta PNG through the headless harness gives a magenta
/// center pixel within tolerance. (Fitted into 800×600, the 8×8 image scales
/// to 600×600 at (100, 0); the window center (400, 300) lies inside it.)
#[test]
fn headless_pixel_center() {
    let _guard = HEADLESS_INIT_LOCK.lock().unwrap();
    with_headless(800, 600, |rl, thread| {
        let frame = render_magenta_frame(rl, thread);
        assert_pixel(&frame, 400, 300, Color::MAGENTA, 32);
    });
}

/// Shared verification plan: `headless_pixel_corners_clear`.
///
/// The same frame gives the clear (background) color at all four window
/// corners, proving the drawn image is the contained, centered fit rect and
/// nothing bleeds outside it.
#[test]
fn headless_pixel_corners_clear() {
    let _guard = HEADLESS_INIT_LOCK.lock().unwrap();
    with_headless(800, 600, |rl, thread| {
        let frame = render_magenta_frame(rl, thread);
        for (x, y) in [(0, 0), (799, 0), (0, 599), (799, 599)] {
            assert_pixel(&frame, x, y, BACKGROUND, 8);
        }
    });
}

/// Sends every acknowledgement the window has queued to its waiting caller
/// (standing in for the GUI loop's deferred-ack delivery — the tests never
/// run the GUI loop).
fn deliver_acks(window: &mut Window) {
    for (ack, result) in window.take_due_acks() {
        let _ = ack.send(result);
    }
}

/// Unpacks a `show_tab`/`set_active` result that carries exactly one
/// issued raster job — the fixtures' precondition: the tab's schedule has
/// nothing in flight, so the request is issued immediately.
fn sole_job(jobs: Vec<RasterJob>) -> RasterJob {
    let mut issued = jobs.into_iter();
    let first = issued.next().expect("the tab's schedule issued a job");
    assert!(issued.next().is_none(), "exactly one job is issued");
    first
}

/// Stages a show through [`Window::show_tab`] (the GUI-thread half: no
/// rasterization, no texture upload) and immediately applies a successful
/// raster outcome for it (the worker half, simulated with the worker's
/// exact primitive [`Window::rasterize_tab_svg`]), leaving the tab
/// committed with a live texture — the steady state the pixel tests want
/// to drive a frame through. The show's initial raster scale hint is 2.0
/// (a floor far below the fit target for these fixtures — ignored). The
/// tab's schedule is idle when the fixture runs, so the show's raster job
/// is issued immediately (one job, none pending).
fn staged_ready(
    rl: &mut RaylibHandle,
    thread: &RaylibThread,
    window: &mut Window,
    id: &str,
    label: &str,
    image: &RenderedImage,
) {
    let (ack_tx, ack_rx) = mpsc::channel();
    let job = sole_job(
        window
            .show_tab(rl, id, label, image, 2.0, ack_tx)
            .expect("the show stages (no raster on the GUI thread)"),
    );
    let result = Window::rasterize_tab_svg(image, job.scale);
    window.apply_raster_result(rl, thread, &job.id, job.generation, result);
    deliver_acks(window);
    ack_rx
        .recv()
        .expect("the staged show is acked exactly once")
        .expect("the commit is Ok");
}

/// A tabbed window with two solid-color tabs: "red" (a red SVG) shown
/// first, then "blue" (a blue SVG) — the last shown tab is the active one.
/// Goes through the same `Window::show_tab` / `Window::apply_raster_result`
/// seams the MCP controller path uses (stage, then the worker's outcome).
fn two_tab_window(rl: &mut RaylibHandle, thread: &RaylibThread) -> Window {
    let mut window = Window::new(0);
    staged_ready(
        rl,
        thread,
        &mut window,
        "red",
        "Red",
        &solid_svg(8, 8, "#ff0000"),
    );
    staged_ready(
        rl,
        thread,
        &mut window,
        "blue",
        "Blue",
        &solid_svg(8, 8, "#0000ff"),
    );
    window
}

/// Shared verification plan: `headless_tabbed_frame_active_content`.
///
/// A tabbed window with two differently colored tabs draws the *active* tab
/// into the content area below the tab strip, and switching the active tab
/// switches the drawn pixels: showing the other tab's ID again (a staged
/// replace + activate — its old content stays live until the commit) and
/// applying its raster outcome re-targets the frame at the same probe
/// point. The probe (400, 318) is the center of the 800×564 content area;
/// the fitted 8×8 image (scaled to 564×564 at (118, 36)) covers it.
#[test]
fn headless_tabbed_frame_active_content() {
    let _guard = HEADLESS_INIT_LOCK.lock().unwrap();
    with_headless(800, 600, |rl, thread| {
        let mut window = two_tab_window(rl, thread);
        let mut ui_closes = Vec::new();

        // "blue" was shown last, so it is active: the content pixel is blue.
        let frame = render_frame(rl, thread, |d| {
            assert!(
                draw_tabbed_frame(d, &mut window, 800, 600, &mut ui_closes, &mut Vec::new()),
                "tabs remain: the frame keeps the window open"
            );
        });
        assert_pixel(&frame, 400, 318, Color::BLUE, 32);

        // Re-show "red" (a staged replace + activate) and apply its
        // successful raster outcome: the same content pixel turns red —
        // the commit re-targets which tab is drawn.
        staged_ready(
            rl,
            thread,
            &mut window,
            "red",
            "Red",
            &solid_svg(8, 8, "#ff0000"),
        );
        let frame = render_frame(rl, thread, |d| {
            assert!(draw_tabbed_frame(
                d,
                &mut window,
                800,
                600,
                &mut ui_closes,
                &mut Vec::new()
            ));
        });
        assert_pixel(&frame, 400, 318, Color::RED, 32);

        // Headless frames get no mouse input: the bar requested no closes.
        assert!(
            ui_closes.is_empty(),
            "no user closes are produced by a headless frame"
        );
    });
}

/// Shared verification plan: `headless_tab_bar_renders`.
///
/// Count-based probe in the pattern of raylib's own `tests/render_gui.rs`:
/// pin `GuiState::STATE_NORMAL` first (so the probe does not depend on prior
/// style mutations), render the tabbed frame, and count the top strip's
/// pixels that differ from the RAYWHITE clear color — a blank frame would
/// count 0. The 800×`TAB_STRIP` strip holds the raygui tab bar (tab base,
/// borders, labels), so well more than a degenerate handful of its 28 800
/// pixels is drawn.
#[test]
fn headless_tab_bar_renders() {
    let _guard = HEADLESS_INIT_LOCK.lock().unwrap();
    with_headless(800, 600, |rl, thread| {
        // Pin a known state so the probe does not depend on prior style
        // mutations (raylib's render_gui.rs pattern).
        rl.gui_set_state(GuiState::STATE_NORMAL);

        let mut window = two_tab_window(rl, thread);
        let mut ui_closes = Vec::new();
        let frame = render_frame(rl, thread, |d| {
            assert!(draw_tabbed_frame(
                d,
                &mut window,
                800,
                600,
                &mut ui_closes,
                &mut Vec::new()
            ));
        });

        // The top strip must contain drawn raygui pixels: more than 16 off
        // the RAYWHITE clear color in any channel.
        let mut drawn = 0u32;
        for y in 0..TAB_STRIP {
            for x in 0..800 {
                let p = pixel_at(&frame, x, y);
                if p.r.abs_diff(BACKGROUND.r) > 16
                    || p.g.abs_diff(BACKGROUND.g) > 16
                    || p.b.abs_diff(BACKGROUND.b) > 16
                {
                    drawn += 1;
                }
            }
        }
        assert!(
            drawn > 100,
            "expected the tab bar to fill many strip pixels, got {drawn}"
        );
    });
}

/// Regression (argument order at the policy *call sites*): the pure policy
/// unit tests call `raster_target`/`raster_show_target` with the correct
/// order themselves, so they cannot see a swap at the window's call sites.
/// This test drives the exact production seams the MCP path uses —
/// `Window::show_tab` (which stages the show and computes its initial
/// raster target) and `Window::apply_raster_result` (which lands the
/// worker's raster into the live texture, recording the target's scale
/// with it) — and probes the resulting live raster, so the fit floor is
/// computed from the *content rect* and [`MAX_RASTER_PIXELS`] constrains
/// the *SVG's raster*, never the window dimensions:
///
/// - show of an 8×8 SVG in the 800×600 window (content 800×564): fit
///   `e = min(100, 70.5) = 70.5`, target `e × 1.25 = 88.125` — the hint
///   2.0 is a floor far below it and must be ignored (a swapped order
///   computes the fit from the 8×8 "content" against the 800×564 "svg"
///   and rasters the 8×8 SVG at the hint: 16×16);
/// - the same cap at the show: a 10000×10000 SVG at the hint 2.0 would be
///   4e8 px — the target must clamp the *raster* to
///   ≤ [`MAX_RASTER_PIXELS`] (a swapped order would cap the window
///   dimensions and attempt a multi-gigabyte allocation);
/// - a tab switch frees the outgoing tab's slot (the one-live-texture
///   invariant): the incoming tab draws the placeholder until its
///   on-demand activation raster lands — the switch issues that job,
///   sized by the same content-rect policy.
#[test]
fn seam_raster_target_argument_order() {
    let _guard = HEADLESS_INIT_LOCK.lock().unwrap();
    with_headless(800, 600, |rl, thread| {
        let mut window = Window::new(0);

        // (1) The show's initial raster: the fit × headroom floor, not the
        // hint, sizes the texture (the target's scale is recorded with the
        // texture when the outcome is applied).
        let image = solid_svg(8, 8, "#ff0000");
        let (ack_tx, ack_rx) = mpsc::channel();
        let job = sole_job(
            window
                .show_tab(rl, "a", "A", &image, 2.0, ack_tx)
                .expect("the show stages (no raster on the GUI thread)"),
        );
        assert!(
            (job.scale - 88.125).abs() < 1e-9,
            "the fit × headroom floor (70.5 × 1.25) must size the show raster, not the hint: {}",
            job.scale
        );
        window.apply_raster_result(rl, thread, &job.id, job.generation, {
            Window::rasterize_tab_svg(&image, job.scale)
        });
        deliver_acks(&mut window);
        ack_rx
            .recv()
            .expect("the commit is acked")
            .expect("it succeeds");
        let (scale, w, h) = window
            .active_raster()
            .expect("the active tab holds a live raster");
        assert!(
            (scale - 88.125).abs() < 1e-9,
            "the live texture was rasterized at the show's target: {scale}"
        );
        assert_eq!(
            (w, h),
            (705, 705),
            "8×8 at 88.125 rasters to 705×705 px: ({w}, {h})"
        );

        // (2) The pixel cap constrains the SVG's raster: an oversized SVG
        // cannot defeat it through the hint.
        let big = solid_svg(10_000, 10_000, "#00ff00");
        let (ack_tx, ack_rx) = mpsc::channel();
        let job = sole_job(
            window
                .show_tab(rl, "big", "Big", &big, 2.0, ack_tx)
                .expect("the oversized SVG stages within the pixel cap"),
        );
        let result = Window::rasterize_tab_svg(&big, job.scale);
        window.apply_raster_result(rl, thread, &job.id, job.generation, result);
        deliver_acks(&mut window);
        ack_rx
            .recv()
            .expect("exactly one success ack per show_tab, after its outcome applied")
            .expect("it succeeds");
        let (scale, w, h) = window
            .active_raster()
            .expect("the active tab holds a live raster");
        assert!(
            (w as u128) * (h as u128) <= MAX_RASTER_PIXELS,
            "the clamped raster {w}×{h} at scale {scale} must fit the pixel budget"
        );

        // (3) A tab switch frees the outgoing tab's slot (the
        // one-live-texture invariant) and issues the incoming tab's
        // on-demand activation raster: the incoming slot is Empty — it
        // draws the placeholder until the job's outcome lands. The job's
        // scale runs the same call-site policy probe: the content-rect
        // fit × headroom, never the hint.
        let jobs = window.set_active(0, 800.0, 564.0);
        let job = sole_job(jobs);
        assert!(
            (job.scale - 88.125).abs() < 1e-9,
            "the activation raster is the content-rect fit × headroom floor (70.5 × 1.25), not the hint: {}",
            job.scale
        );
        assert!(
            window.active_raster().is_none(),
            "the tab-bar switch freed the outgoing tab's slot"
        );
    });
}

/// A staged show (no raster outcome applied yet) draws the placeholder —
/// the tab's label centered in the content area — and *nothing else* of
/// the content: the slot is `Empty`, so no texture is drawn while the tab
/// is live and the worker's raster is in flight. The show is also
/// *unacked* until its outcome is applied (the deferred-ack contract).
#[test]
fn headless_placeholder_while_empty() {
    let _guard = HEADLESS_INIT_LOCK.lock().unwrap();
    with_headless(800, 600, |rl, thread| {
        let mut window = Window::new(0);
        // A red SVG: if the (buggy) path drew content before the outcome,
        // the probes below would see red.
        let image = solid_svg(8, 8, "#ff0000");
        let (ack_tx, ack_rx) = mpsc::channel();
        window
            .show_tab(rl, "a", "Placeholder probe", &image, 2.0, ack_tx)
            .expect("the show stages (the tab is live, its slot is Empty)");

        let frame = render_frame(rl, thread, |d| {
            assert!(
                draw_tabbed_frame(d, &mut window, 800, 600, &mut Vec::new(), &mut Vec::new()),
                "the staged tab keeps the window open"
            );
        });

        // The fitted 8×8 image would cover (400, 500) (the image rect is
        // 564×564 at (118, 36)); the probe is far from the label band, so
        // it must be the clear color — no content while staged.
        assert_pixel(&frame, 400, 500, BACKGROUND, 8);
        // So is a point above the content label.
        assert_pixel(&frame, 400, 100, BACKGROUND, 8);

        // The label band (y ≈ 308..328 for a 17-char label centered in the
        // 800×564 content area): the placeholder's text is drawn there.
        let mut text = 0u32;
        for y in 300..336 {
            for x in 200..600 {
                let p = pixel_at(&frame, x, y);
                if p.r.abs_diff(BACKGROUND.r) > 24
                    || p.g.abs_diff(BACKGROUND.g) > 24
                    || p.b.abs_diff(BACKGROUND.b) > 24
                {
                    text += 1;
                }
            }
        }
        assert!(
            text > 50,
            "the placeholder's label is drawn in the content area, got {text} text pixels"
        );

        // The show's acknowledgement defers to the outcome: it is not sent
        // while the slot is Empty.
        assert!(
            matches!(ack_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "the staged show is not acked before its outcome is applied"
        );
    });
}

/// Applying a *successful* raster outcome for a staged show commits it:
/// the content and label land, the texture is uploaded (the active tab's
/// slot goes `Empty` → `Ready`), the frame draws the rasterized content,
/// and the deferred acknowledgement is `Ok`.
#[test]
fn headless_apply_raster_result_draws_content() {
    let _guard = HEADLESS_INIT_LOCK.lock().unwrap();
    with_headless(800, 600, |rl, thread| {
        let mut window = Window::new(0);
        let image = solid_svg(8, 8, "#ff0000");
        let (ack_tx, ack_rx) = mpsc::channel();
        let job = sole_job(
            window
                .show_tab(rl, "a", "Content probe", &image, 2.0, ack_tx)
                .expect("the show stages"),
        );

        // Before the outcome: the content pixel is the clear color (the
        // placeholder covers the staged tab).
        let frame = render_frame(rl, thread, |d| {
            assert!(draw_tabbed_frame(
                d,
                &mut window,
                800,
                600,
                &mut Vec::new(),
                &mut Vec::new()
            ));
        });
        assert_pixel(&frame, 400, 500, BACKGROUND, 8);

        // Apply the worker's successful outcome for the staged show.
        let result = Window::rasterize_tab_svg(&image, job.scale);
        window.apply_raster_result(rl, thread, &job.id, job.generation, result);
        assert_eq!(
            window.ready_slot_count(),
            1,
            "the commit uploaded exactly one live texture"
        );
        deliver_acks(&mut window);
        ack_rx
            .recv()
            .expect("the commit is acked")
            .expect("the commit is Ok");

        // After the outcome: the same pixel is the rasterized content.
        let frame = render_frame(rl, thread, |d| {
            assert!(draw_tabbed_frame(
                d,
                &mut window,
                800,
                600,
                &mut Vec::new(),
                &mut Vec::new()
            ));
        });
        assert_pixel(&frame, 400, 500, Color::RED, 32);
    });
}

/// A *failed* raster outcome for a staged show rolls back and acks the
/// typed rasterization failure: a provisionally *added* tab (a new ID) is
/// removed again, while a *replaced* tab keeps its old content live (it
/// was never swapped out at staging) — and in both cases the show is
/// acked with the typed [`Error::Rasterize`], exactly once.
#[test]
fn show_raster_failure_acks_typed_error() {
    let _guard = HEADLESS_INIT_LOCK.lock().unwrap();
    with_headless(800, 600, |rl, thread| {
        let mut window = Window::new(0);

        // (1) A failed first raster removes the provisional tab.
        {
            let bad = RenderedImage {
                format: ImageFormat::Svg,
                width: 8,
                height: 8,
                bytes: b"<svg".to_vec(),
            };
            let (ack_tx, ack_rx) = mpsc::channel();
            // Staging succeeds: the malformed SVG is for the *worker* to
            // reject, not the GUI thread.
            let job = sole_job(
                window
                    .show_tab(rl, "bad", "Bad", &bad, 2.0, ack_tx)
                    .expect("a malformed SVG still stages (the worker rejects it)"),
            );
            assert_eq!(
                window.tab_count(),
                1,
                "the provisional tab is live while its raster is in flight"
            );
            window.apply_raster_result(
                rl,
                thread,
                &job.id,
                job.generation,
                Err(Error::Rasterize(
                    "malformed SVG (simulated worker failure)".to_string(),
                )),
            );
            deliver_acks(&mut window);
            let err = ack_rx
                .recv()
                .expect("the failure is acked")
                .expect_err("a failed raster is a typed Err");
            assert!(
                matches!(err, Error::Rasterize(_)),
                "the failure is the typed Rasterize error, got: {err:?}"
            );
            assert_eq!(
                window.tab_count(),
                0,
                "the provisionally added tab is removed by the rollback"
            );
        }

        // (2) A failed replace keeps the old content live.
        {
            let good = solid_svg(8, 8, "#ff0000");
            staged_ready(rl, thread, &mut window, "a", "Old", &good);
            let bad = RenderedImage {
                format: ImageFormat::Svg,
                width: 8,
                height: 8,
                bytes: b"<svg".to_vec(),
            };
            let (ack_tx, ack_rx) = mpsc::channel();
            let job = sole_job(
                window
                    .show_tab(rl, "a", "New", &bad, 2.0, ack_tx)
                    .expect("the replace stages (the old content stays live)"),
            );
            window.apply_raster_result(
                rl,
                thread,
                &job.id,
                job.generation,
                Err(Error::Rasterize(
                    "malformed SVG (simulated worker failure)".to_string(),
                )),
            );
            deliver_acks(&mut window);
            let err = ack_rx
                .recv()
                .expect("the failure is acked")
                .expect_err("a failed raster is a typed Err");
            assert!(
                matches!(err, Error::Rasterize(_)),
                "the failure is the typed Rasterize error, got: {err:?}"
            );
            assert_eq!(
                window.tab_count(),
                1,
                "the replace's rollback removed nothing"
            );
            // The old content is still drawn: the slot was never touched at
            // staging, and the rollback left it in place.
            let frame = render_frame(rl, thread, |d| {
                assert!(draw_tabbed_frame(
                    d,
                    &mut window,
                    800,
                    600,
                    &mut Vec::new(),
                    &mut Vec::new()
                ));
            });
            assert_pixel(&frame, 400, 500, Color::RED, 32);
        }
    });
}

/// Raster outcomes for *stale* states are discarded without effect: an
/// outcome for a removed tab (whose staged show was already acked at
/// close), an outcome for a superseded generation (whose staged show was
/// already acked at supersede), and an outcome for the *first* show of a
/// re-created tab (whose generation no live show carries — generations are
/// unique across tab recreations) change nothing and never double-ack —
/// while the *current* generation's outcome then commits as usual.
#[test]
fn raster_result_discards_stale_outcomes() {
    let _guard = HEADLESS_INIT_LOCK.lock().unwrap();
    with_headless(800, 600, |rl, thread| {
        let image = solid_svg(8, 8, "#00ff00");
        let mut window = Window::new(0);

        // (1) An outcome for a removed tab is discarded without effect.
        {
            let (ack_tx, ack_rx) = mpsc::channel();
            let job = sole_job(
                window
                    .show_tab(rl, "gone", "Gone", &image, 2.0, ack_tx)
                    .expect("the show stages"),
            );
            assert!(
                window.close_tab("gone"),
                "the tab is closed (its staged show is cancelled)"
            );
            deliver_acks(&mut window);
            let err = ack_rx
                .recv()
                .expect("the close acks the cancellation")
                .expect_err("a cancelled show is a typed Err");
            assert!(
                matches!(err, Error::View(_)),
                "the cancellation is a typed View error, got: {err:?}"
            );
            // The (late) raster outcome for the gone tab is a no-op…
            window.apply_raster_result(
                rl,
                thread,
                "gone",
                job.generation,
                Ok((vec![0u8; 4], 1, 1)),
            );
            // …and the show is never acked twice.
            assert!(
                ack_rx.try_recv().is_err(),
                "the discarded outcome produced no second acknowledgement"
            );
            assert_eq!(
                window.tab_count(),
                0,
                "the discarded outcome changed nothing"
            );
            assert_eq!(
                window.ready_slot_count(),
                0,
                "the discarded outcome uploaded nothing"
            );
        }

        // (2) An outcome for a superseded generation is discarded without
        // committing — but landing it completes the schedule: the second
        // show's job, which stayed the latest pending behind the in-flight
        // first one (the coalescing bound: nothing issued), fires and is
        // returned for the worker; its outcome then commits.
        {
            let (ack_tx, ack_rx) = mpsc::channel();
            let first = sole_job(
                window
                    .show_tab(rl, "s", "S1", &image, 2.0, ack_tx)
                    .expect("the first show stages"),
            );
            let (ack_tx2, ack_rx2) = mpsc::channel();
            let second_show = window
                .show_tab(rl, "s", "S2", &image, 2.0, ack_tx2)
                .expect("the second show supersedes the first");
            assert!(
                second_show.is_empty(),
                "the show's job stays the latest pending behind the in-flight one (nothing issued)"
            );
            deliver_acks(&mut window);
            let sup = ack_rx
                .recv()
                .expect("the superseded show is acked")
                .expect_err("the supersede is a typed Err");
            assert!(
                matches!(sup, Error::View(_)),
                "the supersede is a typed View error, got: {sup:?}"
            );
            // The stale first-generation outcome is discarded without
            // commit: no upload, no acknowledgement of the live show…
            let fired = window.apply_raster_result(
                rl,
                thread,
                "s",
                first.generation,
                Ok((vec![0u8; 4], 1, 1)),
            );
            assert_eq!(
                window.ready_slot_count(),
                0,
                "the stale outcome uploaded nothing"
            );
            assert!(
                matches!(ack_rx2.try_recv(), Err(mpsc::TryRecvError::Empty)),
                "the live show is not acked by the stale outcome"
            );
            // …but the landing fires the pending second show's job.
            let second = sole_job(fired);
            assert_eq!(
                second.generation,
                first.generation + 1,
                "the fired job is the superseding show's (the latest request)"
            );
            // …and the fired job's outcome commits.
            let result = Window::rasterize_tab_svg(&image, second.scale);
            assert!(
                window
                    .apply_raster_result(rl, thread, "s", second.generation, result)
                    .is_empty(),
                "nothing was pending behind the fired job"
            );
            assert_eq!(
                window.ready_slot_count(),
                1,
                "the commit uploaded the texture"
            );
            deliver_acks(&mut window);
            ack_rx2
                .recv()
                .expect("the current show is acked")
                .expect("it commits");
        }

        // (3) A re-created tab (same ID after a close) never re-matches a
        // stale outcome: the re-show's generation is unique (the window's
        // monotonic counter never re-issues the first show's generation),
        // so the first job's outcome — delivered *after* the re-show — is
        // discarded without committing old content, uploading, or
        // consuming the new show's acknowledgement.
        {
            let (ack_tx, ack_rx) = mpsc::channel();
            let first = sole_job(
                window
                    .show_tab(rl, "again", "Again1", &image, 2.0, ack_tx)
                    .expect("the first show stages"),
            );
            assert!(
                window.close_tab("again"),
                "the tab is closed (its staged show is cancelled)"
            );
            deliver_acks(&mut window);
            let err = ack_rx
                .recv()
                .expect("the close acks the cancellation")
                .expect_err("a cancelled show is a typed Err");
            assert!(
                matches!(err, Error::View(_)),
                "the cancellation is a typed View error, got: {err:?}"
            );

            let (ack_tx2, ack_rx2) = mpsc::channel();
            let second = sole_job(
                window
                    .show_tab(rl, "again", "Again2", &image, 2.0, ack_tx2)
                    .expect("the re-show stages (a fresh tab for the same ID)"),
            );
            assert_ne!(
                first.generation, second.generation,
                "a re-created tab's staged show carries a generation no prior outcome can match"
            );

            // The stale first job's outcome (a successful raster of the
            // old content) arrives after the re-show: it must be discarded
            // without effect — no commit of old content, no upload…
            // (The window holds two tabs here: "s" from case (2) plus the
            // re-created "again"; the discarded outcome must leave both.)
            window.apply_raster_result(
                rl,
                thread,
                "again",
                first.generation,
                Ok((vec![0u8; 4], 1, 1)),
            );
            assert_eq!(
                window.tab_count(),
                2,
                "the discarded outcome removed nothing (both tabs remain)"
            );
            assert_eq!(
                window.ready_slot_count(),
                0,
                "the stale outcome uploaded nothing"
            );
            assert!(
                matches!(ack_rx2.try_recv(), Err(mpsc::TryRecvError::Empty)),
                "the re-created show is not acked by the stale outcome"
            );

            // …and the new show's own outcome then commits and acks
            // exactly once.
            let result = Window::rasterize_tab_svg(&image, second.scale);
            window.apply_raster_result(rl, thread, "again", second.generation, result);
            assert_eq!(
                window.ready_slot_count(),
                1,
                "the new show's commit uploaded the texture"
            );
            deliver_acks(&mut window);
            ack_rx2
                .recv()
                .expect("the new show is acked")
                .expect("it commits");
            assert!(
                ack_rx2.try_recv().is_err(),
                "the new show is acked exactly once (the stale outcome consumed nothing)"
            );
        }
    });
}

/// Regression (window-cycle staleness): the raster worker and its result
/// channel outlive the window's open/close cycles — `gui_loop` re-opens
/// the window with a fresh `Window` *and* carries the previous window's
/// generation watermark into it. Without the carry, the re-opened window
/// would re-issue the closed window's generations and a queued outcome
/// from the old cycle (same tab ID, same window-local generation as the
/// new show) would match, commit stale content, and consume the new
/// show's acknowledgement.
///
/// Simulated through the exact production seams: close the first window
/// with an in-flight show, re-open with the carried watermark
/// (`Window::new(window.generation_watermark())` — what `gui_loop` does),
/// deliver the old outcome (it must be discarded without consuming the
/// new show's ack), and confirm the new show's own outcome commits and
/// acks exactly once.
#[test]
fn stale_outcome_from_prior_window_cycle_is_discarded() {
    let _guard = HEADLESS_INIT_LOCK.lock().unwrap();
    with_headless(800, 600, |rl, thread| {
        let image = solid_svg(8, 8, "#00ff00");

        // Window cycle 1 (the GUI loop's first window: watermark 0): a
        // show is in flight when the window closes.
        let mut window = Window::new(0);
        let (ack_tx, ack_rx) = mpsc::channel();
        let job = sole_job(
            window
                .show_tab(rl, "a", "A1", &image, 2.0, ack_tx)
                .expect("the cycle-1 show stages"),
        );
        assert_eq!(
            job.generation, 1,
            "the first show of the first window is generation 1"
        );
        // The window closes with the raster still in flight: the staged
        // show is cancelled and its acknowledgement is due (the GUI loop
        // delivers it at teardown, then drops the window).
        assert!(
            window.close_tab("a"),
            "closing the tab cancels its in-flight show"
        );
        deliver_acks(&mut window);
        // The GUI loop carries the window's watermark into the re-open:
        // the next window must never re-issue this window's generations.
        let watermark = window.generation_watermark();
        drop(window);

        // Window cycle 2: the re-opened window, seeded from the watermark
        // (exactly what `gui_loop` does after a window goes down).
        let mut window = Window::new(watermark);
        let (ack_tx2, ack_rx2) = mpsc::channel();
        let job2 = sole_job(
            window
                .show_tab(rl, "a", "A2", &image, 2.0, ack_tx2)
                .expect("the cycle-2 show stages (a fresh tab for the same ID)"),
        );
        assert_ne!(
            job2.generation, job.generation,
            "the re-opened window's show must not reuse a prior cycle's generation: \
             old was {}, new is {}",
            job.generation, job2.generation
        );

        // The old (cycle-1) outcome arrives on the surviving result
        // channel after the re-show: it must be discarded without
        // committing old content, uploading, or consuming the new show's
        // acknowledgement.
        window.apply_raster_result(rl, thread, "a", job.generation, Ok((vec![0u8; 4], 1, 1)));
        assert_eq!(window.tab_count(), 1, "the stale outcome removed nothing");
        assert_eq!(
            window.ready_slot_count(),
            0,
            "the stale outcome uploaded nothing (no commit of old content)"
        );
        assert!(
            matches!(ack_rx2.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "the new show is not acked by the stale outcome"
        );

        // The new show's own outcome commits and acks exactly once.
        let result = Window::rasterize_tab_svg(&image, job2.scale);
        window.apply_raster_result(rl, thread, "a", job2.generation, result);
        assert_eq!(
            window.ready_slot_count(),
            1,
            "the new show's commit uploaded the texture"
        );
        deliver_acks(&mut window);
        ack_rx2
            .recv()
            .expect("the new show is acked")
            .expect("it commits");
        assert!(
            ack_rx2.try_recv().is_err(),
            "the new show is acked exactly once (the stale outcome consumed nothing)"
        );

        // Cycle 1's show was acked exactly once — the cancellation at
        // close — and the stale outcome did not add a second one.
        let cancelled = ack_rx
            .recv()
            .expect("cycle 1's show is acked at close")
            .expect_err("a cancelled show is a typed Err");
        assert!(
            matches!(cancelled, Error::View(_)),
            "the cancellation is a typed View error, got: {cancelled:?}"
        );
        assert!(
            ack_rx.try_recv().is_err(),
            "cycle 1's show is acked exactly once"
        );
    });
}

/// Shared verification plan: `real_controller_spawn_and_drop_joins_threads`.
///
/// A real-backend controller spawns the GUI thread and its sibling raster
/// worker; dropping the controller stops both and joins them well inside
/// the shutdown grace period. No display and no raylib init are needed:
/// the real backend defers init to the first `show_tab`, and this test
/// never issues one — the GUI thread sits in its CLOSED wait loop and the
/// worker in its idle poll.
#[test]
fn real_controller_spawn_and_drop_joins_threads() {
    // Remove any leaked fake-backend env: this test must exercise the real
    // backend (the one that spawns threads to join) — the fake backend
    // would spawn none and make the probe vacuous.
    std::env::remove_var(FAKE_WINDOW_ENV);

    let started = std::time::Instant::now();
    let controller = WindowController::spawn("join-probe").expect("the real controller spawns");
    drop(controller);
    let elapsed = started.elapsed();

    // The view module's `GUI_SHUTDOWN_GRACE` is 5s (private there;
    // mirrored here): the GUI thread observes the stop flag on its next
    // frame/poll, the idle worker within its 200 ms poll — both bounded
    // joins land far inside the grace. A regression that leaves a thread
    // to the full grace (or deadlocks) fails this bound.
    assert!(
        elapsed < Duration::from_secs(5),
        "spawn + drop must return well inside the 5s shutdown grace: {elapsed:?}"
    );
}

/// Shared verification plan: `headless_one_live_texture_across_switches`.
///
/// The on-demand half of the one-live-texture invariant across tab-bar
/// switches: every switch frees the outgoing tab's texture, and the
/// incoming tab — while its slot is `Empty` and nothing in its schedule is
/// in flight — issues exactly one activation raster, sized by the same
/// content-rect fit × headroom policy the show path uses. Switching back
/// to a tab whose activation raster is *still in flight* issues nothing
/// (the in-flight job covers it — the coalescing bound: at most one
/// in-flight job plus one latest pending request per tab, fired at most
/// once). At every point in the test exactly one live texture exists
/// window-wide: the probe point (400, 318) tracks the active tab's color.
#[test]
fn headless_one_live_texture_across_switches() {
    let _guard = HEADLESS_INIT_LOCK.lock().unwrap();
    with_headless(800, 600, |rl, thread| {
        // "blue" was shown last, so it is active and live; "red"'s slot
        // was freed when "blue" was shown.
        let mut window = two_tab_window(rl, thread);
        assert_eq!(
            window.ready_slot_count(),
            1,
            "the active tab alone holds a live texture"
        );

        // Switch to "red": its slot is Empty and its schedule is idle, so
        // the switch issues its activation raster (the content rect is the
        // 800×600 window minus the 36-px tab strip)…
        let job_red = sole_job(window.set_active(0, 800.0, 564.0));
        assert_eq!(
            job_red.id, "red",
            "the activation raster belongs to the incoming tab"
        );
        assert!(
            (job_red.scale - 88.125).abs() < 1e-9,
            "the activation raster is the content-rect fit × headroom floor (70.5 × 1.25): {}",
            job_red.scale
        );
        assert_eq!(
            window.ready_slot_count(),
            0,
            "the switch freed the only live texture"
        );

        // …until its outcome lands: the active tab's texture uploads (no
        // staged show — a re-raster, no acknowledgement) and the frame
        // draws the rasterized content.
        let result = Window::rasterize_tab_svg(&solid_svg(8, 8, "#ff0000"), job_red.scale);
        assert!(
            window
                .apply_raster_result(rl, thread, &job_red.id, job_red.generation, result)
                .is_empty(),
            "nothing was pending behind the activation job"
        );
        let (scale, w, h) = window
            .active_raster()
            .expect("the incoming tab is live again");
        assert!(
            (scale - 88.125).abs() < 1e-9,
            "the live texture keeps the in-flight job's scale: {scale}"
        );
        assert_eq!(
            (w, h),
            (705, 705),
            "8×8 at 88.125 rasters to 705×705: ({w}, {h})"
        );
        let frame = render_frame(rl, thread, |d| {
            assert!(draw_tabbed_frame(
                d,
                &mut window,
                800,
                600,
                &mut Vec::new(),
                &mut Vec::new()
            ));
        });
        assert_pixel(&frame, 400, 318, Color::RED, 32);
        assert_eq!(
            window.ready_slot_count(),
            1,
            "exactly one live texture after the switch and its outcome"
        );

        // Switch back to "blue": the same dance (free the outgoing, issue
        // the incoming's raster, land it).
        let job_blue = sole_job(window.set_active(1, 800.0, 564.0));
        assert!(
            window
                .apply_raster_result(
                    rl,
                    thread,
                    &job_blue.id,
                    job_blue.generation,
                    Window::rasterize_tab_svg(&solid_svg(8, 8, "#0000ff"), job_blue.scale),
                )
                .is_empty(),
            "nothing was pending behind the activation job"
        );
        let frame = render_frame(rl, thread, |d| {
            assert!(draw_tabbed_frame(
                d,
                &mut window,
                800,
                600,
                &mut Vec::new(),
                &mut Vec::new()
            ));
        });
        assert_pixel(&frame, 400, 318, Color::BLUE, 32);

        // A staged show of a fresh tab "green" issues its raster (in
        // flight for the now-active tab)…
        let (ack_tx, ack_rx) = mpsc::channel();
        let job_green = sole_job(
            window
                .show_tab(
                    rl,
                    "green",
                    "Green",
                    &solid_svg(8, 8, "#00ff00"),
                    2.0,
                    ack_tx,
                )
                .expect("the show stages (no raster on the GUI thread)"),
        );

        // …and a switch away to "red" issues *red's* activation raster…
        let job_red2 = sole_job(window.set_active(0, 800.0, 564.0));
        assert_eq!(job_red2.id, "red", "the incoming tab's raster is issued");

        // …while switching back to "green" issues *nothing*: its raster is
        // already in flight (the coalescing bound — a second request for
        // an in-flight tab would only supersede its pending slot; the
        // in-flight job covers the re-activation).
        assert!(
            window.set_active(2, 800.0, 564.0).is_empty(),
            "re-activating a tab with an in-flight raster issues no second job"
        );

        // "green"'s outcome commits: the texture uploads (the tab is
        // active) and the deferred show acknowledgement is Ok.
        let result = Window::rasterize_tab_svg(&solid_svg(8, 8, "#00ff00"), job_green.scale);
        window.apply_raster_result(rl, thread, &job_green.id, job_green.generation, result);
        assert_eq!(
            window.ready_slot_count(),
            1,
            "exactly one live texture across all the switches"
        );
        deliver_acks(&mut window);
        ack_rx
            .recv()
            .expect("the show is acked")
            .expect("it commits");
        let frame = render_frame(rl, thread, |d| {
            assert!(draw_tabbed_frame(
                d,
                &mut window,
                800,
                600,
                &mut Vec::new(),
                &mut Vec::new()
            ));
        });
        // The SVG's green is #00ff00, not raylib's legacy GREEN (#008000):
        // assert the actual fixture color.
        assert_pixel(&frame, 400, 318, Color::new(0, 255, 0, 255), 32);
    });
}

/// Shared verification plan: `headless_reraster_keeps_layout_rect`.
///
/// A wheel zoom-in that settles fires the active tab's on-screen zoom
/// re-raster (M-3.a) through the real per-frame seams: the show's
/// 8×8 red SVG rasters to 705×705 at 88.125, the settled zoom-3.0
/// crossing re-rasters to 2115×2115 at 264.375 — and the on-screen rect is
/// invariant (the fit tracks the live pixel dimensions: both rasters draw
/// as 564×564 at (118, 36) in the 800×564 content area). The probes — the
/// content-area center plus the four content-area corners — hold across
/// the re-raster. The settled clock does not re-fire in-band (no
/// re-raster loop), and a deviation of exactly `RASTER_HYSTERESIS` (1.5)
/// is not a crossing.
#[test]
fn headless_reraster_keeps_layout_rect() {
    let _guard = HEADLESS_INIT_LOCK.lock().unwrap();
    with_headless(800, 600, |rl, thread| {
        let mut window = Window::new(0);
        staged_ready(
            rl,
            thread,
            &mut window,
            "red",
            "Red",
            &solid_svg(8, 8, "#ff0000"),
        );
        let (scale, w, h) = window
            .active_raster()
            .expect("the show committed with a live texture");
        assert!(
            (scale - 88.125).abs() < 1e-9,
            "the show's raster scale: {scale}"
        );
        assert_eq!(
            (w, h),
            (705, 705),
            "8×8 at 88.125 rasters to 705×705: ({w}, {h})"
        );

        // The layout probes: the content-area center (400, 318) is inside
        // the drawn rect; the four content-area corners are outside it
        // (the rect spans (118, 36) to (682, 600)).
        fn probe(frame: &Image) {
            assert_pixel(frame, 400, 318, Color::RED, 32);
            for (x, y) in [(2, 40), (797, 40), (2, 595), (797, 595)] {
                assert_pixel(frame, x, y, BACKGROUND, 8);
            }
        }
        let frame = render_frame(rl, thread, |d| {
            assert!(draw_tabbed_frame(
                d,
                &mut window,
                800,
                600,
                &mut Vec::new(),
                &mut Vec::new()
            ));
        });
        probe(&frame);

        // A wheel zoom-in to 3.0 (e = 70.5 × 3 = 211.5, deviating 2.4×
        // from the slot's 88.125) resets the settle clock: the crossing
        // does not fire in the frame it changes…
        window.set_zoom(3.0);
        assert!(
            window.evaluate_reraster(800.0, 564.0, 5.0).is_none(),
            "a fresh crossing (0 ms of settle) does not fire"
        );

        // …and fires once `e` has been unchanged for 200 ms: the target
        // is e × RASTER_HEADROOM = 264.375 (2115×2115 — within budget,
        // unclamped).
        let job = window
            .evaluate_reraster(800.0, 564.0, 5.3)
            .expect("the settled crossing fires the re-raster once it settles");
        assert_eq!(job.id, "red", "the re-raster belongs to the active tab");
        assert!(
            (job.scale - 264.375).abs() < 1e-9,
            "the target is e × headroom: {}",
            job.scale
        );
        assert!(
            matches!(job.kind, RasterJobKind::Reraster),
            "the job is a re-raster, not a show/activation"
        );

        // The outcome lands: the texture is replaced at the new scale —
        // and the on-screen rect is invariant (the fit of 2115×2115 into
        // 800×564 is the same 564×564 at (118, 36) as the 705×705 fit).
        assert!(
            window
                .apply_raster_result(
                    rl,
                    thread,
                    &job.id,
                    job.generation,
                    Window::rasterize_tab_svg(&solid_svg(8, 8, "#ff0000"), job.scale),
                )
                .is_empty(),
            "nothing was pending behind the re-raster"
        );
        let (scale, w, h) = window
            .active_raster()
            .expect("the re-raster landed with a live texture");
        assert!(
            (scale - 264.375).abs() < 1e-9,
            "the live texture's scale: {scale}"
        );
        assert_eq!(
            (w, h),
            (2115, 2115),
            "8×8 at 264.375 rasters to 2115×2115: ({w}, {h})"
        );
        let frame = render_frame(rl, thread, |d| {
            assert!(draw_tabbed_frame(
                d,
                &mut window,
                800,
                600,
                &mut Vec::new(),
                &mut Vec::new()
            ));
        });
        // The view is invariant in the same sense: at zoom 3.0 the
        // diagram's view rect is 564 × 3 = 1692² centered on (400, 318)
        // — the fit is scale-independent, so for *any* raster scale it
        // covers the entire 800×564 content area: the center and all four
        // content-area corners are diagram pixels (the same pixels a
        // 705² texture would yield at this zoom).
        assert_pixel(&frame, 400, 318, Color::RED, 32);
        for (x, y) in [(2, 40), (797, 40), (2, 595), (797, 595)] {
            assert_pixel(&frame, x, y, Color::RED, 32);
        }

        // At rest the deviation is the show's own headroom (1.25×, inside
        // the band): the settled clock does not re-fire — no re-raster
        // loop.
        assert!(
            window.evaluate_reraster(800.0, 564.0, 6.0).is_none(),
            "an in-band deviation at rest does not re-raster"
        );

        // Zooming out to 2.5 (e = 176.25, d = 264.375/176.25 = exactly
        // 1.5) is the band's edge: not a crossing, even once settled.
        window.set_zoom(2.5);
        assert!(
            window.evaluate_reraster(800.0, 564.0, 7.0).is_none(),
            "the zoom-out changes `e`: a fresh observation"
        );
        assert!(
            window.evaluate_reraster(800.0, 564.0, 7.3).is_none(),
            "d == RASTER_HYSTERESIS exactly is not a crossing"
        );

        // Zooming all the way back to 1.0 (e = 70.5, d = 264.375/70.5 =
        // 3.75) crosses the band in the other direction: the settled
        // crossing re-rasters back to the show's 88.125 (705×705), and
        // the original fit rect is back — the on-screen rect is invariant
        // through the re-raster chain in both directions.
        window.set_zoom(1.0);
        assert!(
            window.evaluate_reraster(800.0, 564.0, 8.0).is_none(),
            "the zoom-out to 1.0 changes `e`: a fresh observation"
        );
        let job_back = window
            .evaluate_reraster(800.0, 564.0, 8.3)
            .expect("the settled zoom-out crossing fires the re-raster");
        assert!(
            (job_back.scale - 88.125).abs() < 1e-9,
            "the zoom-out target is e × headroom: {}",
            job_back.scale
        );
        assert!(
            window
                .apply_raster_result(
                    rl,
                    thread,
                    &job_back.id,
                    job_back.generation,
                    Window::rasterize_tab_svg(&solid_svg(8, 8, "#ff0000"), job_back.scale),
                )
                .is_empty(),
            "nothing was pending behind the zoom-out re-raster"
        );
        let (scale, w, h) = window
            .active_raster()
            .expect("the zoom-out re-raster landed with a live texture");
        assert!(
            (scale - 88.125).abs() < 1e-9,
            "the live texture's scale: {scale}"
        );
        assert_eq!((w, h), (705, 705), "back at the show's 705×705: ({w}, {h})");
        let frame = render_frame(rl, thread, |d| {
            assert!(draw_tabbed_frame(
                d,
                &mut window,
                800,
                600,
                &mut Vec::new(),
                &mut Vec::new()
            ));
        });
        probe(&frame);
    });
}

/// The coalescing rule's landing half, through the real seams: a
/// re-raster settling while one is in flight becomes the tab's pending
/// (the latest crossing), and the in-flight outcome's landing fires it —
/// exactly once — when the landing *leaves* the deviation beyond
/// [`RASTER_HYSTERESIS`]: e = 423 at the landed 264.375 deviates 1.6×, so
/// the pending (the capped 512.0 target) fires and lands as the
/// 4096×4096 cap-side raster — the on-screen rect invariant.
#[test]
fn headless_reraster_pending_fires_on_landing_when_still_deviant() {
    let _guard = HEADLESS_INIT_LOCK.lock().unwrap();
    with_headless(800, 600, |rl, thread| {
        let mut window = Window::new(0);
        staged_ready(
            rl,
            thread,
            &mut window,
            "red",
            "Red",
            &solid_svg(8, 8, "#ff0000"),
        );

        // A settled zoom-3.0 crossing (e = 211.5) fires job1 at 264.375…
        window.set_zoom(3.0);
        assert!(
            window.evaluate_reraster(800.0, 564.0, 5.0).is_none(),
            "a fresh crossing does not fire"
        );
        let job1 = window
            .evaluate_reraster(800.0, 564.0, 5.3)
            .expect("the settled crossing fires");
        assert!(
            (job1.scale - 264.375).abs() < 1e-9,
            "job1's target: {}",
            job1.scale
        );
        assert!(
            matches!(job1.kind, RasterJobKind::Reraster),
            "job1 is a re-raster"
        );

        // …in flight, a steeper settled crossing (zoom 6.0, e = 423)
        // becomes the pending — the latest crossing replaces any older
        // one (no queue) — clamped to the cap's side, 512.0.
        window.set_zoom(6.0);
        assert!(
            window.evaluate_reraster(800.0, 564.0, 5.5).is_none(),
            "a changed `e` resets the settle clock"
        );
        assert!(
            window.evaluate_reraster(800.0, 564.0, 5.8).is_none(),
            "job1 is in flight: the steeper crossing is the pending, not issued"
        );

        // The landing leaves e = 423 at the slot's 264.375 (d = 1.6 >
        // 1.5): the pending is still warranted and fires — at most once.
        let fired = window.apply_raster_result(
            rl,
            thread,
            &job1.id,
            job1.generation,
            Window::rasterize_tab_svg(&solid_svg(8, 8, "#ff0000"), job1.scale),
        );
        assert_eq!(fired.len(), 1, "the landing fires the pending exactly once");
        assert!(
            (fired[0].scale - 512.0).abs() < 1e-9,
            "the fired pending carries the latest crossing's clamped target: {}",
            fired[0].scale
        );
        assert!(
            matches!(fired[0].kind, RasterJobKind::Reraster),
            "the fired job is a re-raster"
        );

        // Its landing commits at the cap's side (4096×4096)…
        assert!(
            window
                .apply_raster_result(
                    rl,
                    thread,
                    &fired[0].id,
                    fired[0].generation,
                    Window::rasterize_tab_svg(&solid_svg(8, 8, "#ff0000"), fired[0].scale),
                )
                .is_empty(),
            "nothing is pending behind the fired re-raster"
        );
        let (scale, w, h) = window.active_raster().expect("the cap-side raster is live");
        assert!((scale - 512.0).abs() < 1e-9, "the live scale: {scale}");
        assert_eq!(
            (w, h),
            (4096, 4096),
            "8×8 at 512 rasters to the 4096×4096 cap side: ({w}, {h})"
        );

        // …and the view is invariant through the re-raster chain: at
        // zoom 6.0 the diagram's view rect is 564 × 6 = 3384² centered on
        // (400, 318) — the fit is scale-independent, so for *any* raster
        // scale it covers the entire 800×564 content area: the center and
        // all four content-area corners are diagram pixels.
        let frame = render_frame(rl, thread, |d| {
            assert!(draw_tabbed_frame(
                d,
                &mut window,
                800,
                600,
                &mut Vec::new(),
                &mut Vec::new()
            ));
        });
        assert_pixel(&frame, 400, 318, Color::RED, 32);
        for (x, y) in [(2, 40), (797, 40), (2, 595), (797, 595)] {
            assert_pixel(&frame, x, y, Color::RED, 32);
        }

        // In-band at the cap's side: no further re-raster (the loop
        // guard, also by deviation: 512/423 ≈ 1.21 ≤ 1.5).
        assert!(
            window.evaluate_reraster(800.0, 564.0, 6.5).is_none(),
            "no re-raster loop at the cap"
        );
    });
}

/// The coalescing rule's landing half, second arm: a re-raster settling
/// while one is in flight becomes the pending, and the in-flight
/// outcome's landing *clears* it (no fire) when the landing leaves the
/// deviation *within* [`RASTER_HYSTERESIS`]: e = 282 at the landed
/// 264.375 deviates only ≈ 1.07×. The cleared crossing does not resurface
/// on later settled frames — the tab keeps the landed raster.
#[test]
fn headless_reraster_pending_clears_on_landing_when_in_band() {
    let _guard = HEADLESS_INIT_LOCK.lock().unwrap();
    with_headless(800, 600, |rl, thread| {
        let mut window = Window::new(0);
        staged_ready(
            rl,
            thread,
            &mut window,
            "red",
            "Red",
            &solid_svg(8, 8, "#ff0000"),
        );

        // A settled zoom-3.0 crossing (e = 211.5) fires job1 at 264.375…
        window.set_zoom(3.0);
        assert!(
            window.evaluate_reraster(800.0, 564.0, 5.0).is_none(),
            "a fresh crossing does not fire"
        );
        let job1 = window
            .evaluate_reraster(800.0, 564.0, 5.3)
            .expect("the settled crossing fires");
        assert!(
            (job1.scale - 264.375).abs() < 1e-9,
            "job1's target: {}",
            job1.scale
        );

        // …in flight, a moderate settled crossing (zoom 4.0, e = 282,
        // target 282 × 1.25 = 352.5) becomes the pending.
        window.set_zoom(4.0);
        assert!(
            window.evaluate_reraster(800.0, 564.0, 5.5).is_none(),
            "a changed `e` resets the settle clock"
        );
        assert!(
            window.evaluate_reraster(800.0, 564.0, 5.8).is_none(),
            "job1 is in flight: the crossing is the pending, not issued"
        );

        // The landing leaves e = 282 at the slot's 264.375 (d ≈ 1.07 ≤
        // 1.5): the pending no longer deviates — cleared, not fired.
        assert!(
            window
                .apply_raster_result(
                    rl,
                    thread,
                    &job1.id,
                    job1.generation,
                    Window::rasterize_tab_svg(&solid_svg(8, 8, "#ff0000"), job1.scale),
                )
                .is_empty(),
            "the in-band pending is cleared on landing, not fired"
        );
        let (scale, w, h) = window.active_raster().expect("job1's outcome is live");
        assert!(
            (scale - 264.375).abs() < 1e-9,
            "the landed scale survives the cleared pending: {scale}"
        );
        assert_eq!((w, h), (2115, 2115), "8×8 at 264.375: ({w}, {h})");

        // The cleared crossing does not resurface: e = 282 deviates from
        // 264.375 within the band, settled or not.
        assert!(
            window.evaluate_reraster(800.0, 564.0, 7.0).is_none(),
            "`e` is unchanged: the clock only accumulates"
        );
        assert!(
            window.evaluate_reraster(800.0, 564.0, 7.3).is_none(),
            "the in-band deviation, settled, is not a crossing"
        );

        // The frame is unchanged by the cleared crossing: at zoom 4.0
        // the diagram's view rect is 564 × 4 = 2256² centered on (400,
        // 318) — it covers the entire content area for *any* raster scale
        // (the fit is scale-independent): the center and all four
        // content-area corners are diagram pixels.
        let frame = render_frame(rl, thread, |d| {
            assert!(draw_tabbed_frame(
                d,
                &mut window,
                800,
                600,
                &mut Vec::new(),
                &mut Vec::new()
            ));
        });
        assert_pixel(&frame, 400, 318, Color::RED, 32);
        for (x, y) in [(2, 40), (797, 40), (2, 595), (797, 595)] {
            assert_pixel(&frame, x, y, Color::RED, 32);
        }
    });
}
