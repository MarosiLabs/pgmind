//! Normalized content (RFC-002 D7): the block's logical text with container
//! decoration stripped exactly as CommonMark strips it, then minimal
//! normalization. Feeds the content hash — the rebinding signal.

use unicode_normalization::UnicodeNormalization;

use crate::span::LineOffsets;

/// One enclosing container's per-line stripping rule, outermost first.
#[derive(Debug, Clone)]
pub(crate) enum Strip {
    /// Block quote: up to 3 spaces, '>', one optional space — if present
    /// (lazy-continuation lines have no prefix and pass through unchanged).
    Quote,
    /// List item. On the item's first source line the marker (and, for task
    /// items, the checkbox) occupies `first_width` columns; continuation lines
    /// indent by `cont_width` columns of whitespace.
    Item {
        first_line: usize,
        first_width: usize,
        cont_width: usize,
    },
}

/// Build the normalized content for one block (D7 steps 1-7).
/// `ref_marker` is the `^id` to strip (the block's own, or one shared with a
/// descendant on the same final line — D7 step 4).
pub(crate) fn normalized_content(
    md: &str,
    lo: &LineOffsets,
    own_lines: &[(usize, usize)],
    strips: &[Strip],
    ref_marker: Option<&str>,
) -> String {
    let mut logical = String::new();
    for &(start_line, end_line) in own_lines {
        for line_no in start_line..=end_line {
            let span = lo.line_span(line_no, line_no);
            if span.start >= md.len() {
                continue;
            }
            let raw = &md[span.start..span.end.min(md.len())];
            let line = raw.trim_end_matches(['\n', '\r']); // step 5: CRLF/CR → LF
            let stripped = strip_containers(line, line_no, strips);
            logical.push_str(stripped);
            logical.push('\n');
        }
    }

    // Step 4: strip the ^id marker together with any whitespace around it
    // (detection tolerates trailing spaces, so stripping must too — adding an
    // ID never changes identity). The marker sits at the end of the block's
    // last CONTENT line — for setext headings that is the line before the
    // underline, so search lines bottom-up and strip the first match.
    if let Some(id) = ref_marker {
        let marker = format!(" ^{id}");
        let mut lines: Vec<String> = logical.split('\n').map(str::to_string).collect();
        for line in lines.iter_mut().rev() {
            let ws_trimmed = line.trim_end_matches([' ', '\t']);
            if let Some(stripped) = ws_trimmed.strip_suffix(&marker) {
                *line = stripped.to_string();
                break;
            }
        }
        logical = lines.join("\n");
    }

    // Step 6: NFC uniformly (including code interiors — hash-only, D7).
    let logical: String = logical.nfc().collect();

    // Step 7: strip trailing newline runs.
    logical.trim_end_matches('\n').to_string()
}

/// Apply the container stack's per-line prefixes, outermost first.
pub(crate) fn strip_containers<'a>(mut line: &'a str, line_no: usize, strips: &[Strip]) -> &'a str {
    for strip in strips {
        line = match strip {
            Strip::Quote => strip_quote_prefix(line),
            Strip::Item {
                first_line,
                first_width,
                cont_width,
            } => {
                if line_no == *first_line {
                    strip_columns(line, *first_width)
                } else {
                    strip_columns_ws_only(line, *cont_width)
                }
            }
        };
    }
    line
}

/// CommonMark block-quote marker: up to 3 leading spaces, '>', one optional space.
fn strip_quote_prefix(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && i < 3 && bytes[i] == b' ' {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'>' {
        i += 1;
        if i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        &line[i..]
    } else {
        line // lazy continuation: no prefix to strip
    }
}

/// Consume up to `width` columns from the line start (tab stops of 4),
/// consuming ANY characters on the item's first line (the marker itself).
fn strip_columns(line: &str, width: usize) -> &str {
    let mut cols = 0usize;
    for (idx, ch) in line.char_indices() {
        if cols >= width {
            return &line[idx..];
        }
        cols += if ch == '\t' { 4 - (cols % 4) } else { 1 };
    }
    ""
}

/// Consume up to `width` columns of whitespace only (continuation lines);
/// under-indented (lazy) lines pass through with what they have.
fn strip_columns_ws_only(line: &str, width: usize) -> &str {
    let mut cols = 0usize;
    for (idx, ch) in line.char_indices() {
        if cols >= width || !matches!(ch, ' ' | '\t') {
            return &line[idx..];
        }
        cols += if ch == '\t' { 4 - (cols % 4) } else { 1 };
    }
    ""
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_prefix_variants() {
        assert_eq!(strip_quote_prefix("> foo"), "foo");
        assert_eq!(strip_quote_prefix(">foo"), "foo");
        assert_eq!(strip_quote_prefix("   > foo"), "foo");
        assert_eq!(strip_quote_prefix("lazy line"), "lazy line");
    }

    #[test]
    fn item_columns() {
        assert_eq!(strip_columns("- foo", 2), "foo");
        assert_eq!(strip_columns("10. foo", 4), "foo");
        assert_eq!(strip_columns_ws_only("  bar", 2), "bar");
        assert_eq!(strip_columns_ws_only("bar", 2), "bar");
    }
}
