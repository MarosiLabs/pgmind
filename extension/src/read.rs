//! The Phase 2 `knowledge.*` surface (RFC-003 D7): storage-backed read,
//! navigate, and write entry points. Path-taking overloads accept `text` and
//! scope to the current vault; the Phase 1 `markdown`-value functions remain.

use pgrx::iter::TableIterator;
use pgrx::prelude::*;
use pgrx::{JsonB, Uuid};

use crate::errors::{pm_error, Pm};
use crate::store::{self, arg};
use crate::write as write_path;
use crate::Markdown;

/// A literal path prefix as a `LIKE` pattern. Note paths may legitimately
/// contain `%`, `_` and `\`, so the prefix is escaped before the wildcard is
/// appended — otherwise a path containing `%` would widen the scan rather
/// than narrow it.
fn like_prefix_pattern(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + 1);
    for c in prefix.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

#[pg_schema]
mod knowledge {
    use super::*;

    /// Upsert a whole note (RFC-003 D6). Returns the revision ID; byte-identical
    /// input returns the current head with no new revision.
    ///
    /// `expected_head` is RFC-005 D5's compare-and-swap: pass the revision you
    /// read and the write raises PM009 if someone else moved the note first.
    /// Omitting it is last-writer-wins, explicitly and by the caller's choice.
    #[pg_extern]
    fn write(path: &str, doc: Markdown, expected_head: default!(Option<Uuid>, "NULL")) -> Uuid {
        write_path::write_note(path, &doc.0, expected_head)
    }

    /// Byte-faithful read: preamble ‖ tiles (RFC-003 D2).
    #[pg_extern]
    fn read(path: &str) -> Markdown {
        let vault = store::current_vault();
        let note = store::note_by_path_or_err(vault, path);
        Markdown(store::load_source(&note))
    }

    /// Heading-delimited subtree slice; first match in document order
    /// (RFC-002 D2). PM007 when the heading path matches nothing.
    #[pg_extern]
    fn read_section(path: &str, heading_path: Vec<String>) -> Markdown {
        let vault = store::current_vault();
        let note = store::note_by_path_or_err(vault, path);
        let source = store::load_source(&note);
        let doc = pgmind_core::parse(&source);
        // A document section is delimited by a heading that IS a tile.
        //
        // `parent.is_none()` alone is not that test: blockquotes are not
        // addressable, so a heading inside a top-level quote also has no
        // parent. Matching one returned quoted text as if it were a section,
        // and the slice ran on to the next document heading — dragging in
        // paragraphs that were never inside the quote at all.
        let is_document_heading = |b: &pgmind_core::Block| {
            b.kind == pgmind_core::BlockKind::Heading
                && b.parent.is_none()
                && doc
                    .top_level
                    .iter()
                    .any(|t| t.start == b.span.start && t.end == b.span.end)
        };
        let target = doc
            .blocks
            .iter()
            .find(|b| {
                is_document_heading(b) && {
                    let own = b.attrs.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    let mut full = b.heading_path.clone();
                    full.push(own.to_string());
                    full == heading_path
                }
            })
            .cloned();
        let Some(target) = target else {
            pm_error(
                Pm::SectionNotFound,
                "section not found",
                &format!("heading path {heading_path:?} in note {path:?}"),
            );
        };
        let level = target
            .attrs
            .get("level")
            .and_then(|v| v.as_i64())
            .unwrap_or(1);
        let end = doc
            .blocks
            .iter()
            .find(|b| {
                b.ord > target.ord
                    && is_document_heading(b)
                    && b.attrs.get("level").and_then(|v| v.as_i64()).unwrap_or(1) <= level
            })
            .map(|b| b.span.start)
            .unwrap_or(source.len());
        Markdown(source[target.span.start..end].to_string())
    }

