//! RFC-005 D7/D8: erasure and retention — the half of Law 8 that a version
//! engine most easily fakes.
//!
//! The obligations are: erase from *every* surface including derived ones,
//! refuse rather than half-erase, log the erasure, and prove it. Three of the
//! four were broken in the accepted RFC and fixed by its review; the comments
//! below name which, because each is a trap worth not falling into twice.

use pgrx::prelude::*;
use pgrx::{JsonB, Uuid};
use serde_json::json;

use crate::errors::{pm_error, Pm};
use crate::store::{self, arg};

/// Every text-bearing column in schema `pgmind`, enumerated from **pg_catalog**.
///
/// Not `information_schema`: those views are filtered by the current user's
/// privileges, and the RFC review measured the difference — the same sweep saw
/// 46 columns as the extension owner and 24 as a Law-11 erasure admin, so the
/// live `pgmind.block` lane was never opened and a canary still sitting in it
/// went unseen. `verify_excision` returned a positive attestation of erasure.
fn text_columns() -> Vec<TextColumn> {
    Spi::connect(|client| {
        client
            .select(
                "SELECT c.relname::text, a.attname::text, format_type(a.atttypid, NULL),
                        a.attgenerated <> '',
                        EXISTS (SELECT 1 FROM pg_catalog.pg_attribute v
                                 WHERE v.attrelid = c.oid AND v.attname = 'vault_id'
                                   AND v.attnum > 0 AND NOT v.attisdropped)
                   FROM pg_catalog.pg_class c
                   JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                   JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid
                  WHERE n.nspname = 'pgmind' AND c.relkind = 'r'
                    AND a.attnum > 0 AND NOT a.attisdropped
                    AND format_type(a.atttypid, NULL) IN
                        ('text','jsonb','text[]','character varying','bytea')
                  ORDER BY c.relname, a.attnum",
                None,
                &[],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure enumerating columns: {e}"))
            .map(|r| TextColumn {
                table: r.get::<String>(1).unwrap().unwrap(),
                column: r.get::<String>(2).unwrap().unwrap(),
                ty: r.get::<String>(3).unwrap().unwrap(),
                generated: r.get::<bool>(4).unwrap().unwrap_or(false),
                vault_scoped: r.get::<bool>(5).unwrap().unwrap_or(false),
            })
            .collect()
    })
}

/// One text-bearing column of the storage schema, as enumerated from
/// `pg_catalog` (never a literal list — see `sweep`).
struct TextColumn {
    table: String,
    column: String,
    ty: String,
    generated: bool,
    /// Whether the owning table carries `vault_id`, i.e. whether excision can
    /// confine itself to one tenant when it touches this column.
    vault_scoped: bool,
}

/// Tables excision does NOT touch, each named and each for a stated reason.
///
/// Excision operates on the note-content lanes, which are exactly the tables
/// carrying `vault_id`. These two carry none, and neither holds note content:
///
///   `excision_replay`  holds the executable excision target, which for the
///                      literal and note forms IS the identifying data the
///                      excision erased (RFC-005 D2 H4b).
///   `vault`            the registry: names and descriptions of containers,
///                      not content. Redacting it would rename vaults across
///                      the database and break every name-based lookup that
///                      depends on them — including, via `note.vault_id`'s
///                      foreign key, the notes themselves.
///
/// A list, not a filter, because anything else without `vault_id` is a table
/// that slipped past the boundary and must stop the transaction rather than be
/// swept unscoped — the silent-skip failure the D7 amendment exists for.
const NOT_CONTENT: [&str; 2] = ["excision_replay", "vault"];

/// `AND vault_id = $n`, or a hard stop.
///
/// Excision that leaves its own vault rewrites another tenant's bytes outside
/// the write path, recording no revision and leaving their notes failing
/// `verify_note`. So a table that cannot be scoped does not get scanned
/// unscoped — it aborts the transaction and names itself.
fn vault_predicate(col: &TextColumn, param: usize) -> String {
    if !col.vault_scoped {
        pgrx::error!(
            "pgmind: pgmind.{} carries no vault_id, so excision cannot confine itself \
             to one vault — refusing rather than sweeping every tenant",
            col.table
        );
    }
    format!("AND vault_id = ${param}")
}

/// Count occurrences of `needle` across every text-bearing column, skipping the
/// one table that holds the target by design (D2 H4b) — named, never silently
/// skipped, because a silent exemption is how this check reports green on a
/// broken system.
pub fn sweep(vault: Uuid, needle: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for col in text_columns() {
        if NOT_CONTENT.contains(&col.table.as_str()) {
            continue;
        }
        let TextColumn {
            table, column, ty, ..
        } = &col;
        let expr = match ty.as_str() {
            "text[]" => format!("array_to_string({column}, ' ')"),
            "bytea" => format!("encode({column}, 'escape')"),
            _ => format!("{column}::text"),
        };
        let scope = vault_predicate(&col, 2);
        let n: Option<i64> = Spi::get_one_with_args(
            &format!(
                "SELECT count(*) FROM pgmind.{table}
                  WHERE {expr} IS NOT NULL AND position($1 in {expr}) > 0 {scope}"
            ),
            &[needle.into(), arg(vault)],
        )
        .unwrap_or(Some(0));
        if n.unwrap_or(0) > 0 {
            hits.push(format!("{table}.{column}: {} row(s)", n.unwrap_or(0)));
        }
    }
    hits
}

#[pg_schema]
mod pgmind {
    use super::*;

    /// Erase content from history. `dry_run` defaults to **true**: erasure is
    /// never the accidental outcome of a typo.
    ///
    /// Live content is REFUSED (PM012) rather than silently spared — an erasure
    /// that left the current copy in place would be the worst possible outcome
    /// of a right-to-erasure request. `and_head => true` first performs an
    /// ordinary audited write that removes it, then erases history.
    #[pg_extern(requires = ["pgmind_storage"])]
    fn excise(
        target: JsonB,
        reason: &str,
        and_head: default!(bool, false),
        dry_run: default!(bool, true),
    ) -> Uuid {
        let vault = store::current_vault();
        let obj = target.0.clone();
        let (kind, literal) = match (
            obj.get("literal").and_then(|v| v.as_str()),
            obj.get("path").and_then(|v| v.as_str()),
            obj.get("block_id").and_then(|v| v.as_str()),
        ) {
            (Some(l), _, _) => ("literal", l.to_string()),
            (_, Some(p), _) => ("note", p.to_string()),
            (_, _, Some(b)) => ("block", b.to_string()),
            _ => pm_error(
                Pm::ExcisionRefused,
                "target must name a literal, a path, or a block_id",
                "excise",
            ),
        };

        // Refuse while the content is still live at head.
        let live = live_hits(vault, kind, &literal);
        if live > 0 && !and_head {
            pm_error(
                Pm::ExcisionRefused,
                "target is still live at head",
                &format!("{live} live row(s); pass and_head => true to remove it first"),
            );
        }
        let history_hits = sweep(vault, &literal).len() as i64;
        if dry_run {
            pgrx::warning!(
                "pgmind: dry run — would erase {live} live and touch {history_hits} history surface(s) for {kind} target"
            );
            return Uuid::from_bytes([0; 16]);
        }

        let id = crate::ids::mint();
        Spi::run_with_args(
            "INSERT INTO pgmind.excision_log (id, vault_id, reason, target_kind, scope)
             VALUES ($1, $2, $3, $4, $5)",
            &[
                arg(id),
                arg(vault),
                reason.into(),
                kind.into(),
                JsonB(json!({"live_rows": live, "history_surfaces": history_hits})).into(),
            ],
        )
        .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure logging excision: {e}"));
        // The executable target lives OUTSIDE the dumped audit row: for the
        // literal and note forms it IS the identifying data, and the audit
        // table is the one table guaranteed to be reproduced in every backup.
        Spi::run_with_args(
            "INSERT INTO pgmind.excision_replay (excision_id, target) VALUES ($1, $2)",
            &[arg(id), JsonB(obj).into()],
        )
        .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure recording replay target: {e}"));

        if and_head && live > 0 {
            remove_live(vault, kind, &literal);
        }
        redact(vault, &literal, id);

        // Verification runs INSIDE the transaction: survivors abort it, so an
        // incomplete excision is never committed and no log row survives one.
        let survivors = sweep(vault, &literal);
        if !survivors.is_empty() {
            pm_error(
                Pm::ExcisionIncomplete,
                "excision left the content in place",
                &format!("survivors: {}", survivors.join("; ")),
            );
        }
        Spi::run_with_args(
            "UPDATE pgmind.excision_log SET verified_at = now(), survivors = 0 WHERE id = $1",
            &[arg(id)],
        )
        .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure closing excision: {e}"));
        id
    }

    /// Empty result ⇒ proven erased. Sweeps from `pg_catalog`, and asserts it
    /// actually looked at something: an under-enumerating sweep is the exact
    /// way this function would report success on a broken system.
    #[pg_extern(requires = ["pgmind_storage"])]
    fn verify_excision(excision: Uuid) -> pgrx::iter::SetOfIterator<'static, String> {
        // Scoped to the CURRENT vault, and deliberately not to the vault
        // recorded on the excision: verifying somebody else's erasure is
        // reading whether their content survived. An excision belonging to
        // another vault is reported exactly as one that never existed.
        let vault = store::current_vault();
        let known: Option<Uuid> = Spi::get_one_with_args(
            "SELECT id FROM pgmind.excision_log WHERE id = $1 AND vault_id = $2",
            &[arg(excision), arg(vault)],
        )
        .unwrap_or(None);
        if known.is_none() {
            return pgrx::iter::SetOfIterator::new(vec![format!(
                "no excision {excision} in this vault"
            )]);
        }
        let target: Option<JsonB> = Spi::get_one_with_args(
            "SELECT target FROM pgmind.excision_replay WHERE excision_id = $1",
            &[arg(excision)],
        )
        .unwrap_or(None);
        let Some(t) = target else {
            return pgrx::iter::SetOfIterator::new(vec![format!(
                "no replay target for {excision} — it was excised from a restored dump, \
                 or the excision predates this database"
            )]);
        };
        let needle =
            t.0.get("literal")
                .or_else(|| t.0.get("path"))
                .or_else(|| t.0.get("block_id"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
        let scanned = super::text_columns().len();
        let mut out = super::sweep(vault, &needle);
        if scanned == 0 {
            out.push("swept zero columns — the enumeration is broken".into());
        }
        pgrx::iter::SetOfIterator::new(out)
    }

    /// RFC-005 D8: bound history. A function with parameters, not a stored
    /// policy catalog — there is no persisted mode that can be toggled into
    /// deleting more than the caller asked for, and no background job that runs
    /// when nobody is watching.
    #[pg_extern(requires = ["pgmind_storage"])]
    fn retain(keep_revisions: default!(Option<i64>, "NULL"), dry_run: default!(bool, true)) -> i64 {
        let vault = store::current_vault();
        let keep = keep_revisions.unwrap_or(i64::MAX).max(1);
        let notes: Vec<(Uuid, i64, i64)> = Spi::connect(|client| {
            client
                .select(
                    "SELECT n.id,
                            (SELECT seq FROM pgmind.revision WHERE id = n.head_revision),
                            n.history_floor
                       FROM pgmind.note n WHERE n.vault_id = $1",
                    None,
                    &[arg(vault)],
                )
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure listing notes: {e}"))
                .map(|r| {
                    (
                        r.get::<Uuid>(1).unwrap().unwrap(),
                        r.get::<i64>(2).unwrap().unwrap_or(0),
                        r.get::<i64>(3).unwrap().unwrap_or(0),
                    )
                })
                .collect()
        });
        let mut removed = 0i64;
        for (note_id, head_seq, floor) in notes {
            let new_floor = (head_seq - keep + 1).max(floor);
            if new_floor <= floor {
                continue;
            }
            if dry_run {
                removed += count_below(note_id, new_floor);
                continue;
            }
            // A frame AT the new floor first: it is the anchor every read at or
            // above the floor depends on, and writing it after the delete would
            // leave a window with no anchor at all.
            let note = store::note_by_id_any(note_id)
                .unwrap_or_else(|| pgrx::error!("pgmind: note vanished during retain"));
            let st = crate::timetravel::state_at(&note, new_floor);
            write_floor_frame(&note, new_floor, &st);
            removed += count_below(note_id, new_floor);
            for stmt in [
                "DELETE FROM pgmind.block_revision WHERE note_id = $1 AND seq < $2",
                "DELETE FROM pgmind.note_revision  WHERE note_id = $1 AND seq < $2",
                // Frames below the floor are deleted too: leaving them would
                // keep full snapshots of exactly the content PM011 claims is
                // no longer reconstructable.
                "DELETE FROM pgmind.note_frame     WHERE note_id = $1 AND seq < $2",
            ] {
                Spi::run_with_args(stmt, &[arg(note_id), new_floor.into()])
                    .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure compacting: {e}"));
            }
            // pgmind.revision rows are NEVER deleted: a vault that forgot must
            // remember that it forgot, and a client holding a compacted id gets
            // PM011 ("no longer reconstructable"), not PM010 ("no such
            // revision") — those mean opposite things to whoever debugs them.
            Spi::run_with_args(
                "UPDATE pgmind.note SET history_floor = $2 WHERE id = $1",
                &[arg(note_id), new_floor.into()],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure raising floor: {e}"));
        }
        removed
    }
}

fn count_below(note_id: Uuid, floor: i64) -> i64 {
    Spi::get_one_with_args(
        "SELECT (SELECT count(*) FROM pgmind.block_revision WHERE note_id = $1 AND seq < $2)
              + (SELECT count(*) FROM pgmind.note_revision  WHERE note_id = $1 AND seq < $2)",
        &[arg(note_id), floor.into()],
    )
    .unwrap_or(Some(0))
    .unwrap_or(0)
}

fn write_floor_frame(note: &store::NoteRow, seq: i64, st: &crate::timetravel::StateAt) {
    let kinds: Vec<&str> = st
        .ids
        .iter()
        .map(|id| {
            st.blocks
                .get(id.as_bytes())
                .map(|b| b.kind.as_str())
                .unwrap_or("paragraph")
        })
        .collect();
    let contents: Vec<&str> = st
        .ids
        .iter()
        .map(|id| {
            st.blocks
                .get(id.as_bytes())
                .map(|b| b.content.as_str())
                .unwrap_or("")
        })
        .collect();
    let refs: Vec<String> = st
        .ids
        .iter()
        .map(|id| {
            st.blocks
                .get(id.as_bytes())
                .and_then(|b| b.block_ref_id.clone())
                .unwrap_or_default()
        })
        .collect();
    let parents: Vec<Option<Uuid>> = st
        .ids
        .iter()
        .map(|id| st.blocks.get(id.as_bytes()).and_then(|b| b.parent))
        .collect();
    let attrs: Vec<serde_json::Value> = st
        .ids
        .iter()
        .map(|id| {
            st.blocks
                .get(id.as_bytes())
                .map(|b| b.attrs.clone())
                .unwrap_or(serde_json::Value::Null)
        })
        .collect();
    let placement: Vec<i32> = st.place.iter().flat_map(|&(t, s, e)| [t, s, e]).collect();
    let tiles: Vec<&str> = st.tiles.iter().map(|s| s.as_str()).collect();
    Spi::run_with_args(
        "INSERT INTO pgmind.note_frame
           (note_id, seq, vault_id, path, preamble, props, tiles, block_ids, kinds,
            contents, head_paths, parents, block_refs, attrs, placement)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
         ON CONFLICT (note_id, seq) DO NOTHING",
        &[
            arg(note.id),
            seq.into(),
            arg(note.vault_id),
            st.path.as_str().into(),
            st.preamble.as_str().into(),
            JsonB(st.props.clone()).into(),
            tiles.into(),
            st.ids.clone().into(),
            kinds.into(),
            contents.into(),
            JsonB(json!(st.heads)).into(),
            parents.into(),
            refs.into(),
            JsonB(json!(attrs)).into(),
            placement.into(),
        ],
    )
    .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure writing floor frame: {e}"));
}

fn live_hits(vault: Uuid, kind: &str, needle: &str) -> i64 {
    let sql = match kind {
        "note" => "SELECT count(*) FROM pgmind.note WHERE vault_id = $1 AND path = $2 AND tombstoned_at IS NULL",
        "block" => "SELECT count(*) FROM pgmind.block WHERE vault_id = $1 AND id::text = $2",
        _ => "SELECT count(*) FROM pgmind.block WHERE vault_id = $1 AND position($2 in content) > 0",
    };
    Spi::get_one_with_args(sql, &[arg(vault), needle.into()])
        .unwrap_or(Some(0))
        .unwrap_or(0)
}

fn remove_live(vault: Uuid, kind: &str, needle: &str) {
    match kind {
        "note" => {
            Spi::run_with_args("SELECT knowledge.delete_note($1)", &[needle.into()])
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure removing live note: {e}"));
        }
        _ => {
            // Rewrite each affected note through the ordinary write path, so the
            // removal is an audited revision like any other edit.
            let paths: Vec<String> = Spi::connect(|client| {
                client
                    .select(
                        "SELECT DISTINCT n.path FROM pgmind.note n
                           JOIN pgmind.block b ON b.note_id = n.id
                          WHERE n.vault_id = $1 AND n.tombstoned_at IS NULL
                            AND (b.id::text = $2 OR position($2 in b.content) > 0)",
                        None,
                        &[arg(vault), needle.into()],
                    )
                    .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure finding live notes: {e}"))
                    .map(|r| r.get::<String>(1).unwrap().unwrap())
                    .collect()
            });
            for p in paths {
                let note = store::note_by_path_or_err(vault, &p);
                let src = store::load_source(&note);
                let cleaned = src.replace(needle, "");
                crate::write::write_note_in(vault, &p, &cleaned, None);
            }
        }
    }
}

/// Redact the content from every history surface. The byte lane is spliced
/// rather than NULLed: a tile pre-image holds a whole top-level child, so
/// nulling it would erase every OTHER block that shared the tile and leave
/// `read_as_of` reassembling a document with a hole.
fn redact(vault: Uuid, needle: &str, excision: Uuid) {
    let marker = format!("⟨redacted pgmind:{excision}⟩");
    for col in text_columns() {
        // Generated columns cannot be updated, and need not be: they are
        // derived from a column this sweep already redacts (note.basename from
        // note.path). They stay in the SWEEP, which is what must be exhaustive.
        if NOT_CONTENT.contains(&col.table.as_str()) || col.generated {
            continue;
        }
        let scope = vault_predicate(&col, 3);
        let TextColumn {
            table, column, ty, ..
        } = &col;
        let sql = match (ty.as_str(), column.as_str()) {
            // Effect rows keep their skeleton so history still knows THAT a
            // block existed and when; `redacted` marks the NULLs as erasure
            // rather than "unchanged by this revision".
            ("text", "prev_content") => format!(
                "UPDATE pgmind.{table} SET prev_content = NULL, prev_content_hash = NULL,
                        prev_attrs = NULL, prev_block_ref_id = NULL, redacted = true
                  WHERE position($1 in prev_content) > 0 {scope}"
            ),
            ("text[]", _) => format!(
                "UPDATE pgmind.{table} SET {column} = (
                     SELECT array_agg(replace(e, $1, $2) ORDER BY o)
                       FROM unnest({column}) WITH ORDINALITY AS u(e, o))
                  WHERE array_to_string({column}, ' ') LIKE '%' || $1 || '%' {scope}"
            ),
            ("text", _) => format!(
                "UPDATE pgmind.{table} SET {column} = replace({column}, $1, $2)
                  WHERE position($1 in {column}) > 0 {scope}"
            ),
            ("jsonb", _) => format!(
                "UPDATE pgmind.{table} SET {column} = replace({column}::text, $1, $2)::jsonb
                  WHERE position($1 in {column}::text) > 0 {scope}"
            ),
            _ => continue,
        };
        Spi::run_with_args(&sql, &[needle.into(), marker.as_str().into(), arg(vault)])
            .unwrap_or_else(|e| {
                pgrx::error!("pgmind: SPI failure redacting {table}.{column}: {e}")
            });
    }
}
