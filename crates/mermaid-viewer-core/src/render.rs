//! Mermaid → SVG/PNG rendering pipeline.
//!
//! The single source of truth for rendering in `mermaid-viewer-core`:
//!
//! ```text
//! mermaid source → [mermaid-render] → SVG (+w,h) → [resvg @ scale] → PNG
//! ```
//!
//! `NativeRenderer` is the only renderer. The pipeline is synchronous and
//! pure (no I/O, no threads), so it is trivially testable. Invalid input is
//! always surfaced as a typed [`crate::error::Error`] — parse-side failures
//! become [`Error::Render`](crate::error::Error::Render) and SVG
//! rasterization failures become [`Error::Rasterize`](crate::error::Error::Rasterize),
//! so the two are distinguishable by construction.

use crate::error::{Error, Result};
use mermaid_render::{render_diagram, DiagramStyle, EstimatedMeasure};

/// The font family embedded for text rasterization.
const EMBEDDED_FONT_FAMILY: &str = "Carlito";

/// The embedded font (SIL OFL 1.1, see `assets/Carlito-OFL.txt`) loaded from
/// the compiled binary, so rasterization needs no runtime filesystem access.
const EMBEDDED_FONT_DATA: &[u8] = include_bytes!("../assets/Carlito-Regular.ttf");

/// Output format of a rendered diagram image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// An XML SVG document.
    Svg,
    /// A PNG rasterization of the SVG at [`RenderOptions::png_scale`].
    Png,
}

/// A rendered diagram image in one of the supported formats.
#[derive(Debug, Clone)]
pub struct RenderedImage {
    /// The format the `bytes` are encoded in.
    pub format: ImageFormat,
    /// Image width in pixels (SVG: logical units, PNG: raster pixels).
    pub width: u32,
    /// Image height in pixels (SVG: logical units, PNG: raster pixels).
    pub height: u32,
    /// The image bytes: SVG document text, or PNG-encoded raster.
    pub bytes: Vec<u8>,
}

/// Knobs for the rendering pipeline.
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// Pixel scale factor applied when rasterizing to PNG; ignored for SVG.
    /// The PNG dimensions are `round(svg × png_scale)` (minimum 1 px).
    pub png_scale: f64,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self { png_scale: 2.0 }
    }
}

/// The synchronous, pure Mermaid renderer.
///
/// Wraps `mermaid-render` (parse + layout + SVG generation) and, for PNG,
/// rasterizes the SVG with `resvg`/`usvg` at the contracted scale. Keeping
/// the renderer a self-contained unit lets an optional subprocess backend
/// be added later without touching consumers.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeRenderer;

impl NativeRenderer {
    /// Creates a new renderer (stateless).
    pub fn new() -> Self {
        Self
    }

    /// Renders `source` into `format` with the given `opts`.
    ///
    /// * SVG output is a complete `<?xml …?> <svg width height> … </svg>`
    ///   document.
    /// * PNG bytes are a decodable PNG whose IHDR width/height equal
    ///   `round(svg_w × png_scale)` / `round(svg_h × png_scale)` (min 1 px).
    /// * Empty/whitespace source and any source the parser rejects map to
    ///   [`Error::Render`]; rasterization failures map to
    ///   [`Error::Rasterize`]. Never panics.
    pub fn render(
        &self,
        source: &str,
        format: ImageFormat,
        opts: &RenderOptions,
    ) -> Result<RenderedImage> {
        // Deterministic up-front rejection of empty input (the upstream
        // parser is lenient about empty flowcharts, so this is ours to own).
        if source.trim().is_empty() {
            return Err(Error::Render("empty diagram source".to_string()));
        }

        // mermaid → SVG content (+ logical size in px units).
        let (content, width, height) =
            render_diagram(source, &DiagramStyle::default(), &mut EstimatedMeasure)
                .map_err(Error::Render)?;
        // Round the logical size to whole pixels *once* and use it for both
        // the SVG document attributes and the reported dimensions, so the
        // document, `RenderedImage`, and the PNG IHDR all agree:
        // png_size = round(svg_size × png_scale) with exact integer svg_size.
        let width = logical_size(width);
        let height = logical_size(height);
        let svg = wrap_svg_document(&content, width, height);

        match format {
            ImageFormat::Svg => Ok(RenderedImage {
                format: ImageFormat::Svg,
                width,
                height,
                bytes: svg.into_bytes(),
            }),
            ImageFormat::Png => {
                let (bytes, width, height) = rasterize_png(&svg, opts.png_scale)?;
                Ok(RenderedImage {
                    format: ImageFormat::Png,
                    width,
                    height,
                    bytes,
                })
            }
        }
    }
}

