//! Headless pixel tests for the diagram window.
//!
//! This file compiles to **empty** unless the `view-headless` feature is
//! enabled, and then drives the window's real draw path through raylib's
//! software-renderer test harness (the CI pass runs:
//! `cargo test -p mermaid-viewer-core --no-default-features --features view-headless`).
//!
//! The window module's texture upload and frame draw routines
//! (`mermaid_viewer_core::view::{load_window_texture, draw_frame}`) are the
//! exact code the real window runs — the pixel assertions below validate
//! window init, texture upload, fit layout, and the frame loop's output.
//!
//! Raylib is single-init per process and the harness enforces it, so the two
//! tests serialize on a global lock (each test owns the init for its body).

#![cfg(feature = "view-headless")]

use std::sync::Mutex;

use mermaid_viewer_core::render::{ImageFormat, RenderedImage};
use mermaid_viewer_core::view::{draw_frame, load_window_texture, BACKGROUND};
use raylib_sw::prelude::*;
use raylib_sw::test_harness::{assert_pixel, render_frame, with_headless};

/// Serializes the two tests: raylib initialization is single per process and
/// `with_headless` panics on a second init in the same process.
static HEADLESS_INIT_LOCK: Mutex<()> = Mutex::new(());

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
