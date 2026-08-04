//! Vault syntax pass (RFC-002 D3): wiki-links, transclusions, tags, block-ID
//! markers, mdlink classification. Operates only on the byte ranges of plain
//! Text inline nodes (CommonMark wins — precedence rule), in original source.

use unicode_normalization::UnicodeNormalization;

use crate::parse::{LinkKind, LinkRef, TagRef, TextUnit};

/// A parsed `[[ … ]]` interior.
struct WikiParts {
    target: String,
    anchor: Option<String>,
    alias: Option<String>,
}

/// Outcome of scanning after a `[[` opener.
enum WikiOutcome {
    /// Valid link; bytes consumed including the closing `]]`.
    Found(WikiParts, usize),
    /// A `]]` terminator exists but the parts are invalid (e.g. empty target);
    /// consume the bracketed span without emitting a link.
    Invalid(usize),
    /// No `]]` before line end / EOF — every later `[[` on this line fails
    /// too, so the caller may skip ahead (prevents O(n²) rescans).
    NoTerminator,
}

/// Scan every block's text units for vault syntax. `text_ranges` is indexed
/// by block ord (blocks are pushed in document order).
pub(crate) fn scan(
    text_ranges: &[Vec<TextUnit>],
    source: &str,
    links: &mut Vec<LinkRef>,
    tags: &mut Vec<TagRef>,
) {
    for (ord, units) in text_ranges.iter().enumerate() {
        let block_start = units.iter().find_map(|u| match u {
            TextUnit::Range(r) => Some(r.start),
            TextUnit::Literal(_) => None,
        });
        for unit in units {
            match unit {
                TextUnit::Range(range) => {
                    let text = &source[range.start.min(source.len())..range.end.min(source.len())];
                    scan_text(
                        text,
                        Some((range.start, source)),
                        ord as u32,
                        block_start,
                        links,
                        tags,
                    );
                }
                TextUnit::Literal(text) => {
                    scan_text(text, None, ord as u32, block_start, links, tags);
                }
            }
        }
    }
}

/// ` ^[A-Za-z0-9-]+` at end of `s` → the id. (Recognition runs in parse.rs
/// on the container-stripped last content line — RFC-002 D3.)
pub(crate) fn trailing_ref_marker(s: &str) -> Option<&str> {
    let last_line = s.rsplit('\n').next().unwrap_or(s);
    let caret = last_line.rfind(" ^")?;
    let id = &last_line[caret + 2..];
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        Some(id)
    } else {
        None
    }
}

fn scan_text(
    text: &str,
    // Some((byte offset of `text` in `source`, source)) for Range units;
    // None for reconstructed literals (table cells).
    origin: Option<(usize, &str)>,
    block: u32,
    block_inline_start: Option<usize>,
    links: &mut Vec<LinkRef>,
    tags: &mut Vec<TagRef>,
) {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // Wiki-link / transclusion.
        if bytes[i] == b'[' && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let embed = i > 0 && bytes[i - 1] == b'!' && !escaped_at(text, i - 1);
            if !escaped_at(text, i) {
                match parse_wiki(&text[i + 2..]) {
                    WikiOutcome::Found(parts, consumed) => {
                        let kind = if embed {
                            LinkKind::Transclusion
                        } else {
                            LinkKind::Wikilink
                        };
                        push_wiki(parts, kind, block, links);
                        i += 2 + consumed;
                        continue;
                    }
                    WikiOutcome::Invalid(consumed) => {
                        i += 2 + consumed;
                        continue;
                    }
                    WikiOutcome::NoTerminator => {
                        // No `]]` before line end: skip to the next line.
                        let skip = text[i..]
                            .find('\n')
                            .map(|j| i + j + 1)
                            .unwrap_or(text.len());
                        i = skip;
                        continue;
                    }
                }
            }
        }
        // Tag.
        if bytes[i] == b'#' && !escaped_at(text, i) {
            let preceded_ok = match origin {
                Some((text_offset, source)) => {
                    let abs = text_offset + i;
                    block_inline_start == Some(abs)
                        || source[..abs]
                            .chars()
                            .next_back()
                            .is_some_and(|c| c.is_whitespace())
                }
                // Literals (cells): start of the literal or preceding whitespace.
                None => {
                    i == 0
                        || text[..i]
                            .chars()
                            .next_back()
                            .is_some_and(|c| c.is_whitespace())
                }
            };
            if preceded_ok {
                let rest = &text[i + 1..];
                let end = rest
                    .char_indices()
                    .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '-')))
                    .map(|(j, _)| j)
                    .unwrap_or(rest.len());
                let tag = &rest[..end];
                if !tag.is_empty() && tag.chars().any(|c| !c.is_ascii_digit()) {
                    tags.push(TagRef {
                        tag: tag.to_string(),
                        block: Some(block),
                    });
                    i += 1 + end;
                    continue;
                }
            }
        }
        i += text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
    }
}

fn escaped_at(text: &str, pos: usize) -> bool {
    let mut backslashes = 0;
    for b in text[..pos].bytes().rev() {
        if b == b'\\' {
            backslashes += 1;
        } else {
            break;
        }
    }
    backslashes % 2 == 1
}

