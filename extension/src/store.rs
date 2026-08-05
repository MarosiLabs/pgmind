//! SPI-backed storage access: current vault, note/block/tile/edge/tag rows,
//! and the D8 link-target resolver (RFC-003 D5). All queries are scoped to a
//! vault; nothing here parses markdown or decides identity.

use pgrx::datum::DatumWithOid;
use pgrx::prelude::*;
use pgrx::{JsonB, Uuid};

use crate::errors::{pm_error, Pm};
use crate::VAULT_ID_GUC;

pub fn default_vault() -> Uuid {
    Uuid::from_bytes([0u8; 16])
}

/// The current vault from GUC `pgmind.vault_id` (RFC-003 D1).
pub fn current_vault() -> Uuid {
    let Some(setting) = VAULT_ID_GUC.get() else {
        return default_vault();
    };
    let s = setting.to_str().unwrap_or("");
    parse_uuid(s).unwrap_or_else(|| {
        pm_error(
            Pm::InvalidPath,
            "malformed pgmind.vault_id GUC",
            &format!("value {s:?} is not a UUID"),
        )
    })
}

/// Parse the `pgmind.vault_id` GUC the way PostgreSQL's own `uuid` input does,
/// so the extension and the RLS predicate RFC-003 D1 documents
/// (`vault_id = current_setting('pgmind.vault_id')::uuid`) can never disagree
/// about which vault a session is in.
///
/// Accepts the canonical `8-4-4-4-12` dashed form and the bare 32-hex form,
/// each optionally brace-wrapped; rejects everything else. Two specific
/// hazards this closes: `u8::from_str_radix` accepts a leading `+`, so
/// `'+0+0…'` used to parse as the all-zeros DEFAULT vault instead of raising
/// PM001; and the old length gate counted BYTES while the slice indexed
/// bytes, so a 32-byte multibyte value panicked mid-character. This walks
/// `chars`, never byte offsets.
fn parse_uuid(s: &str) -> Option<Uuid> {
    let body = s
        .strip_prefix('{')
        .and_then(|b| b.strip_suffix('}'))
        .unwrap_or(s);
    let mut nibbles = [0u8; 32];
    let mut n = 0usize;
    let mut dashes = 0usize;
    for (i, c) in body.chars().enumerate() {
        if c == '-' {
            // Canonical positions only — the old check merely capped the count.
            if !matches!(i, 8 | 13 | 18 | 23) {
                return None;
            }
            dashes += 1;
            continue;
        }
        if n == 32 {
            return None;
        }
        nibbles[n] = c.to_digit(16)? as u8;
        n += 1;
    }
    if n != 32 || !(dashes == 0 || dashes == 4) {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (nibbles[i * 2] << 4) | nibbles[i * 2 + 1];
    }
    Some(Uuid::from_bytes(bytes))
}

#[derive(Debug, Clone)]
pub struct NoteRow {
    pub id: Uuid,
    pub vault_id: Uuid,
    pub head_revision: Uuid,
    pub preamble: String,
    pub tombstoned: bool,
}

/// Split a link anchor into its `(dst_heading, dst_block_ref)` storage
/// columns; `^x` is a block ref, anything else a heading, empty stands in for
/// NULL. Paired with [`join_anchor`] so the encoding is stated once — the
/// write path, `verify_note`, and the read APIs must not each carry their own
/// copy of this rule or `verify_note` ends up validating the write path with
/// the write path's own logic.
pub fn split_anchor(anchor: &Option<String>) -> (String, String) {
    match anchor {
        Some(a) => match a.strip_prefix('^') {
            Some(r) => (String::new(), r.to_string()),
            None => (a.clone(), String::new()),
        },
        None => (String::new(), String::new()),
    }
}

/// Inverse of [`split_anchor`], for the read APIs.
pub fn join_anchor(heading: Option<String>, block_ref: Option<String>) -> Option<String> {
    block_ref.map(|r| format!("^{r}")).or(heading)
}

