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

/// Upper bound on a `knowledge.notes()` glob. Patterns are matched against
/// every candidate path, so an unbounded pattern is an unbounded per-row cost;
/// callers reject anything longer rather than silently not matching.
pub const MAX_GLOB_BYTES: usize = 4096;

/// Size check for a caller-supplied glob (RFC-002 D8).
pub fn glob_is_valid(glob: &str) -> bool {
    !glob.is_empty() && glob.len() <= MAX_GLOB_BYTES
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

/// The longest literal path prefix every match must start with, so callers can
/// push a range/`LIKE` predicate down to the `note_path_prefix` index instead
/// of matching every row. Conservative by construction: `**` matches zero
/// segments, so it must not contribute the preceding separator.
pub fn glob_literal_prefix(glob: &str) -> String {
    let mut literal: Vec<&str> = Vec::new();
    for seg in glob.split('/') {
        if seg == "**" {
            return literal.join("/");
        }
        if let Some(star) = seg.find('*') {
            let mut out = literal.join("/");
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(&seg[..star]);
            return out;
        }
        literal.push(seg);
    }
    literal.join("/")
}

/// Segment-level wildcard match. Iterative greedy match with a single
/// backtrack point — O(pattern x path), no recursion. The obvious
/// `(0..=segs.len()).any(|skip| recurse(...))` form is exponential in the
/// number of `**` segments and overflows the stack (which aborts the whole
/// backend) on a long pattern, and neither is acceptable for an argument that
/// arrives from SQL.
fn match_segments(pat: &[&str], segs: &[&str]) -> bool {
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while si < segs.len() {
        if pi < pat.len() && pat[pi] == "**" {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if pi < pat.len() && match_one(pat[pi], segs[si]) {
            pi += 1;
            si += 1;
        } else if let Some(sp) = star {
            // Let the last `**` absorb one more segment.
            pi = sp + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == "**" {
        pi += 1;
    }
    pi == pat.len()
}

/// `*` within a single segment; literal otherwise. Linear-time two-pointer
/// wildcard match over chars. The `*` arm must be tested before literal
/// equality: `*` is a legal path character, and testing equality first
/// consumes a pattern `*` against a literal `*` without recording a
/// backtrack point (`a*c` would then fail to match `a*bc`).
fn match_one(pat: &str, seg: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let s: Vec<char> = seg.chars().collect();
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while si < s.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
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

    #[test]
    fn star_is_a_legal_path_character() {
        // `*` in the PATH must not consume a `*` in the PATTERN without
        // leaving a backtrack point.
        assert!(glob_match("a*c", "a*bc"));
        assert!(glob_match("*x", "*yx"));
        assert!(glob_match("*", "*"));
        assert!(glob_match("a*", "a*b*c"));
        assert!(!glob_match("a*c", "a*bd"));
    }

    #[test]
    fn repeated_double_stars_are_linear() {
        // The old recursive form was exponential in the number of `**`
        // segments and blew the stack on long patterns; both are reachable
        // from SQL, so both must be cheap.
        let deep = vec!["a"; 400].join("/");
        let many = format!("{}zzz", "**/".repeat(300));
        assert!(!glob_match(&many, &deep));
        assert!(glob_match(&format!("{}a", "**/".repeat(300)), &deep));
        // Zero-segment absorption still works with many stars in a row.
        assert!(glob_match("**/**/**/a", "a"));
        assert!(glob_match("a/**/**/b", "a/b"));
    }

    #[test]
    fn literal_prefixes() {
        // Must never exclude a path the glob matches.
        assert_eq!(glob_literal_prefix("projects/auth/**"), "projects/auth");
        assert_eq!(glob_literal_prefix("projects/*"), "projects/");
        assert_eq!(glob_literal_prefix("proj*s/a*h"), "proj");
        assert_eq!(glob_literal_prefix("**"), "");
        assert_eq!(glob_literal_prefix("a/b/c"), "a/b/c");
        for (glob, path) in [
            ("projects/**", "projects"),
            ("projects/**", "projects/auth"),
            ("projects/*", "projects/auth"),
            ("**", "a/b"),
            ("a/b/c", "a/b/c"),
        ] {
            assert!(glob_match(glob, path), "{glob} should match {path}");
            assert!(
                path.starts_with(&glob_literal_prefix(glob)),
                "prefix of {glob} wrongly excludes {path}"
            );
        }
    }

    #[test]
    fn glob_size_bound() {
        assert!(glob_is_valid("**"));
        assert!(!glob_is_valid(""));
        assert!(glob_is_valid(&"a".repeat(MAX_GLOB_BYTES)));
        assert!(!glob_is_valid(&"a".repeat(MAX_GLOB_BYTES + 1)));
    }
}
