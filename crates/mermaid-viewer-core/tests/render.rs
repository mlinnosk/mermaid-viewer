//! Shared verification plan [M-2.a] — `NativeRenderer` rendering pipeline.
//!
//! Covers the named core tests: flowchart SVG, PNG dimensions at 1× and 2×,
//! invalid source, empty source, the 10-type smoke test, and the default
//! scale. The render pipeline is synchronous, pure, and side-effect free.

use mermaid_viewer_core::error::Error;
use mermaid_viewer_core::{ImageFormat, NativeRenderer, RenderOptions};

const FLOWCHART: &str = "graph TD\n  A-->B";
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

fn be_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

/// Reads the PNG IHDR width/height: the IHDR chunk follows the 8-byte
/// signature, so width is the big-endian u32 at offset 16 and height at 20.
fn png_dimensions(bytes: &[u8]) -> (u32, u32) {
    assert_eq!(&bytes[..8], &PNG_SIGNATURE, "bytes are not a PNG");
    assert_eq!(&bytes[12..16], b"IHDR", "first PNG chunk is not IHDR");
    (be_u32(bytes, 16), be_u32(bytes, 20))
}

/// Named: `render_flowchart_svg` — flowchart renders to SVG that starts with
/// `<?xml` or `<svg`, contains the node labels, and has positive dimensions.
#[test]
fn render_flowchart_svg() {
    let image = NativeRenderer::new()
        .render(FLOWCHART, ImageFormat::Svg, &RenderOptions::default())
        .expect("flowchart renders");

    assert_eq!(image.format, ImageFormat::Svg);
    let text = std::str::from_utf8(&image.bytes).expect("SVG is UTF-8 text");
    assert!(
        text.starts_with("<?xml") || text.starts_with("<svg"),
        "SVG must start with `<?xml` or `<svg`, got: {:?}",
        &text[..text.len().min(60)]
    );
    assert!(text.contains("A"), "missing node label A");
    assert!(text.contains("B"), "missing node label B");
    assert!(image.width > 0, "width must be > 0");
    assert!(image.height > 0, "height must be > 0");
}

/// Named: `render_png_dimensions_follow_scale` — PNG starts with the
/// signature and its IHDR equals the SVG dimensions × scale (at both 1.0
/// and 2.0).
#[test]
fn render_png_dimensions_follow_scale() {
    let renderer = NativeRenderer::new();
    let svg = renderer
        .render(FLOWCHART, ImageFormat::Svg, &RenderOptions::default())
        .expect("flowchart renders");
    let (svg_w, svg_h) = (svg.width, svg.height);

    for scale in [1.0, 2.0] {
        let png = renderer
            .render(
                FLOWCHART,
                ImageFormat::Png,
                &RenderOptions { png_scale: scale },
            )
            .expect("flowchart renders to PNG");

        assert_eq!(png.format, ImageFormat::Png);
        assert_eq!(&png.bytes[..8], &PNG_SIGNATURE, "not a PNG signature");
        let (w, h) = png_dimensions(&png.bytes);
        let expected_w = (svg_w as f64 * scale).round().max(1.0) as u32;
        let expected_h = (svg_h as f64 * scale).round().max(1.0) as u32;
        assert_eq!(
            (w, h),
            (expected_w, expected_h),
            "IHDR at scale {scale} does not match round(svg × scale)"
        );
        assert_eq!(
            (png.width, png.height),
            (w, h),
            "RenderedImage dimensions disagree with the PNG IHDR"
        );
    }
}

/// Named: `render_invalid_source_is_error` — sources mermaid-render 0.10
/// rejects map to `Err(Error::Render(msg))` with a non-empty message; never
/// a panic, never an `Ok`.
#[test]
fn render_invalid_source_is_error() {
    // Builder's choice (per the M-2 contract card): these are exact fixture
    // sources verified to be rejected by mermaid-render 0.10.0's parsers.
    let rejected = [
        "timeline",
        "timeline\n  title A Timeline",
        "mindmap",
        "gitGraph",
        "gitGraph\n  frobnicate",
    ];
    let renderer = NativeRenderer::new();
    for source in rejected {
        let result = renderer.render(source, ImageFormat::Svg, &RenderOptions::default());
        match result {
            Err(Error::Render(msg)) => {
                assert!(!msg.is_empty(), "reject message must be non-empty")
            }
            other => panic!("`{source}` must be rejected with Error::Render, got {other:?}"),
        }
    }
}

