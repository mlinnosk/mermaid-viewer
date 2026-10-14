//! Typed errors for the mermaid-viewer-core pipelines.

/// Errors produced by the render, rasterize, and view pipelines.
///
/// Invalid diagram input is always surfaced as a typed error, never as a
/// panic or as a bogus `Ok` value.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Mermaid parsing, layout, or SVG generation failed.
    #[error("mermaid render failed: {0}")]
    Render(String),

    /// Rasterizing the SVG to PNG failed.
    #[error("svg rasterization failed: {0}")]
    Rasterize(String),

    /// Creating, updating, or driving the GUI window failed.
    #[error("view failed: {0}")]
    View(String),

    /// A file-system or other I/O operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Convenience alias for `Result<T, error::Error>` inside this crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::Error;

    /// Shared verification plan: `error_display_messages`.
    #[test]
    fn error_display_messages() {
        let render = Error::Render("syntax error at line 3".to_string());
        assert_eq!(
            render.to_string(),
            "mermaid render failed: syntax error at line 3"
        );
        assert!(render.to_string().starts_with("mermaid render failed: "));

        let rasterize = Error::Rasterize("invalid <svg> document".to_string());
        assert!(rasterize
            .to_string()
            .starts_with("svg rasterization failed: "));
        assert_eq!(
            rasterize.to_string(),
            "svg rasterization failed: invalid <svg> document"
        );

        let view = Error::View("failed to open display".to_string());
        assert!(view.to_string().starts_with("view failed: "));
        assert_eq!(view.to_string(), "view failed: failed to open display");

        let io = Error::from(std::io::Error::new(std::io::ErrorKind::NotFound, "d.mmd"));
        // `Io` is transparent: it displays the underlying io::Error.
        assert_eq!(io.to_string(), "d.mmd");
    }

    /// The `#[from]` conversion on the `Io` variant works.
    #[test]
    fn io_from_conversion() {
        let err: Error = std::io::Error::other("boom").into();
        assert!(matches!(err, Error::Io(_)));
    }
}