/// Rasterizes a complete SVG document to a straight (unassociated) RGBA8
/// buffer at `scale` (min 1 px per side) and returns
/// `(rgba8, width, height)`, with `rgba8` row-major and
/// `rgba8.len() == width * height * 4`.
///
/// This is the resvg core of the rasterizer: usvg parse with
/// [`raster_options()`], a `tiny_skia::Pixmap` rendered at the clamped
/// scale, and `take_demultiplied()` straight RGBA8. Dimensions follow the
/// PNG path's rule (`round(svg_size × scale)`, minimum 1 px per side); a
/// malformed SVG is [`Error::Rasterize`]; a non-finite or non-positive
/// `scale` is clamped to 1.0, never an error.
///
/// Crate-internal: the view module's on-demand raster path (the
/// controller's `show_tab` seam) calls the same primitive, so the window's
/// textures and the PNG pipeline rasterize identically.
pub(crate) fn rasterize_rgba(svg: &str, scale: f64) -> Result<(Vec<u8>, u32, u32)> {
    // Defensive: a non-finite or non-positive scale would produce a
    // degenerate pixel buffer; clamp to 1.0 instead of failing the render.
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };

    // SVG → scene tree. Any malformed document is a rasterization failure,
    // distinguishable from a parse failure by the caller.
    let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &raster_options())
        .map_err(|e| Error::Rasterize(e.to_string()))?;

    let width = pixel_size(tree.size().width() as f64 * scale);
    let height = pixel_size(tree.size().height() as f64 * scale);

    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| Error::Rasterize("pixel buffer allocation failed".to_string()))?;
    {
        let mut surface = pixmap.as_mut();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(scale as f32, scale as f32),
            &mut surface,
        );
    }
    // `take()` would hand back the *premultiplied* RGBA buffer that
    // `tiny_skia` stores internally; consumers expect straight (unassociated)
    // alpha, so demultiply first.
    let data = pixmap.take_demultiplied(); // straight RGBA8, row-major, len = w*h*4

    Ok((data, width, height))
}

/// Rasterizes a complete SVG document to PNG bytes at `scale` (min 1 px per
/// side) and returns `(png_bytes, width, height)`.
///
/// Composes [`rasterize_rgba`] with the PNG encode step; the observable
/// behavior (dimensions, pixel values, bytes) is that of the raw buffer
/// encoded with straight RGBA8.
fn rasterize_png(svg: &str, scale: f64) -> Result<(Vec<u8>, u32, u32)> {
    let (data, width, height) = rasterize_rgba(svg, scale)?;

    // The `png::Encoder`/`Writer` borrow the output buffer for their whole
    // lifetime, so the encode step is scoped to release the borrow before
    // the buffer is returned.
    let bytes = {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_filter(png::Filter::NoFilter);
        let mut writer = encoder
            .write_header()
            .map_err(|e| Error::Rasterize(format!("png header write failed: {e}")))?;
        writer
            .write_image_data(&data)
            .map_err(|e| Error::Rasterize(format!("png encode failed: {e}")))?;
        // The writer's `Drop` impl keeps the borrow on `bytes` alive until it
        // is explicitly dropped.
        drop(writer);
        bytes
    };

    Ok((bytes, width, height))
}

/// The `usvg` options for rasterization.
///
/// `usvg` converts every `<text>` element into glyph paths at parse time and
/// needs a font for each glyph; its default options carry an *empty* font
/// database, so with defaults every glyph is silently dropped and PNGs would
/// miss all diagram labels. To keep the pipeline I/O-free, exactly one font
/// is preloaded from the compiled binary (`include_bytes!`) and the
/// generic-family requests it is asked for (mermaid-render emits
/// `font-family="sans-serif"`) are mapped onto it.
fn raster_options() -> resvg::usvg::Options<'static> {
    let mut db = resvg::usvg::fontdb::Database::new();
    db.load_font_data(EMBEDDED_FONT_DATA.to_vec());
    // `fontdb` resolves a generic `sans-serif`/`serif` request to a literal
    // family name ("Arial" by default), so point it at the embedded font.
    db.set_sans_serif_family(EMBEDDED_FONT_FAMILY);
    db.set_serif_family(EMBEDDED_FONT_FAMILY);
    resvg::usvg::Options {
        fontdb: std::sync::Arc::new(db),
        ..Default::default()
    }
}

