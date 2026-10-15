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
//! Raylib is single-init per process and the harness enforces it, so the four
//! tests serialize on a global lock (each test owns the init for its body).

#![cfg(feature = "view-headless")]

use std::sync::Mutex;

use mermaid_viewer_core::render::{ImageFormat, RenderedImage};
use mermaid_viewer_core::view::{
    draw_frame, draw_tabbed_frame, load_window_texture, Window, BACKGROUND,
};
use raylib_sw::prelude::*;
use raylib_sw::test_harness::{assert_pixel, pixel_at, render_frame, with_headless};

/// Serializes the four tests: raylib initialization is single per process and
/// `with_headless` panics on a second init in the same process.
static HEADLESS_INIT_LOCK: Mutex<()> = Mutex::new(());

/// The fixed height (window pixels) of the tab strip the controller window
/// draws across the top — `TAB_STRIP_HEIGHT` in the view module (private
/// there; mirrored here so the probes know the strip's geometry).
const TAB_STRIP: i32 = 36;

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

/// A tabbed window with two solid-color tabs: "red" (a red PNG) shown first,
/// then "blue" (a blue PNG) — the last shown tab is the active one. Goes
/// through the same `Window::show_tab` seam the MCP controller path uses.
fn two_tab_window(rl: &mut RaylibHandle, thread: &RaylibThread) -> Window {
    let mut window = Window::new();
    window
        .show_tab(rl, thread, "red", "Red", &solid_png(8, 8, [255, 0, 0, 255]))
        .expect("the red tab loads");
    window
        .show_tab(
            rl,
            thread,
            "blue",
            "Blue",
            &solid_png(8, 8, [0, 0, 255, 255]),
        )
        .expect("the blue tab loads");
    window
}

/// Shared verification plan: `headless_tabbed_frame_active_content`.
///
/// A tabbed window with two differently colored tabs draws the *active* tab
/// into the content area below the tab strip, and switching the active index
/// switches the drawn pixels: showing the other tab's ID again (replace +
/// activate, the documented `show_tab` rule) re-targets the frame at the
/// same probe point. The probe (400, 318) is the center of the 800×564
/// content area; the fitted 8×8 image (scaled to 564×564 at (118, 36))
/// covers it.
#[test]
fn headless_tabbed_frame_active_content() {
    let _guard = HEADLESS_INIT_LOCK.lock().unwrap();
    with_headless(800, 600, |rl, thread| {
        let mut window = two_tab_window(rl, thread);
        let mut ui_closes = Vec::new();

        // "blue" was shown last, so it is active: the content pixel is blue.
        let frame = render_frame(rl, thread, |d| {
            assert!(
                draw_tabbed_frame(d, &mut window, 800, 600, &mut ui_closes),
                "tabs remain: the frame keeps the window open"
            );
        });
        assert_pixel(&frame, 400, 318, Color::BLUE, 32);

        // Re-show "red" (replace + activate): the same content pixel turns
        // red — the active index re-targets which tab is drawn.
        window
            .show_tab(rl, thread, "red", "Red", &solid_png(8, 8, [255, 0, 0, 255]))
            .expect("re-showing a known ID replaces + activates the tab");
        let frame = render_frame(rl, thread, |d| {
            assert!(draw_tabbed_frame(d, &mut window, 800, 600, &mut ui_closes));
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
            assert!(draw_tabbed_frame(d, &mut window, 800, 600, &mut ui_closes));
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