/// Named: `render_empty_source_is_error` — `""` and whitespace-only
/// sources are `Err`, never a panic and never an `Ok`.
#[test]
fn render_empty_source_is_error() {
    let renderer = NativeRenderer::new();
    for source in ["", "   \n"] {
        let result = renderer.render(source, ImageFormat::Svg, &RenderOptions::default());
        assert!(
            result.is_err(),
            "empty/whitespace source `{source:?}` must be an error, got {result:?}"
        );
    }
}

/// Named: `render_supported_types_smoke` — one fixture for each of the 10
/// supported diagram types renders to a non-empty SVG `Ok`.
#[test]
fn render_supported_types_smoke() {
    let fixtures = [
        ("flowchart", "graph TD\n  A-->B"),
        ("sequence", "sequenceDiagram\n  A->>B: hello\n  B-->>A: bye"),
        ("class", "classDiagram\n  class A { +int x }\n  A -- B"),
        ("er", "erDiagram\n  CUSTOMER ||--o{ ORDER : places"),
        (
            "state",
            "stateDiagram-v2\n  [*] --> Active\n  Active --> [*]",
        ),
        (
            "gantt",
            "gantt\n  title A Gantt Chart\n  dateFormat YYYY-MM-DD\n  \
             section Section\n  A task :a1, 2014-01-01, 30d",
        ),
        ("pie", "pie\n  title Pie chart\n  \"A\" : 42\n  \"B\" : 58"),
        (
            "timeline",
            "timeline\n  title A Timeline\n  Section 1 : Event 1 : Event 2",
        ),
        ("mindmap", "mindmap\n  root((root))\n    A\n    B"),
        ("gitgraph", "gitGraph\n  commit\n  branch dev"),
    ];
    assert_eq!(fixtures.len(), 10, "smoke test must cover all 10 types");

    let renderer = NativeRenderer::new();
    let opts = RenderOptions::default();
    for (name, source) in fixtures {
        let image = renderer
            .render(source, ImageFormat::Svg, &opts)
            .unwrap_or_else(|e| panic!("diagram type `{name}` failed to render: {e}"));
        assert!(!image.bytes.is_empty(), "`{name}` produced an empty SVG");
        let text = std::str::from_utf8(&image.bytes).expect("SVG is UTF-8");
        assert!(
            text.starts_with("<?xml") || text.starts_with("<svg"),
            "`{name}` SVG is not a full document: {:?}",
            &text[..text.len().min(60)]
        );
        assert!(
            image.width > 0 && image.height > 0,
            "`{name}` has zero size"
        );
    }
}

/// Named: `default_options_scale_is_2` — the default `png_scale` is 2.0.
#[test]
fn default_options_scale_is_2() {
    let opts = RenderOptions::default();
    assert_eq!(opts.png_scale, 2.0);
}

/// Acceptance example from the sub-milestone: `png_scale = 3.0` yields
/// `width = 3 × svg_w` (and `height = 3 × svg_h`).
#[test]
fn acceptance_example_scale_3() {
    let renderer = NativeRenderer::new();
    let svg = renderer
        .render(FLOWCHART, ImageFormat::Svg, &RenderOptions::default())
        .unwrap();
    let png = renderer
        .render(
            FLOWCHART,
            ImageFormat::Png,
            &RenderOptions { png_scale: 3.0 },
        )
        .unwrap();
    assert_eq!(png.width, 3 * svg.width);
    assert_eq!(png.height, 3 * svg.height);
    let (w, h) = png_dimensions(&png.bytes);
    assert_eq!((w, h), (png.width, png.height));
}