    /// Live notes matching a git-style glob (RFC-002 D8: `*` and `**` only).
    #[allow(clippy::type_complexity)] // pgrx TableIterator signatures are nominal
    #[pg_extern]
    fn notes(
        glob: default!(String, "'**'"),
    ) -> TableIterator<
        'static,
        (
            name!(path, String),
            name!(title, String),
            name!(properties, JsonB),
            name!(head_revision, Uuid),
            name!(created_at, pgrx::datum::TimestampWithTimeZone),
            name!(updated_at, Option<pgrx::datum::TimestampWithTimeZone>),
        ),
    > {
        let vault = store::current_vault();
        if !pgmind_core::path::glob_is_valid(&glob) {
            pm_error(
                Pm::InvalidPath,
                "invalid glob",
                &format!(
                    "globs are 1..={} bytes; got {}",
                    pgmind_core::path::MAX_GLOB_BYTES,
                    glob.len()
                ),
            );
        }
        // Push the glob's literal prefix down to the note_path_prefix index
        // (RFC-003 D4 created it for exactly this) instead of dragging every
        // note in the vault — plus its properties jsonb and a revision join —
        // across the SPI boundary to be filtered in Rust.
        let like = like_prefix_pattern(&pgmind_core::path::glob_literal_prefix(&glob));
        let rows: Vec<_> = Spi::connect(|client| {
            client
                .select(
                    "SELECT n.path, n.basename, n.properties, n.head_revision, n.created_at,
                            r.created_at
                     FROM pgmind.note n
                     LEFT JOIN pgmind.revision r ON r.id = n.head_revision
                     WHERE n.vault_id = $1 AND n.tombstoned_at IS NULL
                       AND n.path LIKE $2
                     ORDER BY n.path",
                    None,
                    &[arg(vault), like.as_str().into()],
                )
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in notes(): {e}"))
                .map(|row| {
                    (
                        row.get::<String>(1).unwrap().unwrap(),
                        row.get::<String>(2).unwrap().unwrap(),
                        JsonB(row.get::<JsonB>(3).unwrap().unwrap().0),
                        row.get::<Uuid>(4).unwrap().unwrap(),
                        row.get(5).unwrap().unwrap(),
                        row.get(6).unwrap(),
                    )
                })
                .collect()
        });
        TableIterator::new(rows.into_iter().filter(move |(path, ..)| {
            // The matcher is linear now, but it still runs once per candidate
            // row against a caller-supplied pattern; stay cancellable.
            pgrx::check_for_interrupts!();
            pgmind_core::path::glob_match(&glob, path)
        }))
    }

    /// Storage-backed structural access: one row per addressable block, with
    /// identity (RFC-003 D7). Spans are absolute in the note source.
    #[allow(clippy::type_complexity)] // pgrx TableIterator signatures are nominal
    #[pg_extern(name = "blocks")]
    fn blocks_by_path(
        path: &str,
    ) -> TableIterator<
        'static,
        (
            name!(block_id, Uuid),
            name!(ord, i32),
            name!(kind, String),
            name!(parent_block, Option<Uuid>),
            name!(heading_path, Vec<String>),
            name!(content, String),
            name!(content_hash, Vec<u8>),
            name!(block_ref_id, Option<String>),
            name!(span_start, i64),
            name!(span_end, i64),
            name!(attrs, JsonB),
        ),
    > {
        let vault = store::current_vault();
        let note = store::note_by_path_or_err(vault, path);
        let tiles = store::tiles_of(note.id);
        let preamble_len = note.preamble.len() as i64;
        let mut tile_starts = Vec::with_capacity(tiles.len());
        let mut cum = preamble_len;
        for t in &tiles {
            tile_starts.push(cum);
            cum += t.len() as i64;
        }
        let blocks = store::blocks_of(note.id);
        let note_id = note.id;
        TableIterator::new(blocks.into_iter().map(move |b| {
            // RFC-003 D3 deliberately has no FK from block(tile_ord) to tile —
            // the write path maintains it and verify_note checks it. So an
            // out-of-range tile_ord is a recognized corrupt state, and
            // silently basing the span at byte 0 handed the caller a span
            // pointing at a different block's text with no error at all.
            let base = tile_starts
                .get(b.tile_ord as usize)
                .copied()
                .unwrap_or_else(|| {
                    pgrx::error!(
                        "pgmind: block {} references tile {} of {} in note {note_id} — run pgmind.verify_note",
                        b.id,
                        b.tile_ord,
                        tile_starts.len()
                    )
                });
            (
                b.id,
                b.ord,
                b.kind,
                b.parent_block,
                b.heading_path,
                b.content,
                b.content_hash,
                b.block_ref_id,
                base + b.start_in_tile as i64,
                base + b.end_in_tile as i64,
                JsonB(b.attrs),
            )
        }))
    }

