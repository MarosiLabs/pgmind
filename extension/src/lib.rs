//! pgmind extension — SQL surface over pgmind-core (RFC-002 Phase 1).
//! The extension is a thin adapter: all parsing/hashing logic lives in the
//! pure `pgmind-core` crate (Laws 1-4).

use core::ffi::CStr;
use std::ffi::CString;

use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use pgrx::prelude::*;
use pgrx::{InOutFuncs, StringInfo};
use serde::{Deserialize, Serialize};

pub mod errors;
pub mod excision;
pub mod history;
pub mod ids;
pub mod ops;
pub mod read;
pub mod rebind;
pub mod schema;
pub mod store;
mod tests_phase2;
pub mod timetravel;
pub mod verify;
pub mod write;

::pgrx::pg_module_magic!(name, version);

/// RFC-002 D9: default 8 MiB per document.
static MAX_DOCUMENT_BYTES: GucSetting<i32> = GucSetting::<i32>::new(8 * 1024 * 1024);

/// RFC-005 D3: write an absolute `note_frame` every N revisions, so deep
/// `as_of` is bounded by the cadence rather than by the note's total depth.
/// The knob trades storage (a frame is a full copy of both lanes) against
/// deep-read latency; the storage-growth gate measures both ends of it.
pub static FRAME_EVERY: GucSetting<i32> = GucSetting::<i32>::new(50);

/// RFC-011 D1: who is writing. Free text, and deliberately so — this is the
/// connected role's claim about itself, not an identity pgmind derived or
/// verified. Unset (or empty) falls back to `current_user`.
pub static AUTHOR_GUC: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);

/// RFC-003 D1: the current vault. Userset by design — vault *scoping*; the
/// tenant *boundary* story is the documented RLS patterns, not this GUC.
pub static VAULT_ID_GUC: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(Some(c"00000000-0000-0000-0000-000000000000"));

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    GucRegistry::define_int_guc(
        c"pgmind.max_document_bytes",
        c"Maximum size of a markdown document in bytes (RFC-002 D9).",
        c"Documents larger than this are rejected by the markdown type input function.",
        &MAX_DOCUMENT_BYTES,
        1024,
        i32::MAX,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"pgmind.frame_every",
        c"Revisions between absolute history frames (RFC-005 D3).",
        c"Lower bounds deep as_of latency; higher stores less history. 0 or 1 frames every revision.",
        &FRAME_EVERY,
        1,
        1_000_000,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        c"pgmind.author",
        c"Who is writing (RFC-011 D1).",
        c"Recorded as revision.author. An ASSERTION by the connected role, never authenticated: any session that can write can claim any author. Unset => current_user.",
        &AUTHOR_GUC,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        c"pgmind.vault_id",
        c"The current vault (RFC-003 D1).",
        c"UUID of the vault all knowledge.* functions operate in. Default: the default vault.",
        &VAULT_ID_GUC,
        GucContext::Userset,
        GucFlags::default(),
    );
}

/// Phase 0 smoke-test entry point: `SELECT pgmind_version();`
#[pg_extern]
fn pgmind_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The `markdown` boundary type (RFC-002 D9): validated UTF-8 source bytes.
/// Storage is the source text; parsing is transient (Law 3).
#[derive(PostgresType, Serialize, Deserialize, Debug, Clone)]
#[inoutfuncs]
pub struct Markdown(pub String);

impl InOutFuncs for Markdown {
    fn input(input: &CStr) -> Self {
        let s = input
            .to_str()
            .unwrap_or_else(|_| error!("pgmind: markdown must be valid UTF-8"));
        let limit = MAX_DOCUMENT_BYTES.get().max(0) as usize;
        if s.len() > limit {
            error!(
                "pgmind: document is {} bytes, exceeding pgmind.max_document_bytes ({})",
                s.len(),
                limit
            );
        }
        Markdown(s.to_string())
    }

    fn output(&self, buffer: &mut StringInfo) {
        buffer.push_str(&self.0);
    }
}

/// Public API schema (`knowledge.*`) — proposed in the product plan §5,
/// ratified in RFC-007. Internal storage will live in schema `pgmind` (Phase 2).
#[pg_schema]
mod knowledge {
    use super::*;
    use pgrx::iter::TableIterator;

    /// Structural access (RFC-002 D2/D5/D7): one row per addressable block.
    #[allow(clippy::type_complexity)] // pgrx TableIterator signatures are nominal
    #[pg_extern(immutable, parallel_safe)]
    fn blocks(
        doc: Markdown,
    ) -> TableIterator<
        'static,
        (
            name!(ord, i32),
            name!(kind, String),
            name!(parent, Option<i32>),
            name!(heading_path, Vec<String>),
            name!(content, String),
            name!(content_hash, Vec<u8>),
            name!(span_start, i64),
            name!(span_end, i64),
            name!(attrs, pgrx::JsonB),
        ),
    > {
        let parsed = pgmind_core::parse(&doc.0);
        TableIterator::new(parsed.blocks.into_iter().map(|b| {
            (
                b.ord as i32,
                b.kind.tag().to_string(),
                b.parent.map(|p| p as i32),
                b.heading_path,
                b.normalized_content,
                b.content_hash.to_vec(),
                b.span.start as i64,
                b.span.end as i64,
                pgrx::JsonB(b.attrs),
            )
        }))
    }

