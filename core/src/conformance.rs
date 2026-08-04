//! Internal conformance renderer (RFC-002 D9): test/eval only.
//! Pure CommonMark configuration — vault pass and ALL GFM extensions disabled
//! (gate 1) — rendered via comrak's HTML output.

use comrak::{markdown_to_html, Options};

/// Render with the PURE CommonMark configuration (no extensions at all).
pub fn render_pure_commonmark(source: &str) -> String {
    let mut options = Options::default();
    // Spec tests include raw HTML passthrough.
    options.render.r#unsafe = true;
    markdown_to_html(source, &options)
}
