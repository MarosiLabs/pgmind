//! Note-path grammar, normalization, and glob matching (RFC-002 D8, enforced
//! in storage by RFC-003 D5).
//!
//! Paths: UTF-8, NFC-canonicalized on input, case-sensitive, `/`-separated
//! segments; no leading/trailing `/`; segments non-empty, not `.`/`..`, no
//! control characters, no `\`, no leading/trailing whitespace; ≤ 1024 bytes
//! total; no `.md` suffix concern here (that is RFC-006's filename mapping).

use unicode_normalization::UnicodeNormalization;

pub const MAX_PATH_BYTES: usize = 1024;

/// NFC-normalize and trim surrounding whitespace (RFC-003 D5: the write path
/// normalizes, then validates). Does NOT validate.
pub fn path_normalize(path: &str) -> String {
    path.trim().nfc().collect()
}

/// RFC-002 D8 grammar check. Expects an already-normalized path (the check is
/// pure syntax: it does not itself NFC-normalize, so a valid-but-NFD path is
/// invalid — matching the storage CHECK constraint's backstop role).
pub fn path_is_valid(path: &str) -> bool {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return false;
    }
    if path != path.trim() {
        return false;
    }
    // NFC canonical form required (macOS emits NFD).
    if path.nfc().collect::<String>() != path {
        return false;
    }
    path.split('/').all(segment_is_valid)
}

fn segment_is_valid(seg: &str) -> bool {
    if seg.is_empty() || seg == "." || seg == ".." {
        return false;
    }
    if seg != seg.trim() {
        return false;
    }
    !seg.chars().any(|c| c.is_control() || c == '\\')
}

/// Last path segment — the note's title (RFC-003 D3) and the unit of
/// basename link resolution (RFC-002 D8). Assumes a valid path.
pub fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Git-style glob for `knowledge.notes()` (RFC-002 D8): `*` matches within a
/// segment, `**` matches across segments; nothing else is special. `**` is
/// recognized only as a full segment (`a/**/b`, `**`), matching zero or more
/// whole segments; a `*`-run inside a segment collapses to `*`.
pub fn glob_match(glob: &str, path: &str) -> bool {
    let pat: Vec<&str> = glob.split('/').collect();
    let segs: Vec<&str> = path.split('/').collect();
    match_segments(&pat, &segs)
}

fn match_segments(pat: &[&str], segs: &[&str]) -> bool {
    match pat.split_first() {
        None => segs.is_empty(),
        Some((&"**", rest)) => {
            // Zero or more whole segments.
            (0..=segs.len()).any(|skip| match_segments(rest, &segs[skip..]))
        }
        Some((p, rest)) => match segs.split_first() {
            Some((s, srest)) => match_one(p, s) && match_segments(rest, srest),
            None => false,
        },
    }
}

/// `*` within a single segment; literal otherwise. Linear-time two-pointer
/// wildcard match over chars.
fn match_one(pat: &str, seg: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let s: Vec<char> = seg.chars().collect();
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_nfc_and_trim() {
        // "é" as e + combining acute → NFC single code point
        assert_eq!(path_normalize("  caf\u{0065}\u{0301}  "), "caf\u{00e9}");
    }

    #[test]
    fn validity() {
        assert!(path_is_valid("projects/auth"));
        assert!(path_is_valid("a"));
        assert!(path_is_valid("caf\u{00e9}/notes"));
        for bad in [
            "",
            "/a",
            "a/",
            "a//b",
            "a/./b",
            "a/../b",
            ".",
            "..",
            "a\\b",
            "a/\u{0007}",
            " a",
            "a ",
            "a/ b",
            "a /b",
            "caf\u{0065}\u{0301}", // NFD — must be normalized before validation
        ] {
            assert!(!path_is_valid(bad), "expected invalid: {bad:?}");
        }
        assert!(!path_is_valid(&"x".repeat(MAX_PATH_BYTES + 1)));
        assert!(path_is_valid(&"x".repeat(MAX_PATH_BYTES)));
    }

    #[test]
    fn basenames() {
        assert_eq!(basename("projects/auth"), "auth");
        assert_eq!(basename("auth"), "auth");
    }

    #[test]
    fn globs() {
        assert!(glob_match("**", "a/b/c"));
        assert!(glob_match("**", "a"));
        assert!(glob_match("projects/**", "projects/auth"));
        assert!(glob_match("projects/**", "projects/a/b"));
        assert!(!glob_match("projects/**", "other/auth"));
        // '**' matches zero segments
        assert!(glob_match("projects/**/auth", "projects/auth"));
        assert!(glob_match("projects/**/auth", "projects/x/y/auth"));
        assert!(glob_match("*/auth", "projects/auth"));
        assert!(!glob_match("*/auth", "a/b/auth"));
        assert!(glob_match("proj*s/a*h", "projects/auth"));
        assert!(!glob_match("proj*s", "projects/auth"));
        // '*' does not cross segments
        assert!(!glob_match("p*h", "projects/auth"));
        // literal match, case-sensitive
        assert!(glob_match("A", "A"));
        assert!(!glob_match("a", "A"));
    }
}