/// Rounds a logical dimension to whole pixels, minimum 1 px.
fn pixel_size(dimension: f64) -> u32 {
    dimension.round().max(1.0) as u32
}

/// Rounds a logical (f32) dimension to whole pixels, minimum 1 px.
fn logical_size(dimension: f32) -> u32 {
    (dimension as f64).round().max(1.0) as u32
}

/// Wraps `mermaid-render`'s raw SVG *content* (shapes/text only, no root
/// element) into a complete, self-contained SVG document.
fn wrap_svg_document(content: &str, width: u32, height: u32) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" \
         height=\"{height}\" viewBox=\"0 0 {width} {height}\">\n\
         {content}\n\
         </svg>\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes PNG bytes into straight RGBA8 pixels (via the `png` crate's
    /// decoder, which is independent of this module's encoder path).
    fn decode_rgba(bytes: &[u8]) -> (Vec<[u8; 4]>, u32, u32) {
        use std::io::Cursor;
        let mut decoder = png::Decoder::new(Cursor::new(bytes));
        let info = decoder
            .read_header_info()
            .expect("test PNG has a valid header");
        let (w, h) = (info.width, info.height);
        let mut reader = decoder.read_info().expect("test PNG info readable");
        let mut pixels: Vec<[u8; 4]> = Vec::with_capacity((w * h) as usize);
        while let Some(row) = reader.next_row().expect("test PNG decodes") {
            let (rows, _) = row.data().as_chunks::<4>();
            pixels.extend(rows.iter().copied());
        }
        reader.finish().expect("test PNG decodes completely");
        (pixels, w, h)
    }

    /// Regression (audit fix 1): the PNG carries *straight* (unassociated)
    /// alpha. A 50%-opacity **red** square must decode to full-strength red
    /// with ~50% alpha; the premultiplied encoding would store it as
    /// ~half-strength red instead. (A black fill would not distinguish the
    /// two encodings, since black × 0.5 is still black.)
    #[test]
    fn rasterize_writes_straight_alpha() {
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"20\" height=\"20\">\
                   <rect x=\"5\" y=\"5\" width=\"10\" height=\"10\" fill=\"#ff0000\" fill-opacity=\"0.5\"/>\
                   </svg>";
        let (bytes, w, h) = rasterize_png(svg, 1.0).expect("renders");
        assert_eq!((w, h), (20, 20));
        let (pixels, _, _) = decode_rgba(&bytes);
        let center = pixels[(10 * w + 10) as usize];
        // 50% red on transparent, unassociated: R = 255, G = B = 0, A ≈ 128.
        assert!(
            (100..=155).contains(&center[3]),
            "expected ~50% straight alpha, got {}",
            center[3]
        );
        assert!(
            center[0] >= 230,
            "expected full-strength red channel (straight alpha), got {} (premultiplied would be ~128)",
            center[0]
        );
        assert!(
            center[1] < 10 && center[2] < 10,
            "expected no green/blue channel, got ({},{})",
            center[1],
            center[2]
        );
    }

    /// A 20×20 SVG at scale 2 rasterizes to (40, 40) with a `w*h*4`
    /// straight-RGBA8 buffer, and the geometry matches what
    /// `rasterize_png` reports for the same input and scale.
    #[test]
    fn rasterize_rgba_dimensions_and_stride() {
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"20\" height=\"20\">\
                   <rect x=\"5\" y=\"5\" width=\"10\" height=\"10\" fill=\"#ff0000\"/>\
                   </svg>";
        let (rgba, w, h) = rasterize_rgba(svg, 2.0).expect("rasterizes");
        assert_eq!((w, h), (40, 40));
        assert_eq!(rgba.len(), (w * h * 4) as usize, "buffer must be w*h*4");
        // The PNG path reports the same geometry for the same input.
        let (png, pw, ph) = rasterize_png(svg, 2.0).expect("renders");
        assert_eq!((pw, ph), (w, h), "PNG path reports the same dimensions");
        let (pixels, dw, dh) = decode_rgba(&png);
        assert_eq!((dw, dh), (w, h));
        assert_eq!(pixels.len(), rgba.len() / 4);
    }

    /// A 50%-opacity red rect yields full-strength red with ~50% alpha in
    /// the *raw* buffer: no premultiplication, and no PNG round-trip
    /// involved (the straight-alpha guarantee is checked on the primitive
    /// directly, not just after the PNG encode/decode).
    #[test]
    fn rasterize_rgba_straight_alpha() {
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"20\" height=\"20\">\
                   <rect x=\"5\" y=\"5\" width=\"10\" height=\"10\" fill=\"#ff0000\" fill-opacity=\"0.5\"/>\
                   </svg>";
        let (rgba, w, h) = rasterize_rgba(svg, 1.0).expect("rasterizes");
        assert_eq!((w, h), (20, 20));
        let center = (10 * w + 10) as usize;
        // 50% red on transparent, unassociated: R = 255, G = B = 0, A ≈ 128.
        let a = rgba[center * 4 + 3];
        assert!(
            (100..=155).contains(&a),
            "expected ~50% straight alpha, got {a}"
        );
        let r = rgba[center * 4];
        assert!(
            r >= 230,
            "expected full-strength red channel (straight alpha), got {r} (premultiplied would be ~128)"
        );
        let g = rgba[center * 4 + 1];
        let b = rgba[center * 4 + 2];
        assert!(
            g < 10 && b < 10,
            "expected no green/blue channel, got ({g},{b})"
        );
    }

    /// Regression (audit fix 2): with the embedded font loaded, `<text>`
    /// glyphs are actually rasterized. Red text must show up as red pixels;
    /// with usvg's default (empty) font db the count is 0.
    #[test]
    fn rasterize_renders_text_glyphs() {
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100\" height=\"40\">\
                   <text x=\"10\" y=\"30\" font-size=\"20\" fill=\"#ff0000\">A</text>\
                   </svg>";
        let (bytes, _, _) = rasterize_png(svg, 1.0).expect("renders");
        let (pixels, _, _) = decode_rgba(&bytes);
        let red: usize = pixels
            .iter()
            .filter(|p| p[0] > 150 && p[1] < 100 && p[2] < 100 && p[3] > 100)
            .count();
        assert!(red > 0, "no red text pixels found — glyphs were dropped");
    }

    /// The pipeline is stateless and repeatable: same input, same bytes.
    #[test]
    fn render_is_deterministic() {
        let renderer = NativeRenderer::new();
        let opts = RenderOptions::default();
        let a = renderer
            .render("graph TD\n  A-->B", ImageFormat::Svg, &opts)
            .unwrap();
        let b = renderer
            .render("graph TD\n  A-->B", ImageFormat::Svg, &opts)
            .unwrap();
        assert_eq!(a.bytes, b.bytes);
        assert_eq!(a.width, b.width);
        assert_eq!(a.height, b.height);
    }

    /// Rasterization failures surface as `Error::Rasterize`, not
    /// `Error::Render`: the two are distinguishable by construction.
    #[test]
    fn rasterize_failure_is_distinguishable() {
        let bad = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"><rect";
        assert!(matches!(rasterize_png(bad, 1.0), Err(Error::Rasterize(_))));
        // A non-finite scale is clamped to 1.0, never a failure.
        let ok = rasterize_png(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"/>",
            f64::NAN,
        )
        .expect("clamped scale renders");
        assert_eq!(ok.1, 10);
        assert_eq!(ok.2, 10);
    }

    /// Unit-level guards on the size helpers.
    #[test]
    fn size_helpers_clamp_to_one_px() {
        assert_eq!(pixel_size(0.0), 1);
        assert_eq!(pixel_size(1.499), 1);
        assert_eq!(pixel_size(1.5), 2);
        assert_eq!(pixel_size(123.0), 123);
        assert_eq!(logical_size(0.0_f32), 1);
        assert_eq!(logical_size(7.9_f32), 8);
    }

    /// The wrapper emits a complete XML SVG document.
    #[test]
    fn wrapper_emits_complete_document() {
        let doc = wrap_svg_document("<rect/>", 10, 20);
        assert!(doc.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(doc.contains("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(doc.contains(r#"width="10""#));
        assert!(doc.contains(r#"height="20""#));
        assert!(doc.contains("<rect/>"));
        assert!(doc.trim_end().ends_with("</svg>"));
    }
}
