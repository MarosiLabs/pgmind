//! `pgmind.verify_note` (RFC-003 D7): the admin/debug invariant checker
//! (Law 11 — marked as such). Recomputes the parse from stored bytes and
//! reports every disagreement between the lanes, the extraction indexes,
//! and a fresh deterministic recomputation. Empty result = healthy.

use pgrx::iter::SetOfIterator;
use pgrx::prelude::*;
use pgrx::Uuid;

use crate::store::{self, arg};
use crate::write::parse_note;

#[pg_schema]
mod pgmind {
    use super::*;

    #[pg_extern]
    fn verify_note(note_id: Uuid) -> SetOfIterator<'static, String> {
        super::verify_note_impl(note_id)
    }
}

fn verify_note_impl(note_id: Uuid) -> SetOfIterator<'static, String> {
    let mut v: Vec<String> = Vec::new();

    let Some(note) = store::note_by_id(note_id) else {
        return SetOfIterator::new(vec![format!("note {note_id} does not exist")]);
    };
    let (head, preamble, vault) = (note.head_revision, note.preamble.clone(), note.vault_id);

    // Head revision must exist and belong to this note (no FK by design — D3).
    let head_ok: Option<bool> = Spi::get_one_with_args(
        "SELECT note_id = $2 FROM pgmind.revision WHERE id = $1",
        &[arg(head), arg(note_id)],
    )
    .unwrap_or(None);
    match head_ok {
        Some(true) => {}
        Some(false) => v.push(format!("head_revision {head} belongs to a different note")),
        None => v.push(format!("head_revision {head} has no revision row")),
    }

    // Reconstruct and re-parse; the parse is the truth the lanes must mirror.
    let tiles = store::tiles_of(note_id);
    let source = store::source_of(&note, &tiles);
    let parsed = parse_note(&source);

    if parsed.doc.preamble.end != preamble.len() {
        v.push(format!(
            "preamble length {} disagrees with parse ({})",
            preamble.len(),
            parsed.doc.preamble.end
        ));
    }
    if parsed.tiles.len() != tiles.len() {
        v.push(format!(
            "tile count {} disagrees with parse ({})",
            tiles.len(),
            parsed.tiles.len()
        ));
    } else {
        for (i, (raw, stored)) in parsed.tiles.iter().zip(tiles.iter()).enumerate() {
            if raw != stored {
                v.push(format!("tile {i} bytes disagree with parse"));
            }
        }
    }

    // Block rows must equal the parse, ord-dense, with consistent placement.
    let rows = store::blocks_of(note_id);
    if rows.len() != parsed.doc.blocks.len() {
        v.push(format!(
            "block row count {} disagrees with parse ({})",
            rows.len(),
            parsed.doc.blocks.len()
        ));
    } else {
        let by_id: std::collections::HashMap<[u8; 16], usize> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| (*r.id.as_bytes(), i))
            .collect();
        for (i, (row, nb)) in rows.iter().zip(parsed.doc.blocks.iter()).enumerate() {
            let ord = i as i32;
            if row.ord != ord {
                v.push(format!(
                    "block {} ord {} not dense at {}",
                    row.id, row.ord, ord
                ));
            }
            if row.kind != nb.kind.tag() {
                v.push(format!(
                    "block {} kind {} vs parse {}",
                    row.id,
                    row.kind,
                    nb.kind.tag()
                ));
            }
            if row.content != nb.normalized_content {
                v.push(format!("block {} content disagrees with parse", row.id));
            }
            if row.content_hash != nb.content_hash.to_vec() {
                v.push(format!("block {} hash disagrees with parse", row.id));
            }
            if row.heading_path != nb.heading_path {
                v.push(format!(
                    "block {} heading_path disagrees with parse",
                    row.id
                ));
            }
            if row.block_ref_id != nb.block_ref_id {
                v.push(format!(
                    "block {} block_ref_id disagrees with parse",
                    row.id
                ));
            }
            if row.attrs != nb.attrs {
                v.push(format!("block {} attrs disagree with parse", row.id));
            }
            let (t, s, e) = parsed.placement[i];
            if (row.tile_ord, row.start_in_tile, row.end_in_tile) != (t, s, e) {
                v.push(format!("block {} placement disagrees with parse", row.id));
            }
            let expect_parent = nb.parent.map(|p| *rows[p as usize].id.as_bytes());
            if row.parent_block.map(|u| *u.as_bytes()) != expect_parent {
                v.push(format!(
                    "block {} parent_block disagrees with parse",
                    row.id
                ));
            }
            if let Some(pid) = row.parent_block {
                if !by_id.contains_key(pid.as_bytes()) {
                    v.push(format!("block {} parent {} not in this note", row.id, pid));
                }
            }
        }
    }

    // Extraction indexes must equal a fresh recomputation, resolution included
    // (D5: incremental maintenance ≡ full recomputation).
    {
        use std::collections::BTreeSet;
        let ords_ok = rows.len() == parsed.doc.blocks.len();
        if ords_ok {
            let mut want_edges: BTreeSet<String> = BTreeSet::new();
            for l in &parsed.doc.links {
                let kind = l.kind.tag();
                let (heading, block_ref) = store::split_anchor(&l.anchor);
                let r = store::resolve_target(vault, &l.target);
                want_edges.insert(format!(
                    "{}|{kind}|{}|{heading}|{block_ref}|{}|{:?}|{:?}|{:?}",
                    rows[l.block as usize].id,
                    l.target,
                    l.alias.clone().unwrap_or_default(),
                    r.dst_note().map(|u| u.to_string()),
                    r.via(),
                    r.reason(),
                ));
            }
            let have_edges: BTreeSet<String> = Spi::connect(|client| {
                client
                    .select(
                        "SELECT src_block::text, kind::text, dst_path,
                                coalesce(dst_heading,''), coalesce(dst_block_ref,''),
                                coalesce(alias,''), dst_note::text, resolved_via, dangling_reason
                         FROM pgmind.edge WHERE src_note = $1",
                        None,
                        &[arg(note_id)],
                    )
                    .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in verify_note: {e}"))
                    .map(|row| {
                        format!(
                            "{}|{}|{}|{}|{}|{}|{:?}|{:?}|{:?}",
                            row.get::<String>(1).unwrap().unwrap(),
                            row.get::<String>(2).unwrap().unwrap(),
                            row.get::<String>(3).unwrap().unwrap(),
                            row.get::<String>(4).unwrap().unwrap(),
                            row.get::<String>(5).unwrap().unwrap(),
                            row.get::<String>(6).unwrap().unwrap(),
                            row.get::<String>(7).unwrap(),
                            row.get::<String>(8).unwrap(),
                            row.get::<String>(9).unwrap(),
                        )
                    })
                    .collect()
            });
            for missing in want_edges.difference(&have_edges) {
                v.push(format!("edge missing or wrong: {missing}"));
            }
            for extra in have_edges.difference(&want_edges) {
                v.push(format!("edge unexpected: {extra}"));
            }

            let mut want_tags: BTreeSet<String> = BTreeSet::new();
            for t in &parsed.doc.tags {
                let block = t
                    .block
                    .map(|b| rows[b as usize].id.to_string())
                    .unwrap_or_default();
                want_tags.insert(format!("{block}|{}", t.tag));
            }
            let have_tags: BTreeSet<String> = Spi::connect(|client| {
                client
                    .select(
                        "SELECT coalesce(block_id::text,''), tag FROM pgmind.tag
                         WHERE note_id = $1",
                        None,
                        &[arg(note_id)],
                    )
                    .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in verify_note: {e}"))
                    .map(|row| {
                        format!(
                            "{}|{}",
                            row.get::<String>(1).unwrap().unwrap(),
                            row.get::<String>(2).unwrap().unwrap(),
                        )
                    })
                    .collect()
            });
            for missing in want_tags.difference(&have_tags) {
                v.push(format!("tag missing: {missing}"));
            }
            for extra in have_tags.difference(&want_tags) {
                v.push(format!("tag unexpected: {extra}"));
            }
        }
    }

    SetOfIterator::new(v)
}
