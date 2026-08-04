//! Byte spans derived from comrak's line/column source positions (RFC-002 D5).
//!
//! comrak reports 1-based line/column positions. Columns are advisory (unreliable
//! around tab expansion, cmark heritage) — spans here are derived from LINE
//! boundaries only, via a line-offset table over the original bytes.

use serde::Serialize;

/// Byte range [start, end) into the original source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn slice<'a>(&self, source: &'a str) -> &'a str {
        &source[self.start..self.end]
    }
}

/// Offset of each line start; lines are 1-based to match comrak.
pub(crate) struct LineOffsets {
    starts: Vec<usize>,
    len: usize,
}

impl LineOffsets {
    pub fn new(source: &str) -> Self {
        // Line boundaries must mirror comrak/CommonMark exactly: a line ending
        // is LF, CRLF, or a lone CR (comrak counts lone-CR breaks too).
        let mut starts = vec![0usize];
        let bytes = source.as_bytes();
        for i in 0..bytes.len() {
            match bytes[i] {
                b'\n' => starts.push(i + 1),
                b'\r' if bytes.get(i + 1) != Some(&b'\n') => starts.push(i + 1),
                _ => {}
            }
        }
        Self {
            starts,
            len: source.len(),
        }
    }

    /// Byte offset of the start of `line` (1-based); source end if past EOF.
    pub fn line_start(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        self.starts.get(line - 1).copied().unwrap_or(self.len)
    }

    /// Span covering `start_line..=end_line` including the trailing newline
    /// of `end_line` (i.e. up to the start of the next line).
    pub fn line_span(&self, start_line: usize, end_line: usize) -> Span {
        Span {
            start: self.line_start(start_line),
            end: self.line_start(end_line + 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_offsets_basic() {
        let lo = LineOffsets::new("ab\ncd\n\nef");
        assert_eq!(lo.line_start(1), 0);
        assert_eq!(lo.line_start(2), 3);
        assert_eq!(lo.line_start(3), 6);
        assert_eq!(lo.line_start(4), 7);
        assert_eq!(lo.line_start(5), 9); // past EOF -> len
        assert_eq!(lo.line_span(2, 3), Span { start: 3, end: 7 });
    }

    #[test]
    fn no_trailing_newline() {
        let lo = LineOffsets::new("ab");
        assert_eq!(lo.line_span(1, 1), Span { start: 0, end: 2 });
    }
}
