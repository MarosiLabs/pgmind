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

    /// RFC-005 D8: the history lane's invariant checker. Empty = healthy.
    #[pg_extern]
    fn verify_history(note_id: Uuid) -> SetOfIterator<'static, String> {
        super::verify_history_impl(note_id)
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

/// RFC-005 D8's post-conditions, which the review rewrote after finding all
/// three of the accepted ones unsatisfiable, vacuous, or both.
///
/// The vacuous one is worth naming: `read_as_of(head) = read()` proves nothing,
/// because reconstruction at head applies no scripts and reads current state by
/// definition. Reconstruction is therefore checked where it can actually fail —
/// at the floor, at the newest frame, and at sampled revisions in between.
fn verify_history_impl(note_id: Uuid) -> SetOfIterator<'static, String> {
    let mut v: Vec<String> = Vec::new();
    let Some(note) = store::note_by_id(note_id) else {
        return SetOfIterator::new(vec![format!("note {note_id} does not exist")]);
    };

    let (head_seq, floor): (i64, i64) = Spi::connect(|client| {
        client
            .select(
                "SELECT (SELECT seq FROM pgmind.revision WHERE id = n.head_revision),
                        n.history_floor FROM pgmind.note n WHERE n.id = $1",
                None,
                &[arg(note_id)],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in verify_history: {e}"))
            .map(|r| {
                (
                    r.get::<i64>(1).unwrap().unwrap_or(-1),
                    r.get::<i64>(2).unwrap().unwrap_or(0),
                )
            })
            .next()
            .unwrap_or((-1, 0))
    });
    if head_seq < 0 {
        v.push("head revision has no seq".into());
        return SetOfIterator::new(v);
    }

    // (2) seq is dense and gapless above the floor, and every revision above it
    // has exactly one note_revision row. The second half is what catches a
    // history lane that silently stopped being written -- including a dump that
    // restored the revisions and lost their pre-images.
    let (revs, hist): (i64, i64) = Spi::connect(|client| {
        client
            .select(
                "SELECT (SELECT count(*) FROM pgmind.revision
                          WHERE note_id = $1 AND seq >= $2),
                        (SELECT count(*) FROM pgmind.note_revision
                          WHERE note_id = $1 AND seq >= $2)",
                None,
                &[arg(note_id), floor.into()],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in verify_history: {e}"))
            .map(|r| {
                (
                    r.get::<i64>(1).unwrap().unwrap_or(0),
                    r.get::<i64>(2).unwrap().unwrap_or(0),
                )
            })
            .next()
            .unwrap_or((0, 0))
    });
    if revs != head_seq - floor + 1 {
        v.push(format!(
            "seq is not dense above the floor: {revs} revisions for seqs {floor}..{head_seq}"
        ));
    }
    if hist != revs {
        v.push(format!(
            "{revs} revisions above the floor but {hist} note_revision rows — history is missing"
        ));
    }

    // (1) The floor frame is the anchor retention leaves behind. Absent, every
    // read at the floor walks to head and compaction has silently orphaned the
    // oldest reconstructable point.
    if floor > 0 {
        let framed: Option<i64> = Spi::get_one_with_args(
            "SELECT count(*) FROM pgmind.note_frame WHERE note_id = $1 AND seq = $2",
            &[arg(note_id), floor.into()],
        )
        .unwrap_or(Some(0));
        if framed != Some(1) {
            v.push(format!("no note_frame at the history floor (seq {floor})"));
        }
    }

    // (3) Reconstruction, checked where it can fail. Deterministic sampling so
    // a failure is reproducible rather than flaky.
    let mut probes: Vec<i64> = vec![floor, head_seq];
    if let Some(f) = newest_frame_below(note_id, head_seq) {
        probes.push(f);
    }
    let span = head_seq - floor;
    if span > 0 {
        let step = (span / 20).max(1);
        let mut s = floor;
        while s < head_seq {
            probes.push(s);
            s += step;
        }
    }
    probes.sort_unstable();
    probes.dedup();
    for seq in probes {
        let st = crate::timetravel::state_at(&note, seq);
        let src = st.source();
        if st.ids.len() != st.place.len() || st.ids.len() != st.heads.len() {
            v.push(format!(
                "seq {seq}: vectors disagree ({} ids, {} placements, {} heading paths)",
                st.ids.len(),
                st.place.len(),
                st.heads.len()
            ));
            continue;
        }
        // X2 forbids parsing to RECONSTRUCT; parsing to CHECK is exactly what an
        // invariant checker is for.
        let parsed = pgmind_core::parse(&src);
        if parsed.blocks.len() != st.ids.len() {
            v.push(format!(
                "seq {seq}: reconstructed bytes parse to {} blocks but the id vector holds {}",
                parsed.blocks.len(),
                st.ids.len()
            ));
        }
        for (i, (t, s0, e)) in st.place.iter().enumerate() {
            let ti = *t as usize;
            if ti >= st.tiles.len() || *s0 < 0 || *e < *s0 || (*e as usize) > st.tiles[ti].len() {
                v.push(format!(
                    "seq {seq}: block {i} span ({t},{s0},{e}) is outside its tile"
                ));
                break;
            }
        }
    }
    SetOfIterator::new(v)
}

fn newest_frame_below(note_id: Uuid, head_seq: i64) -> Option<i64> {
    Spi::get_one_with_args(
        "SELECT max(seq) FROM pgmind.note_frame WHERE note_id = $1 AND seq <= $2",
        &[arg(note_id), head_seq.into()],
    )
    .unwrap_or(None)
}
