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

/// How many notes one `write_many()` call accepts.
///
/// Not a memory bound — `pgmind.max_document_bytes` already bounds each element
/// and `text[]` has no per-element ceiling (RFC-003 D6). It bounds *time*: the
/// whole batch is one statement in one transaction, holding its path locks and
/// rolling back entirely on any failure. At the write cost D8 publishes — single
/// digit ms per note — a full batch is a several-second statement, which is a
/// unit a caller can retry; ten times that is a lock hold long enough to be
/// somebody else's outage.
const MAX_BATCH_NOTES: usize = 1000;

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

    /// Create a vault. Returns its id and name.
    ///
    /// `vault_id` is caller-supplied and defaults to a minted UUIDv7: an
    /// application that already keys tenants, users or agents by id wants the
    /// vault to carry the id it chose, so its own row and the vault agree
    /// without a mapping table to keep in sync. Supplying it is also what makes
    /// provisioning idempotent alongside `if_not_exists`.
    ///
    /// A collision on either id or name raises rather than adopting the
    /// existing vault, unless `if_not_exists` — silently handing back somebody
    /// else's vault is the failure this registry exists to prevent.
    #[pg_extern(requires = ["pgmind_storage"])]
    fn create_vault(
        name: &str,
        description: default!(Option<String>, "NULL"),
        vault_id: default!(Option<Uuid>, "NULL"),
        if_not_exists: default!(bool, false),
    ) -> TableIterator<'static, (name!(vault_id, Uuid), name!(name, String))> {
        let normalized = pgmind_core::path::path_normalize(name);
        if !pgmind_core::path::path_is_valid(&normalized) {
            pm_error(
                Pm::InvalidPath,
                "invalid vault name",
                &format!("vault names use the same grammar as note paths; {name:?} does not"),
            );
        }
        let existing: Option<Uuid> = Spi::get_one_with_args(
            "SELECT id FROM pgmind.vault WHERE name = $1",
            &[normalized.as_str().into()],
        )
        .unwrap_or(None);
        if let Some(id) = existing {
            if if_not_exists {
                return TableIterator::once((id, normalized));
            }
            pm_error(
                Pm::PathTaken,
                "a vault with that name already exists",
                &format!("vault {normalized:?} is {id}; pass if_not_exists => true to adopt it"),
            );
        }
        let id = vault_id.unwrap_or_else(crate::ids::mint);
        let taken: Option<String> =
            Spi::get_one_with_args("SELECT name FROM pgmind.vault WHERE id = $1", &[arg(id)])
                .unwrap_or(None);
        if let Some(other) = taken {
            if if_not_exists && other == normalized {
                return TableIterator::once((id, normalized));
            }
            pm_error(
                Pm::PathTaken,
                "a vault with that id already exists",
                &format!("id {id} is vault {other:?}"),
            );
        }
        Spi::run_with_args(
            "INSERT INTO pgmind.vault (id, name, description) VALUES ($1, $2, $3)",
            &[
                arg(id),
                normalized.as_str().into(),
                description.as_deref().into(),
            ],
        )
        .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure creating vault: {e}"));
        TableIterator::once((id, normalized))
    }

    /// Every vault, filtered by a glob over the NAME (RFC-002 D8 `*`/`**`).
    ///
    /// This lists every vault in the database. pgmind has no tenant concept —
    /// applications carry their hierarchy in the name, so scoping a listing is
    /// `knowledge.vaults('acme/**')` and enforcing that scope is the
    /// application's job until the isolation RFC lands.
    #[pg_extern(requires = ["pgmind_storage"])]
    fn vaults(
        glob: default!(String, "'**'"),
    ) -> TableIterator<
        'static,
        (
            name!(vault_id, Uuid),
            name!(name, String),
            name!(description, Option<String>),
            name!(created_at, pgrx::datum::TimestampWithTimeZone),
        ),
    > {
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
        let rows: Vec<_> = Spi::connect(|client| {
            client
                .select(
                    "SELECT id, name, description, created_at FROM pgmind.vault ORDER BY name",
                    None,
                    &[],
                )
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in vaults(): {e}"))
                .map(|row| {
                    (
                        row.get::<Uuid>(1).unwrap().unwrap(),
                        row.get::<String>(2).unwrap().unwrap(),
                        row.get::<String>(3).unwrap(),
                        row.get(4).unwrap().unwrap(),
                    )
                })
                .collect()
        });
        TableIterator::new(rows.into_iter().filter(move |(_, name, ..)| {
            pgrx::check_for_interrupts!();
            pgmind_core::path::glob_match(&glob, name)
        }))
    }

    /// A note's id, by path. PM002 when there is none.
    ///
    /// The public API could not previously produce a note id at all, which is
    /// why `pgmind.verify_note(note_id uuid)` — the health check — took an
    /// argument nothing public returned, and why an application keying a row to
    /// a note had to store mutable text and hope.
    #[pg_extern(requires = ["pgmind_storage"])]
    fn note_id(path: &str, vault: default!(Option<String>, "NULL")) -> Uuid {
        let vault = store::resolve_vault(vault.as_deref());
        store::note_by_path_or_err(vault, path).id
    }

    /// A note's current path, by id. The inverse of `note_id`, and the reason
    /// an id is worth holding: it survives `move_note`, where a path does not.
    #[pg_extern(requires = ["pgmind_storage"])]
    fn path_of(note_id: Uuid) -> String {
        let vault = store::current_vault();
        let found: Option<String> = Spi::get_one_with_args(
            "SELECT path FROM pgmind.note
              WHERE id = $1 AND vault_id = $2 AND tombstoned_at IS NULL",
            &[arg(note_id), arg(vault)],
        )
        .unwrap_or(None);
        found.unwrap_or_else(|| {
            pm_error(Pm::NoteNotFound, "note not found", &format!("id {note_id}"))
        })
    }

    /// A vault's id, by name. PM018 when there is none.
    #[pg_extern(requires = ["pgmind_storage"])]
    fn vault_id(name: &str) -> Uuid {
        store::resolve_vault(Some(name))
    }

    /// Upsert a whole note (RFC-003 D6). Returns the revision ID; byte-identical
    /// input returns the current head with no new revision.
    ///
    /// `expected_head` is RFC-005 D5's compare-and-swap: pass the revision you
    /// read and the write raises PM009 if someone else moved the note first.
    /// Omitting it is last-writer-wins, explicitly and by the caller's choice.
    #[pg_extern]
    fn write(
        path: &str,
        doc: Markdown,
        expected_head: default!(Option<Uuid>, "NULL"),
        vault: default!(Option<String>, "NULL"),
    ) -> Uuid {
        write_path::write_note(path, &doc.0, expected_head, vault.as_deref())
    }

    /// Write many notes in one call, in input order. Returns one row per input.
    ///
    /// Each element is `write()` with `expected_head => NULL`, so the per-note
    /// semantics are unchanged: upsert, last-writer-wins, byte-identical input
    /// returns the current head without minting a revision. Compare-and-swap is
    /// deliberately absent — a batch is for ingestion, where the caller holds no
    /// prior head, and an `expected_heads uuid[]` would make the one array a
    /// caller must get exactly right the one they have no values for.
    ///
    /// Parallel arrays rather than one `jsonb`: RFC-003 D6's batching amendment
    /// makes this normative, not stylistic. `jsonb` imposes PostgreSQL's
    /// `JENTRY_OFFLENMASK` ceiling on the element total, so routing documents
    /// through it would let a batch reject a note that `write()` accepts —
    /// batching may not narrow what pgmind stores.
    ///
    /// This is one statement in one transaction: a failure anywhere rolls the
    /// whole batch back. That is the reason for `MAX_BATCH_NOTES` — not memory,
    /// which `markdown`'s own limit already bounds, but making the unit of retry
    /// something a caller can re-send.
    ///
    /// **This does not make writes faster, and the claim that it would was
    /// wrong.** Measured on a local socket, 500 notes as 500 autocommit
    /// statements, as one `SELECT count(write(…)) FROM staging`, and as one
    /// `write_many` all cost the same 8.4 ms/note — within run-to-run spread of
    /// each other. Per-note work (parse, hash, extraction, the revision row)
    /// dominates so completely that removing 499 statements and 499 commits
    /// changes nothing measurable; a commit on that machine costs 0.16 ms
    /// against a note's 8.4. What batching removes is `N` round trips and `N`
    /// commits, so the saving is whatever those cost *you* — real over a network
    /// or against durable storage, nil over a socket. The ergonomics are the
    /// point: a migration is one call.
    #[pg_extern]
    fn write_many(
        paths: Vec<Option<String>>,
        docs: Vec<Option<Markdown>>,
        vault: default!(Option<String>, "NULL"),
    ) -> TableIterator<'static, (name!(path, String), name!(revision, Uuid))> {
        if paths.len() != docs.len() {
            pm_error(
                Pm::BatchArity,
                "paths and docs must have the same length",
                &format!("{} path(s), {} doc(s)", paths.len(), docs.len()),
            );
        }
        if paths.len() > MAX_BATCH_NOTES {
            pm_error(
                Pm::BatchArity,
                "batch is too large",
                &format!("{} notes, limit {MAX_BATCH_NOTES}", paths.len()),
            );
        }
        // Resolved once. Resolving per note would re-probe the registry for a
        // value that cannot change mid-statement, and — worse — a batch that
        // straddled two vaults would be a silent surprise rather than an error.
        let vault = store::resolve_vault(vault.as_deref());

        let rows: Vec<(String, Uuid)> = paths
            .into_iter()
            .zip(docs)
            .enumerate()
            .map(|(i, (path, doc))| {
                // A NULL element is caught here rather than left to unwrap: the
                // array is often built by the client's own SQL, where a missing
                // join row produces NULL, and "element 37 is NULL" names the
                // input that is wrong.
                let (path, doc) = match (path, doc) {
                    (Some(p), Some(d)) => (p, d),
                    (p, _) => pm_error(
                        Pm::BatchArity,
                        "batch element is NULL",
                        &format!("element {i} ({})", if p.is_none() { "path" } else { "doc" }),
                    ),
                };
                // The stored path, not the submitted one. `write()` normalizes
                // (NFC, trim) and the caller needs a value it can read back
                // with; input order already lets it correlate rows to inputs.
                let path = pgmind_core::path::path_normalize(&path);
                let rev = write_path::write_note_in(vault, &path, &doc.0, None);
                (path, rev)
            })
            .collect();
        TableIterator::new(rows)
    }

    /// Byte-faithful read: preamble ‖ tiles (RFC-003 D2).
    #[pg_extern]
    fn read(path: &str, vault: default!(Option<String>, "NULL")) -> Markdown {
        let vault = store::resolve_vault(vault.as_deref());
        let note = store::note_by_path_or_err(vault, path);
        Markdown(store::load_source(&note))
    }

    /// Heading-delimited subtree slice; first match in document order
    /// (RFC-002 D2). PM007 when the heading path matches nothing.
    #[pg_extern]
    fn read_section(
        path: &str,
        heading_path: Vec<String>,
        vault: default!(Option<String>, "NULL"),
    ) -> Markdown {
        let vault = store::resolve_vault(vault.as_deref());
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
        vault: default!(Option<String>, "NULL"),
    ) -> TableIterator<
        'static,
        (
            name!(note_id, Uuid),
            name!(path, String),
            name!(title, String),
            name!(description, Option<String>),
            name!(properties, JsonB),
            name!(head_revision, Uuid),
            name!(created_at, pgrx::datum::TimestampWithTimeZone),
            name!(updated_at, Option<pgrx::datum::TimestampWithTimeZone>),
        ),
    > {
        let vault = store::resolve_vault(vault.as_deref());
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
                    "SELECT n.path, coalesce(n.title, n.basename), n.properties,
                            n.head_revision, n.created_at, r.created_at, n.id, n.description
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
                        row.get::<Uuid>(7).unwrap().unwrap(),
                        row.get::<String>(1).unwrap().unwrap(),
                        row.get::<String>(2).unwrap().unwrap(),
                        row.get::<String>(8).unwrap(),
                        JsonB(row.get::<JsonB>(3).unwrap().unwrap().0),
                        row.get::<Uuid>(4).unwrap().unwrap(),
                        row.get(5).unwrap().unwrap(),
                        row.get(6).unwrap(),
                    )
                })
                .collect()
        });
        TableIterator::new(rows.into_iter().filter(move |(_, path, ..)| {
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
        vault: default!(Option<String>, "NULL"),
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
        let vault = store::resolve_vault(vault.as_deref());
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
        vault: default!(Option<String>, "NULL"),
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
        let vault = store::resolve_vault(vault.as_deref());
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
        vault: default!(Option<String>, "NULL"),
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
        let vault = store::resolve_vault(vault.as_deref());
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
        vault: default!(Option<String>, "NULL"),
    ) -> TableIterator<'static, (name!(tag, String), name!(notes, i64), name!(blocks, i64))> {
        let vault = store::resolve_vault(vault.as_deref());
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
    ///
    /// `path` narrows to one note — "which blocks in THIS document are tagged
    /// X", which the vault-wide form could only answer by returning the whole
    /// vault and making the caller filter. It is index-backed by
    /// `tag_note_lookup`, and a path with no live note is PM002 rather than an
    /// empty result: a typo'd path and a tag nothing carries are different
    /// answers and should not look alike.
    #[pg_extern]
    fn tagged(
        tag: &str,
        path: default!(Option<String>, "NULL"),
        vault: default!(Option<String>, "NULL"),
    ) -> TableIterator<
        'static,
        (
            name!(path, String),
            name!(block_id, Option<Uuid>),
            name!(tag, String),
        ),
    > {
        let vault = store::resolve_vault(vault.as_deref());
        let note = path
            .as_deref()
            .map(|p| store::note_by_path_or_err(vault, p).id);
        let rows: Vec<_> = Spi::connect(|client| {
            client
                .select(
                    "SELECT n.path, t.block_id, t.tag
                     FROM pgmind.tag t JOIN pgmind.note n ON n.id = t.note_id
                     WHERE t.vault_id = $1 AND lower(t.tag) = lower($2)
                       AND ($3::uuid IS NULL OR t.note_id = $3)
                       AND n.tombstoned_at IS NULL
                     ORDER BY n.path, t.id",
                    None,
                    &[arg(vault), tag.into(), note.into()],
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

    /// Full-text search over blocks, best match first.
    ///
    /// `q` is `websearch_to_tsquery` syntax — bare words are ANDed, `"quoted
    /// phrases"` are literal, `or` and a leading `-` do what they look like.
    /// That parser is chosen over `to_tsquery` because it cannot be made to
    /// raise: a search box is fed whatever a user or an agent types, and a
    /// syntax error is not an answer.
    ///
    /// `path` narrows to one note, `tags` to blocks carrying every tag listed —
    /// where a tag counts if it is on the block OR on its note, so a
    /// frontmatter tag scopes the whole document the way a reader expects.
    ///
    /// **A query with no lexemes in it is a filter, not a failure.** Empty,
    /// whitespace or all stop words means "no text predicate": with `tags` or
    /// `path` given, those still apply and `rank` is NULL, because there is no
    /// ranking function involved and a fabricated 0.0 would be a number the
    /// caller could sort by and should not. With nothing else given there is no
    /// predicate at all, and that is an empty result rather than the whole
    /// vault. This is the only way to ask for the intersection of several tags
    /// — `tagged()` takes one — and without it the `tags` argument would be
    /// reachable only in the company of a text query it has nothing to do with.
    ///
    /// Matching, ranking and excerpting are Postgres's own: this function adds
    /// vault scoping, the tag filter, and nothing else (Law 6).
    #[allow(clippy::type_complexity)] // pgrx TableIterator signatures are nominal
    #[pg_extern]
    fn search(
        q: &str,
        path: default!(Option<String>, "NULL"),
        tags: default!(Option<Vec<String>>, "NULL"),
        limit_n: default!(i32, 20),
        vault: default!(Option<String>, "NULL"),
    ) -> TableIterator<
        'static,
        (
            name!(path, String),
            name!(block_id, Uuid),
            name!(heading_path, Vec<String>),
            name!(excerpt, String),
            name!(rank, Option<f32>),
        ),
    > {
        let vault = store::resolve_vault(vault.as_deref());
        let note = path
            .as_deref()
            .map(|p| store::note_by_path_or_err(vault, p).id);

        // Decide here whether there is a text predicate at all. Postgres emits
        // one NOTICE when a query has no lexemes — worth keeping, it is the
        // only explanation a caller gets — and settling it once means the main
        // statement neither repeats the NOTICE nor evaluates the parser again.
        let has_text: bool = Spi::get_one_with_args(
            "SELECT numnode(websearch_to_tsquery('english', $1)) > 0",
            &[q.into()],
        )
        .unwrap_or(Some(false))
        .unwrap_or(false);
        let has_tags = tags.as_ref().is_some_and(|t| !t.is_empty());
        if !has_text && !has_tags {
            // No predicate of any kind. Returning the whole vault here would be
            // the worst possible reading of an empty search box.
            return TableIterator::new(Vec::new());
        }
        // NULL rather than the empty string when there is no text predicate:
        // websearch_to_tsquery is STRICT, so it returns NULL without running,
        // which is what keeps the NOTICE from firing a second time.
        let q_arg: Option<&str> = has_text.then_some(q);

        let rows: Vec<_> = Spi::connect(|client| {
            client
                .select(
                    // Two things this statement must not lose, both measured:
                    //
                    // pgmind.search_vector() is repeated VERBATIM from the index
                    // definition — an expression index is only used when the
                    // query spells the expression the same way.
                    //
                    // And the CTE must stay inlinable. Written AS MATERIALIZED
                    // it plans as a scan of every live note probing blocks per
                    // note, because materializing hides the tsquery from
                    // constant folding and the planner loses any estimate for
                    // `@@`. Inlined, it is a bitmap index scan on block_fts and
                    // a note_pkey lookup.
                    "WITH tq AS (SELECT websearch_to_tsquery('english', $2) AS q),
                     m AS (
                       SELECT n.path, b.id AS block_id, b.parent_block,
                              b.heading_path, b.content, b.ord,
                              CASE WHEN tq.q IS NULL THEN NULL
                                   ELSE ts_rank_cd(pgmind.search_vector(b.content), tq.q, 32)
                              END AS rank
                       FROM tq, pgmind.block b
                       JOIN pgmind.note n ON n.id = b.note_id
                       WHERE b.vault_id = $1
                         AND n.tombstoned_at IS NULL
                         AND ($3::uuid IS NULL OR b.note_id = $3)
                         AND (tq.q IS NULL
                              OR pgmind.search_vector(b.content) @@ tq.q)
                         AND ($4::text[] IS NULL OR NOT EXISTS (
                               SELECT 1 FROM unnest($4::text[]) AS want(tag)
                               WHERE NOT EXISTS (
                                 SELECT 1 FROM pgmind.tag t
                                 WHERE t.note_id = b.note_id
                                   AND (t.block_id = b.id OR t.block_id IS NULL)
                                   AND lower(t.tag) = lower(want.tag))))
                     ),
                     hit AS (
                       -- Report the most specific match and not its containers.
                       -- A list item's content includes its own paragraph
                       -- (RFC-002 D7), so `- rotate the key` matches twice and
                       -- an agent gets the same sentence billed to it twice.
                       -- Dropping a block when one of its children also matched
                       -- keeps the innermost — which is also the block whose id
                       -- is the better citation and the simpler edit target.
                       SELECT * FROM m
                       WHERE NOT EXISTS (
                         SELECT 1 FROM m child WHERE child.parent_block = m.block_id)
                       ORDER BY rank DESC NULLS LAST, path, ord
                       LIMIT $5
                     )
                     SELECT hit.path, hit.block_id, hit.heading_path,
                            CASE WHEN tq.q IS NULL
                                 -- Nothing was matched, so there is nothing to
                                 -- highlight; the opening of the block is the
                                 -- honest excerpt.
                                 THEN left(hit.content, 200)
                                 ELSE ts_headline('english', hit.content, tq.q,
                                        'StartSel=**, StopSel=**, MaxFragments=1,
                                         MaxWords=30, MinWords=12')
                            END AS excerpt,
                            hit.rank
                     -- ts_headline runs after the LIMIT, so it is paid for the
                     -- rows returned rather than every row that matched.
                     FROM hit, tq
                     ORDER BY hit.rank DESC NULLS LAST, hit.path, hit.ord",
                    None,
                    &[
                        arg(vault),
                        q_arg.into(),
                        note.into(),
                        tags.into(),
                        i64::from(limit_n.max(0)).into(),
                    ],
                )
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in search(): {e}"))
                .map(|row| {
                    (
                        row.get::<String>(1).unwrap().unwrap(),
                        row.get::<Uuid>(2).unwrap().unwrap(),
                        row.get::<Vec<String>>(3).unwrap().unwrap_or_default(),
                        row.get::<String>(4).unwrap().unwrap_or_default(),
                        row.get::<f32>(5).unwrap(),
                    )
                })
                .collect()
        });
        TableIterator::new(rows)
    }

    /// Live notes with zero resolved incoming edges from OTHER notes
    /// (self-links and dangling edges never count — RFC-003 D7).
    #[pg_extern]
    fn orphans(
        vault: default!(Option<String>, "NULL"),
    ) -> TableIterator<'static, (name!(path, String),)> {
        let vault = store::resolve_vault(vault.as_deref());
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
    fn stats(
        vault: default!(Option<String>, "NULL"),
    ) -> TableIterator<
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
        let vault = store::resolve_vault(vault.as_deref());
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