    /// Outgoing references of a note, with resolution state (RFC-003 D5).
    #[allow(clippy::type_complexity)] // pgrx TableIterator signatures are nominal
    #[pg_extern(name = "links")]
    fn links_by_path(
        path: &str,
    ) -> TableIterator<
        'static,
        (
            name!(block_id, Uuid),
            name!(kind, String),
            name!(target, String),
            name!(anchor, Option<String>),
            name!(alias, Option<String>),
            name!(resolved_path, Option<String>),
            name!(dangling_reason, Option<String>),
        ),
    > {
        let vault = store::current_vault();
        let note = store::note_by_path_or_err(vault, path);
        let rows: Vec<_> = Spi::connect(|client| {
            client
                .select(
                    "SELECT e.src_block, e.kind::text, e.dst_path, e.dst_heading,
                            e.dst_block_ref, e.alias, n.path, e.dangling_reason
                     FROM pgmind.edge e
                     LEFT JOIN pgmind.note n ON n.id = e.dst_note
                     WHERE e.src_note = $1
                     ORDER BY e.id",
                    None,
                    &[arg(note.id)],
                )
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in links(): {e}"))
                .map(|row| {
                    let anchor = store::join_anchor(row.get(4).unwrap(), row.get(5).unwrap());
                    (
                        row.get::<Uuid>(1).unwrap().unwrap(),
                        row.get::<String>(2).unwrap().unwrap(),
                        row.get::<String>(3).unwrap().unwrap(),
                        anchor,
                        row.get::<String>(6).unwrap(),
                        row.get::<String>(7).unwrap(),
                        row.get::<String>(8).unwrap(),
                    )
                })
                .collect()
        });
        TableIterator::new(rows)
    }

    /// Who points here? Incoming resolved edges (RFC-003 D7); anchors are
    /// reported as written — sub-note anchor resolution is query-time.
    #[allow(clippy::type_complexity)] // pgrx TableIterator signatures are nominal
    #[pg_extern]
    fn backlinks(
        path: &str,
    ) -> TableIterator<
        'static,
        (
            name!(src_path, String),
            name!(block_id, Uuid),
            name!(kind, String),
            name!(anchor, Option<String>),
            name!(excerpt, String),
        ),
    > {
        let vault = store::current_vault();
        let note = store::note_by_path_or_err(vault, path);
        let rows: Vec<_> = Spi::connect(|client| {
            client
                .select(
                    "SELECT sn.path, e.src_block, e.kind::text, e.dst_heading, e.dst_block_ref,
                            b.content
                     FROM pgmind.edge e
                     JOIN pgmind.note sn ON sn.id = e.src_note
                     JOIN pgmind.block b ON b.id = e.src_block
                     WHERE e.dst_note = $1
                     ORDER BY sn.path, e.id",
                    None,
                    &[arg(note.id)],
                )
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in backlinks(): {e}"))
                .map(|row| {
                    let anchor = store::join_anchor(row.get(4).unwrap(), row.get(5).unwrap());
                    (
                        row.get::<String>(1).unwrap().unwrap(),
                        row.get::<Uuid>(2).unwrap().unwrap(),
                        row.get::<String>(3).unwrap().unwrap(),
                        anchor,
                        row.get::<String>(6).unwrap().unwrap(),
                    )
                })
                .collect()
        });
        TableIterator::new(rows)
    }