#[derive(Debug, Clone)]
pub struct BlockRow {
    pub id: Uuid,
    pub ord: i32,
    pub parent_block: Option<Uuid>,
    pub kind: String,
    pub heading_path: Vec<String>,
    pub content: String,
    pub content_hash: Vec<u8>,
    pub block_ref_id: Option<String>,
    pub tile_ord: i32,
    pub start_in_tile: i32,
    pub end_in_tile: i32,
    pub attrs: serde_json::Value,
}

pub fn arg(u: Uuid) -> DatumWithOid<'static> {
    u.into()
}

/// Live note at `path` in the current vault.
///
/// Normalizes here rather than at each entry point: `write()` stores the
/// NFC-trimmed spelling (RFC-003 D5), so a reader that passed the caller's raw
/// text straight into `path = $2` could not find a note it had just written —
/// macOS hands out NFD, and every path-taking read API goes through here.
pub fn note_by_path(vault: Uuid, path: &str) -> Option<NoteRow> {
    let path = pgmind_core::path::path_normalize(path);
    Spi::connect(|client| {
        let rows = client
            .select(
                "SELECT id, head_revision, preamble FROM pgmind.note
                 WHERE vault_id = $1 AND path = $2 AND tombstoned_at IS NULL",
                Some(1),
                &[arg(vault), path.as_str().into()],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in note_by_path: {e}"));
        if rows.is_empty() {
            return None;
        }
        let row = rows.first();
        Some(NoteRow {
            id: row.get(1).unwrap().unwrap(),
            vault_id: vault,
            head_revision: row.get(2).unwrap().unwrap(),
            preamble: row.get::<String>(3).unwrap().unwrap(),
            tombstoned: false,
        })
    })
}

/// The same row reached by id. The block ops and `verify_note` address notes
/// by id; both used to hand-roll this query and the preamble‖tiles
/// concatenation, which is how they came to disagree with `note_by_path` about
/// liveness. Tombstone state is reported, not filtered: `verify_note` is a
/// debug checker and must still be able to inspect a retired note.
pub fn note_by_id(note_id: Uuid) -> Option<NoteRow> {
    Spi::connect(|client| {
        let rows = client
            .select(
                "SELECT id, vault_id, head_revision, preamble, tombstoned_at
                 FROM pgmind.note WHERE id = $1",
                Some(1),
                &[arg(note_id)],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in note_by_id: {e}"));
        if rows.is_empty() {
            return None;
        }
        let row = rows.first();
        Some(NoteRow {
            id: row.get(1).unwrap().unwrap(),
            vault_id: row.get(2).unwrap().unwrap(),
            head_revision: row.get(3).unwrap().unwrap(),
            preamble: row.get::<String>(4).unwrap().unwrap(),
            tombstoned: row
                .get::<pgrx::datum::TimestampWithTimeZone>(5)
                .unwrap()
                .is_some(),
        })
    })
}

pub fn note_by_path_or_err(vault: Uuid, path: &str) -> NoteRow {
    note_by_path(vault, path).unwrap_or_else(|| {
        pm_error(
            Pm::NoteNotFound,
            "note not found",
            &format!("path {path:?}"),
        )
    })
}

/// All tiles of a note, in ord order.
pub fn tiles_of(note: Uuid) -> Vec<String> {
    Spi::connect(|client| {
        client
            .select(
                "SELECT raw FROM pgmind.tile WHERE note_id = $1 ORDER BY ord",
                None,
                &[arg(note)],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in tiles_of: {e}"))
            .map(|row| row.get::<String>(1).unwrap().unwrap())
            .collect()
    })
}

/// Full source of a note: preamble ‖ tiles (RFC-003 D2 invariant). Takes the
/// tiles the caller already holds — the write path, the block ops and
/// `verify_note` all need both, and fetching them twice meant a second full
/// TOAST read and decompression of the whole document per operation.
pub fn source_of(note: &NoteRow, tiles: &[String]) -> String {
    let mut s =
        String::with_capacity(note.preamble.len() + tiles.iter().map(String::len).sum::<usize>());
    s.push_str(&note.preamble);
    for t in tiles {
        s.push_str(t);
    }
    s
}

/// `source_of` for callers that do not otherwise need the tiles.
pub fn load_source(note: &NoteRow) -> String {
    source_of(note, &tiles_of(note.id))
}

/// All block rows of a note, in ord order.
pub fn blocks_of(note: Uuid) -> Vec<BlockRow> {
    Spi::connect(|client| {
        client
            .select(
                "SELECT id, ord, parent_block, kind::text, heading_path, content,
                        content_hash, block_ref_id, tile_ord, start_in_tile, end_in_tile,
                        attrs
                 FROM pgmind.block WHERE note_id = $1 ORDER BY ord",
                None,
                &[arg(note)],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in blocks_of: {e}"))
            .map(|row| BlockRow {
                id: row.get(1).unwrap().unwrap(),
                ord: row.get(2).unwrap().unwrap(),
                parent_block: row.get(3).unwrap(),
                kind: row.get(4).unwrap().unwrap(),
                heading_path: row.get(5).unwrap().unwrap(),
                content: row.get(6).unwrap().unwrap(),
                content_hash: row.get(7).unwrap().unwrap(),
                block_ref_id: row.get(8).unwrap(),
                tile_ord: row.get(9).unwrap().unwrap(),
                start_in_tile: row.get(10).unwrap().unwrap(),
                end_in_tile: row.get(11).unwrap().unwrap(),
                attrs: row.get::<JsonB>(12).unwrap().unwrap().0,
            })
            .collect()
    })
}

/// Outcome of D8 note-level link resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Exact(Uuid),
    Basename(Uuid),
    Missing,
    Ambiguous,
    Invalid,
}

impl Resolution {
    pub fn dst_note(&self) -> Option<Uuid> {
        match self {
            Resolution::Exact(id) | Resolution::Basename(id) => Some(*id),
            _ => None,
        }
    }
    pub fn via(&self) -> Option<&'static str> {
        match self {
            Resolution::Exact(_) => Some("exact"),
            Resolution::Basename(_) => Some("basename"),
            _ => None,
        }
    }
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Resolution::Missing => Some("missing"),
            Resolution::Ambiguous => Some("ambiguous"),
            Resolution::Invalid => Some("invalid"),
            _ => None,
        }
    }
}