    /// Extracted references (RFC-002 D3): wiki-links, transclusions,
    /// block refs, and internal markdown links.
    #[allow(clippy::type_complexity)] // pgrx TableIterator signatures are nominal
    #[pg_extern(immutable, parallel_safe)]
    fn links(
        doc: Markdown,
    ) -> TableIterator<
        'static,
        (
            name!(kind, String),
            name!(target, String),
            name!(anchor, Option<String>),
            name!(alias, Option<String>),
            name!(block, i32),
        ),
    > {
        let parsed = pgmind_core::parse(&doc.0);
        TableIterator::new(parsed.links.into_iter().map(|l| {
            let kind = match l.kind {
                pgmind_core::LinkKind::Wikilink => "wikilink",
                pgmind_core::LinkKind::Transclusion => "transclusion",
                pgmind_core::LinkKind::Mdlink => "mdlink",
                pgmind_core::LinkKind::Blockref => "blockref",
            };
            (
                kind.to_string(),
                l.target,
                l.anchor,
                l.alias,
                l.block as i32,
            )
        }))
    }

    /// Tags (RFC-002 D3/D4): inline tags carry their block ord;
    /// frontmatter tags are note-level (block IS NULL).
    #[pg_extern(immutable, parallel_safe)]
    fn tags(
        doc: Markdown,
    ) -> TableIterator<'static, (name!(tag, String), name!(block, Option<i32>))> {
        let parsed = pgmind_core::parse(&doc.0);
        TableIterator::new(
            parsed
                .tags
                .into_iter()
                .map(|t| (t.tag, t.block.map(|b| b as i32))),
        )
    }

    /// Frontmatter properties (RFC-002 D4); `{}` when absent or invalid.
    #[pg_extern(immutable, parallel_safe)]
    fn properties(doc: Markdown) -> pgrx::JsonB {
        pgrx::JsonB(pgmind_core::parse(&doc.0).properties)
    }

    /// Document preamble (frontmatter + leading trivia, RFC-002 D6).
    #[pg_extern(immutable, parallel_safe)]
    fn preamble(doc: Markdown) -> String {
        let parsed = pgmind_core::parse(&doc.0);
        doc.0[parsed.preamble.start..parsed.preamble.end].to_string()
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn version_is_reported_via_sql() {
        let version = Spi::get_one::<String>("SELECT pgmind_version();")
            .expect("SPI call failed")
            .expect("pgmind_version() returned NULL");
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
    }

    #[pg_test]
    fn markdown_type_round_trips() {
        let out = Spi::get_one::<String>("SELECT ('# Hi\n\npara [[x]]\n'::markdown)::text;")
            .expect("SPI failed")
            .expect("NULL");
        assert_eq!(out, "# Hi\n\npara [[x]]\n");
    }

    #[pg_test]
    fn blocks_are_exposed() {
        let kinds = Spi::get_one::<i64>(
            "SELECT count(*) FROM knowledge.blocks('# A\n\npara with [[link]] and #tag\n'::markdown);",
        )
        .expect("SPI failed")
        .expect("NULL");
        assert_eq!(kinds, 2); // heading + paragraph

        let hp = Spi::get_one::<Vec<String>>(
            "SELECT heading_path FROM knowledge.blocks('# A\n\npara\n'::markdown) WHERE kind = 'paragraph';",
        )
        .expect("SPI failed")
        .expect("NULL");
        assert_eq!(hp, vec!["A".to_string()]);
    }

    #[pg_test]
    fn links_and_tags_extracted() {
        let target = Spi::get_one::<String>(
            "SELECT target FROM knowledge.links('see [[note#Sec|N]]\n'::markdown);",
        )
        .expect("SPI failed")
        .expect("NULL");
        assert_eq!(target, "note");

        let tag =
            Spi::get_one::<String>("SELECT tag FROM knowledge.tags('hello #world\n'::markdown);")
                .expect("SPI failed")
                .expect("NULL");
        assert_eq!(tag, "world");
    }

    #[pg_test]
    fn properties_and_preamble() {
        let title = Spi::get_one::<String>(
            "SELECT (knowledge.properties('---\ntitle: X\n---\nbody\n'::markdown))::jsonb->>'title';",
        )
        .expect("SPI failed")
        .expect("NULL");
        assert_eq!(title, "X");
    }
}

/// This module is required by `cargo pgrx test` invocations.
/// It must be visible at the root of your extension crate.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}