/// Parse the interior after `[[`. Grammar per D3 — closed, single line.
fn parse_wiki(rest: &str) -> WikiOutcome {
    let mut target = String::new();
    let mut anchor: Option<String> = None;
    let mut alias: Option<String> = None;
    let mut current = 0u8; // 0=target 1=anchor 2=alias
    let mut chars = rest.char_indices().peekable();

    while let Some((idx, c)) = chars.next() {
        match c {
            '\n' => return WikiOutcome::NoTerminator,
            '\\' => {
                if let Some(&(_, next)) = chars.peek() {
                    if matches!(next, '#' | '|' | ']') {
                        chars.next();
                        push_part(&mut target, &mut anchor, &mut alias, current, next);
                        continue;
                    }
                }
                push_part(&mut target, &mut anchor, &mut alias, current, '\\');
            }
            ']' => {
                if let Some(&(_, ']')) = chars.peek() {
                    let consumed = idx + 2;
                    // Empty target with no anchor is not a wiki-link (D3);
                    // consume the bracket pair so scanning moves past it.
                    if target.trim().is_empty() && anchor.is_none() {
                        return WikiOutcome::Invalid(consumed);
                    }
                    let target = target.trim().nfc().collect::<String>();
                    // Anchors are NFC-normalized like targets: D8 matches them
                    // against NFC heading text (NFD input must still resolve).
                    let anchor = anchor
                        .map(|a| a.trim().nfc().collect::<String>())
                        .filter(|a| !a.is_empty());
                    let alias = alias
                        .map(|a| a.trim().to_string())
                        .filter(|a| !a.is_empty());
                    return WikiOutcome::Found(
                        WikiParts {
                            target,
                            anchor,
                            alias,
                        },
                        consumed,
                    );
                }
                push_part(&mut target, &mut anchor, &mut alias, current, ']');
            }
            '#' if current == 0 => current = 1,
            '|' if current < 2 => current = 2,
            _ => push_part(&mut target, &mut anchor, &mut alias, current, c),
        }
    }
    WikiOutcome::NoTerminator
}

fn push_part(
    target: &mut String,
    anchor: &mut Option<String>,
    alias: &mut Option<String>,
    current: u8,
    c: char,
) {
    match current {
        0 => target.push(c),
        1 => anchor.get_or_insert_with(String::new).push(c),
        _ => alias.get_or_insert_with(String::new).push(c),
    }
}

fn push_wiki(parts: WikiParts, kind: LinkKind, block: u32, links: &mut Vec<LinkRef>) {
    let kind = match (&kind, parts.anchor.as_deref()) {
        (LinkKind::Wikilink, Some(a)) if a.starts_with('^') => LinkKind::Blockref,
        _ => kind,
    };
    links.push(LinkRef {
        kind,
        target: parts.target,
        anchor: parts.anchor,
        alias: parts.alias,
        block,
    });
}

/// mdlink classification (D3): scheme-ful targets are external (no edge);
/// scheme-less targets are candidate vault paths. An optional `#fragment` is
/// split off first (mirroring wiki-link anchors), then an optional `.md`.
pub(crate) fn record_mdlink(url: &str, block: u32, links: &mut Vec<LinkRef>) {
    if has_url_scheme(url) {
        return;
    }
    let (path, fragment) = match url.split_once('#') {
        Some((p, f)) if !f.is_empty() => (p, Some(f)),
        _ => (url, None),
    };
    let target = path.strip_suffix(".md").unwrap_or(path);
    let target: String = target.trim().nfc().collect();
    if target.is_empty() {
        // Pure-fragment links (`#section`) are self-references — out of v1 scope.
        return;
    }
    links.push(LinkRef {
        kind: LinkKind::Mdlink,
        target,
        anchor: fragment.map(|f| f.trim().nfc().collect()),
        alias: None,
        block,
    });
}

fn has_url_scheme(url: &str) -> bool {
    let mut chars = url.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    for c in chars {
        match c {
            ':' => return true,
            c if c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-') => {}
            _ => return false,
        }
    }
    false
}

/// For heading text (D2): replace `[[…]]` occurrences with alias-else-target.
pub(crate) fn replace_wikilinks_with_display(text: &str) -> String {
    let mut out = String::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'[' && i + 1 < bytes.len() && bytes[i + 1] == b'[' && !escaped_at(text, i) {
            if let WikiOutcome::Found(parts, consumed) = parse_wiki(&text[i + 2..]) {
                out.push_str(parts.alias.as_deref().unwrap_or(&parts.target));
                i += 2 + consumed;
                continue;
            }
        }
        let c = text[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// For heading text (D2): strip a trailing ` ^id` marker.
pub(crate) fn strip_ref_marker_text(text: &str) -> String {
    let trimmed = text.trim_end();
    if let Some(id) = trailing_ref_marker(trimmed) {
        let cut = trimmed.len() - id.len() - 2; // " ^id"
        return trimmed[..cut].to_string();
    }
    text.to_string()
}