/// RFC-002 D8: exact path match; else unique live-basename match for slash-free
/// targets; else dangling. The target arrives NFC-trimmed from extraction.
pub fn resolve_target(vault: Uuid, target: &str) -> Resolution {
    if !pgmind_core::path::path_is_valid(target) {
        return Resolution::Invalid;
    }
    let exact: Option<Uuid> = Spi::connect(|client| {
        client
            .select(
                "SELECT id FROM pgmind.note
                 WHERE vault_id = $1 AND path = $2 AND tombstoned_at IS NULL",
                Some(1),
                &[arg(vault), target.into()],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in resolve_target: {e}"))
            .map(|row| row.get::<Uuid>(1).unwrap().unwrap())
            .next()
    });
    if let Some(id) = exact {
        return Resolution::Exact(id);
    }
    if target.contains('/') {
        return Resolution::Missing;
    }
    let candidates: Vec<Uuid> = Spi::connect(|client| {
        client
            .select(
                "SELECT id FROM pgmind.note
                 WHERE vault_id = $1 AND basename = $2 AND tombstoned_at IS NULL
                 ORDER BY path LIMIT 2",
                None,
                &[arg(vault), target.into()],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in resolve_target: {e}"))
            .map(|row| row.get::<Uuid>(1).unwrap().unwrap())
            .collect()
    });
    match candidates.len() {
        0 => Resolution::Missing,
        1 => Resolution::Basename(candidates[0]),
        _ => Resolution::Ambiguous,
    }
}

/// RFC-003 D5 creation repair: re-run D8 resolution for every edge the new
/// note could affect (dst_path = new path, or slash-free dst_path = new
/// basename). Definitionally equal to full recomputation.
pub fn repair_edges_on_creation(vault: Uuid, new_path: &str) {
    // The affected set is defined by exactly two literal dst_path values, so
    // resolution is loop-invariant: resolve each once and apply it set-wise
    // rather than re-resolving per edge (fan-in used to cost 3 statements per
    // linking note).
    let base = pgmind_core::path::basename(new_path);
    let mut targets: Vec<&str> = vec![new_path];
    if base != new_path {
        targets.push(base);
    }
    for target in targets {
        let r = resolve_target(vault, target);
        update_edge_resolution_by_path(vault, target, r);
    }
}

/// Apply one resolution to every edge in the vault pointing at `dst_path`.
fn update_edge_resolution_by_path(vault: Uuid, dst_path: &str, r: Resolution) {
    Spi::run_with_args(
        "UPDATE pgmind.edge
         SET dst_note = $3, resolved_via = $4, dangling_reason = $5
         WHERE vault_id = $1 AND dst_path = $2
           AND (dst_note IS DISTINCT FROM $3
             OR resolved_via IS DISTINCT FROM $4
             OR dangling_reason IS DISTINCT FROM $5)",
        &[
            arg(vault),
            dst_path.into(),
            r.dst_note()
                .map(DatumWithOid::from)
                .unwrap_or_else(DatumWithOid::null::<Uuid>),
            r.via()
                .map(DatumWithOid::from)
                .unwrap_or_else(DatumWithOid::null::<String>),
            r.reason()
                .map(DatumWithOid::from)
                .unwrap_or_else(DatumWithOid::null::<String>),
        ],
    )
    .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure updating edge resolution: {e}"));
}

#[cfg(test)]
mod parse_uuid_tests {
    use super::parse_uuid;

    const NIL: [u8; 16] = [0u8; 16];

    #[test]
    fn accepts_what_postgres_accepts() {
        for s in [
            "00000000-0000-0000-0000-000000000000",
            "00000000000000000000000000000000",
            "{00000000-0000-0000-0000-000000000000}",
            "{00000000000000000000000000000000}",
        ] {
            assert_eq!(parse_uuid(s).map(|u| *u.as_bytes()), Some(NIL), "{s:?}");
        }
        assert_eq!(
            parse_uuid("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11").map(|u| u.as_bytes()[0]),
            Some(0xA0)
        );
    }

    #[test]
    fn rejects_what_postgres_rejects() {
        for s in [
            // `u8::from_str_radix` accepted the '+', silently yielding the
            // DEFAULT vault while the RLS `::uuid` cast raised 22P02.
            "+0+0+0+0+0+0+0+0+0+0+0+0+0+0+0+0",
            "----00000000000000000000000000000000",
            "0-0000000-00000000000000000000000-0",
            // 32 BYTES but 12 chars: the old byte-slicing panicked here.
            "€€€€€€€€€€ab",
            "aéaéaéaéaéaéaéaéaéaéaéaéaéaéaé00",
            "",
            "0000000000000000000000000000000",   // 31
            "000000000000000000000000000000000", // 33
            "0000000g-0000-0000-0000-000000000000",
            "{00000000-0000-0000-0000-000000000000",
            " 00000000-0000-0000-0000-000000000000",
        ] {
            assert!(parse_uuid(s).is_none(), "should reject {s:?}");
        }
    }
}

pub fn update_edge_resolution(edge_id: i64, r: Resolution) {
    Spi::run_with_args(
        "UPDATE pgmind.edge
         SET dst_note = $2, resolved_via = $3, dangling_reason = $4
         WHERE id = $1
           AND (dst_note IS DISTINCT FROM $2
             OR resolved_via IS DISTINCT FROM $3
             OR dangling_reason IS DISTINCT FROM $4)",
        &[
            edge_id.into(),
            r.dst_note()
                .map(DatumWithOid::from)
                .unwrap_or_else(DatumWithOid::null::<Uuid>),
            r.via()
                .map(DatumWithOid::from)
                .unwrap_or_else(DatumWithOid::null::<String>),
            r.reason()
                .map(DatumWithOid::from)
                .unwrap_or_else(DatumWithOid::null::<String>),
        ],
    )
    .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure updating edge resolution: {e}"));
}
