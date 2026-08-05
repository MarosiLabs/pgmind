//! pgmind-core — the deterministic heart of pgmind (RFC-002).
//!
//! Pure functions from markdown source text to structure, hashes, positions,
//! and extracted references. No identity (Law 4), no I/O (Law 2), no pgrx.

mod content;
mod frontmatter;
mod parse;
pub mod path;
mod span;
mod vault;

#[cfg(feature = "conformance")]
pub mod conformance;

pub use parse::{parse, Block, BlockKind, Document, LinkKind, LinkRef, TagRef};
pub use span::Span;

/// BLAKE3 content hash (RFC-002 D7): kind_tag ‖ 0x00 ‖ normalized_content.
pub fn content_hash(kind: BlockKind, normalized_content: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(kind.tag().as_bytes());
    hasher.update(&[0u8]);
    hasher.update(normalized_content.as_bytes());
    *hasher.finalize().as_bytes()
}
