//! Pure Markdown fence extraction.
//!
//! [`mermaid_blocks`] scans a document for fenced code blocks whose info
//! string is tagged `mermaid` (ASCII case-insensitive) and returns their
//! bodies, in document order, whitespace-trimmed. It is the single
//! classification signal for the `view` command: a non-empty result means
//! the input is Markdown containing one or more diagrams (tabbed window
//! path); an empty result means the whole input is a single diagram
//! (blocking full-window path).
//!
//! The scanner is deliberately small and dependency-free (hand-written, no
//! Markdown parser): it implements just the CommonMark fenced-code rules the
//! classification needs — opener/closer matching, opaque skipping of
//! non-`mermaid` fences, up-to-three-space indent stripping, and
//! unterminated fences extending to end of input. It never panics.

/// Extracts the body of every `mermaid`-tagged fenced code block in
/// `source`, in document order.
///
/// A fenced block opens on a line whose content, after removing 0–3 leading
/// spaces, begins with a run of three or more identical backtick or tilde
/// characters; the info string is the trimmed remainder of that line. The
/// block is a *mermaid* block when the first ASCII-whitespace-separated
/// token of the info string equals `mermaid` byte-wise case-insensitively.
///
/// The body is every line up to the matching closing fence — a line that,
/// after removing 0–3 leading spaces, is a run of the same fence character,
/// at least as long as the opener's run, followed by whitespace only. The
/// opener's indentation (0–3 spaces) is stripped from each body line, the
/// body is trimmed, and whitespace-only bodies are omitted. An unterminated
/// fence extends to end of input. Non-`mermaid` fences and their bodies are
/// skipped as opaque blocks, so a fence line inside another code block is
/// never an opener.
///
/// Returns `Vec::new()` when the document contains no `mermaid` fenced
/// block. CRLF line endings are tolerated. Never panics.
#[must_use]
pub fn mermaid_blocks(source: &str) -> Vec<String> {
    // Tolerate CRLF line endings: split on '\n', drop one trailing '\r'.
    let lines: Vec<&str> = source
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();

    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some((indent, ch, run, info)) = fence_open(lines[i]) else {
            i += 1;
            continue;
        };
        // Index of the closing fence, if any; an unterminated fence ends at
        // end of input.
        let close_idx = (i + 1..lines.len())
            .find(|&j| is_fence_close(lines[j], ch, run));
        let (body_end, next) = match close_idx {
            Some(j) => (j, j + 1),
            None => (lines.len(), lines.len()),
        };
        let body = lines[i + 1..body_end]
            .iter()
            .map(|line| strip_leading_spaces(line, indent))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();
        if is_mermaid_info(info) && !body.is_empty() {
            blocks.push(body);
        }
        i = next;
    }
    blocks
}

