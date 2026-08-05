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

fn parse_uuid(s: &str) -> Option<Uuid> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || s.chars().filter(|c| *c == '-').count() > 4 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for i in 0..16 {
        bytes[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(Uuid::from_bytes(bytes))
}

#[derive(Debug, Clone)]
pub struct NoteRow {
    pub id: Uuid,
    pub head_revision: Uuid,
    pub preamble: String,
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
pub fn note_by_path(vault: Uuid, path: &str) -> Option<NoteRow> {
    Spi::connect(|client| {
        let rows = client
            .select(
                "SELECT id, head_revision, preamble FROM pgmind.note
                 WHERE vault_id = $1 AND path = $2 AND tombstoned_at IS NULL",
                Some(1),
                &[arg(vault), path.into()],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in note_by_path: {e}"));
        if rows.is_empty() {
            return None;
        }
        let row = rows.first();
        Some(NoteRow {
            id: row.get(1).unwrap().unwrap(),
            head_revision: row.get(2).unwrap().unwrap(),
            preamble: row.get::<String>(3).unwrap().unwrap(),
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

/// Full source of a note: preamble ‖ tiles (RFC-003 D2 invariant).
pub fn source_of(note: &NoteRow) -> String {
    let mut s = note.preamble.clone();
    for t in tiles_of(note.id) {
        s.push_str(&t);
    }
    s
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
    let base = pgmind_core::path::basename(new_path);
    let affected: Vec<(i64, String)> = Spi::connect(|client| {
        client
            .select(
                "SELECT id, dst_path FROM pgmind.edge
                 WHERE vault_id = $1 AND (dst_path = $2 OR dst_path = $3)",
                None,
                &[arg(vault), new_path.into(), base.into()],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in repair: {e}"))
            .map(|row| {
                (
                    row.get::<i64>(1).unwrap().unwrap(),
                    row.get::<String>(2).unwrap().unwrap(),
                )
            })
            .collect()
    });
    for (edge_id, dst_path) in affected {
        let r = resolve_target(vault, &dst_path);
        update_edge_resolution(edge_id, r);
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