    /// All tags in the current vault, grouped case-insensitively, spelled as
    /// the lexicographically-first variant (deterministic — RFC-003 D7).
    #[pg_extern(name = "tags")]
    fn tags_vault(
    ) -> TableIterator<'static, (name!(tag, String), name!(notes, i64), name!(blocks, i64))> {
        let vault = store::current_vault();
        let rows: Vec<_> = Spi::connect(|client| {
            client
                .select(
                    "SELECT min(tag), count(DISTINCT note_id)::int8, count(block_id)::int8
                     FROM pgmind.tag WHERE vault_id = $1
                     GROUP BY lower(tag) ORDER BY min(tag)",
                    None,
                    &[arg(vault)],
                )
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in tags(): {e}"))
                .map(|row| {
                    (
                        row.get::<String>(1).unwrap().unwrap(),
                        row.get::<i64>(2).unwrap().unwrap(),
                        row.get::<i64>(3).unwrap().unwrap(),
                    )
                })
                .collect()
        });
        TableIterator::new(rows)
    }

    /// Everything carrying a tag (case-insensitive match; RFC-002 D3).
    #[pg_extern]
    fn tagged(
        tag: &str,
    ) -> TableIterator<
        'static,
        (
            name!(path, String),
            name!(block_id, Option<Uuid>),
            name!(tag, String),
        ),
    > {
        let vault = store::current_vault();
        let rows: Vec<_> = Spi::connect(|client| {
            client
                .select(
                    "SELECT n.path, t.block_id, t.tag
                     FROM pgmind.tag t JOIN pgmind.note n ON n.id = t.note_id
                     WHERE t.vault_id = $1 AND lower(t.tag) = lower($2)
                       AND n.tombstoned_at IS NULL
                     ORDER BY n.path, t.id",
                    None,
                    &[arg(vault), tag.into()],
                )
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in tagged(): {e}"))
                .map(|row| {
                    (
                        row.get::<String>(1).unwrap().unwrap(),
                        row.get::<Uuid>(2).unwrap(),
                        row.get::<String>(3).unwrap().unwrap(),
                    )
                })
                .collect()
        });
        TableIterator::new(rows)
    }

    /// Live notes with zero resolved incoming edges from OTHER notes
    /// (self-links and dangling edges never count — RFC-003 D7).
    #[pg_extern]
    fn orphans() -> TableIterator<'static, (name!(path, String),)> {
        let vault = store::current_vault();
        let rows: Vec<_> = Spi::connect(|client| {
            client
                .select(
                    "SELECT n.path FROM pgmind.note n
                     WHERE n.vault_id = $1 AND n.tombstoned_at IS NULL
                       AND NOT EXISTS (
                         SELECT 1 FROM pgmind.edge e
                         WHERE e.dst_note = n.id AND e.src_note <> n.id)
                     ORDER BY n.path",
                    None,
                    &[arg(vault)],
                )
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in orphans(): {e}"))
                .map(|row| (row.get::<String>(1).unwrap().unwrap(),))
                .collect()
        });
        TableIterator::new(rows)
    }

    /// Vault-level counts (RFC-003 D7).
    #[allow(clippy::type_complexity)] // pgrx TableIterator signatures are nominal
    #[pg_extern]
    fn stats() -> TableIterator<
        'static,
        (
            name!(vault_id, Uuid),
            name!(notes, i64),
            name!(blocks, i64),
            name!(edges_resolved, i64),
            name!(edges_dangling, i64),
            name!(tags, i64),
            name!(revisions, i64),
            name!(bytes, i64),
        ),
    > {
        let vault = store::current_vault();
        // Every count is over the SAME population: live notes in this vault.
        // Filtering tombstones on the note count alone made the published
        // capacity ratios (bytes/block, notes/s) describe two different sets
        // of notes the moment soft delete exists.
        let counts: Vec<i64> = Spi::connect(|client| {
            let queries = [
                "SELECT count(*) FROM pgmind.note n
                  WHERE n.vault_id = $1 AND n.tombstoned_at IS NULL",
                "SELECT count(*) FROM pgmind.block b JOIN pgmind.note n ON n.id = b.note_id
                  WHERE b.vault_id = $1 AND n.tombstoned_at IS NULL",
                "SELECT count(*) FROM pgmind.edge e JOIN pgmind.note n ON n.id = e.src_note
                  WHERE e.vault_id = $1 AND n.tombstoned_at IS NULL AND e.dst_note IS NOT NULL",
                "SELECT count(*) FROM pgmind.edge e JOIN pgmind.note n ON n.id = e.src_note
                  WHERE e.vault_id = $1 AND n.tombstoned_at IS NULL AND e.dst_note IS NULL",
                "SELECT count(*) FROM pgmind.tag t JOIN pgmind.note n ON n.id = t.note_id
                  WHERE t.vault_id = $1 AND n.tombstoned_at IS NULL",
                "SELECT count(*) FROM pgmind.revision r JOIN pgmind.note n ON n.id = r.note_id
                  WHERE r.vault_id = $1 AND n.tombstoned_at IS NULL",
                "SELECT (SELECT coalesce(sum(octet_length(t.raw)), 0)
                           FROM pgmind.tile t JOIN pgmind.note n ON n.id = t.note_id
                          WHERE t.vault_id = $1 AND n.tombstoned_at IS NULL)::int8
                      + (SELECT coalesce(sum(octet_length(n.preamble)), 0) FROM pgmind.note n
                          WHERE n.vault_id = $1 AND n.tombstoned_at IS NULL)::int8",
            ];
            queries
                .iter()
                .map(|q| {
                    client
                        .select(*q, Some(1), &[arg(vault)])
                        .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in stats(): {e}"))
                        .first()
                        .get_one::<i64>()
                        .unwrap()
                        .unwrap_or(0)
                })
                .collect()
        });
        TableIterator::new(std::iter::once((
            vault, counts[0], counts[1], counts[2], counts[3], counts[4], counts[5], counts[6],
        )))
    }
}