/// Counts the leading ASCII spaces of `line`.
fn leading_space_count(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Parses a potential fence-opening line into
/// `(leading spaces, fence char, fence run length, info string)`.
///
/// A fence opens only with 0–3 leading spaces followed by a run of ≥ 3
/// identical backtick or tilde characters.
fn fence_open(line: &str) -> Option<(usize, char, usize, &str)> {
    let indent = leading_space_count(line);
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let first = rest.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let run = rest.chars().take_while(|&c| c == first).count();
    if run < 3 {
        return None;
    }
    // `run` counts one-byte fence characters, so the offset is a boundary.
    Some((indent, first, run, rest[run..].trim()))
}

/// Whether `line` closes a fence opened with character `ch` and a run of
/// `min_run` characters: 0–3 leading spaces, a run of the same character of
/// at least `min_run`, then whitespace only.
fn is_fence_close(line: &str, ch: char, min_run: usize) -> bool {
    let indent = leading_space_count(line);
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let run = rest.chars().take_while(|&c| c == ch).count();
    run >= min_run && rest[run..].chars().all(char::is_whitespace)
}

/// Removes up to `max` leading ASCII spaces from `line`.
fn strip_leading_spaces(line: &str, max: usize) -> &str {
    let n = leading_space_count(line).min(max);
    &line[n..]
}

/// Whether the fence info string's first ASCII-whitespace-separated token
/// is `mermaid` (byte-wise case-insensitive).
fn is_mermaid_info(info: &str) -> bool {
    let token = info
        .split(|c: char| c.is_ascii_whitespace())
        .next()
        .unwrap_or("");
    token.eq_ignore_ascii_case("mermaid")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Given a doc with prose and two fenced mermaid blocks when extracting
    /// then exactly those two bodies are returned, in order, trimmed.
    #[test]
    fn markdown_extracts_blocks_in_order() {
        let doc = "## Intro\n\nSome prose.\n\n```mermaid\ngraph TD\n  A-->B\n```\n\nMore prose.\n\n\
                   ```mermaid\nflowchart LR\n  x --> y\n```\n\nDone.\n";
        assert_eq!(
            mermaid_blocks(doc),
            vec!["graph TD\n  A-->B", "flowchart LR\n  x --> y"]
        );
    }

    /// Given ```rust and bare ``` blocks when extracting then the result is
    /// empty.
    #[test]
    fn markdown_ignores_non_mermaid_fences() {
        let doc = "```rust\nfn main() {}\n```\n\n```\nplain text\n```\n";
        assert!(mermaid_blocks(doc).is_empty());
    }

    /// Given a `mermaid` fence nested inside a ```markdown block when
    /// extracting then nothing is extracted.
    #[test]
    fn markdown_ignores_mermaid_inside_other_fence() {
        let doc = "```markdown\nLook at this:\n\n```mermaid\ngraph TD\n  A-->B\n```\n\n```\n";
        assert!(mermaid_blocks(doc).is_empty());
    }

    /// Given ~~~mermaid and ````mermaid blocks when extracting then both
    /// bodies are returned; a ```-opened block is not closed by ~~~.
    #[test]
    fn markdown_supports_tilde_and_long_fences() {
        let doc = "~~~mermaid\ngraph TD\n  A-->B\n~~~\n\n````mermaid\nflowchart LR\n  x --> y\n````\n";
        assert_eq!(
            mermaid_blocks(doc),
            vec!["graph TD\n  A-->B", "flowchart LR\n  x --> y"]
        );

        // `~~~` is body content of a backtick-opened block, not a closer.
        let doc2 = "```mermaid\ngraph TD\n~~~\n  A-->B\n```\n";
        assert_eq!(
            mermaid_blocks(doc2),
            vec!["graph TD\n~~~\n  A-->B"]
        );
    }

    /// Given an unclosed mermaid fence when extracting then the body is the
    /// rest of the input.
    #[test]
    fn markdown_unterminated_block_extends_to_eof() {
        let doc = "text\n```mermaid\ngraph TD\n  A-->B";
        assert_eq!(mermaid_blocks(doc), vec!["graph TD\n  A-->B"]);
    }

    /// Given ```Mermaid title=\"x\" when extracting then the body is
    /// returned.
    #[test]
    fn markdown_info_token_matches_attributes_and_case() {
        let doc = "```Mermaid title=\"x\"\ngraph TD\n  A-->B\n```\n";
        assert_eq!(mermaid_blocks(doc), vec!["graph TD\n  A-->B"]);
    }

    /// Given a fence indented 2 spaces when extracting then the body has
    /// that indentation removed.
    #[test]
    fn markdown_strips_up_to_three_space_indent() {
        let doc = "  ```mermaid\n  graph TD\n    A-->B\n  ```\n";
        assert_eq!(mermaid_blocks(doc), vec!["graph TD\n  A-->B"]);
    }

    /// Given an empty mermaid fence plus a valid one when extracting then
    /// only the valid body is returned.
    #[test]
    fn markdown_omits_whitespace_only_blocks() {
        let doc = "```mermaid\n```\n\n```mermaid\n   \n```\n\n```mermaid\ngraph TD\n  A-->B\n```\n";
        assert_eq!(mermaid_blocks(doc), vec!["graph TD\n  A-->B"]);
    }

    /// Given a plain `graph TD\n A-->B` when extracting then the result is
    /// empty.
    #[test]
    fn markdown_returns_empty_without_fences() {
        let doc = "graph TD\n A-->B\n";
        assert!(mermaid_blocks(doc).is_empty());
    }

    /// Given CRLF-terminated fences when extracting then the body contains
    /// `\n`-joined lines without `\r`.
    #[test]
    fn markdown_handles_crlf() {
        let doc = "intro\r\n```mermaid\r\ngraph TD\r\n  A-->B\r\n```\r\noutro\r\n";
        assert_eq!(mermaid_blocks(doc), vec!["graph TD\n  A-->B"]);
    }
}
