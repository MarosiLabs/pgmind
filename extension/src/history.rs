//! RFC-005 D1/D2/D4: the history lanes.
//!
//! History stores **pre-images**: what the two storage lanes held *before* a
//! revision. Current state stays current state, so `knowledge.read()` at head
//! pays nothing for the past; reconstruction (slice 3) starts at an anchor at
//! or above the target and applies pre-images backwards.
//!
//! Two invariants govern everything here, and both exist to protect erasure and
//! forward-compatibility rather than to save bytes:
//!
//! * **X1** — no history row defines its bytes by reference to another history
//!   row's bytes. Every payload is literal, which makes erasing one revision's
//!   content a bounded in-place rewrite instead of a re-base of every successor.
//! * **X2** — reconstruction never invokes the parser. Structure at a past
//!   revision comes from the vectors stored here, so an RFC-002 amendment
//!   cannot silently re-interpret history.

use pgrx::prelude::*;
use pgrx::{JsonB, Uuid};
use serde_json::json;

use crate::store::{arg, BlockRow};
use crate::write::{ParsedNote, LANE_CHUNK_ROWS};

/// KEEP: copy `len` elements of the *newer* vector from `start`.
const OP_KEEP: i32 = 0;
/// INS: take the next `count` elements from the payload literally (X1).
const OP_INS: i32 = 1;

/// A script that rebuilds the older vector from the newer one.
struct Script<T> {
    ops: Vec<i32>,
    payload: Vec<T>,
}

/// Diff by common prefix and suffix.
///
/// Deliberately not an LCS: a minimal edit script would cost O(n·m) on a lane
/// that a whole-document rewrite touches entirely, and the payoff is only in
/// the multi-site-edit case. Prefix/suffix is O(n), exact for the single-region
/// edits that dominate (one block changed, one inserted, one removed), and
/// degrades to "store the changed middle literally" otherwise — which is
/// correct, just larger. If the storage-growth gate shows the middle dominating
/// real traffic, this is the function to revisit; the format does not change.
fn diff<T: PartialEq + Clone>(new: &[T], old: &[T]) -> Script<T> {
    let max = new.len().min(old.len());
    let mut p = 0;
    while p < max && new[p] == old[p] {
        p += 1;
    }
    let mut s = 0;
    while s < max - p && new[new.len() - 1 - s] == old[old.len() - 1 - s] {
        s += 1;
    }
    let mut ops = Vec::new();
    if p > 0 {
        ops.extend_from_slice(&[OP_KEEP, 0, p as i32]);
    }
    let middle = &old[p..old.len() - s];
    if !middle.is_empty() {
        ops.extend_from_slice(&[OP_INS, middle.len() as i32]);
    }
    if s > 0 {
        ops.extend_from_slice(&[OP_KEEP, (new.len() - s) as i32, s as i32]);
    }
    Script {
        ops,
        payload: middle.to_vec(),
    }
}

/// One block's pre-image, for the columns RFC-005 D4 calls content-visible.
struct BlockPre {
    kind: String,
    content: String,
    content_hash: Vec<u8>,
    block_ref_id: Option<String>,
    attrs: serde_json::Value,
    parent_block: Option<Uuid>,
}

struct Effect {
    block_id: Uuid,
    existed: bool,
    prev: Option<BlockPre>,
    bind: &'static str,
}

/// Everything one revision must record, computed before the lanes are mutated.
pub struct PreImage {
    prev_path: Option<String>,
    prev_preamble: Option<String>,
    prev_props: Option<serde_json::Value>,
    tiles: Script<String>,
    ids: Script<Uuid>,
    place_idx: Vec<i32>,
    place_prev: Vec<i32>,
    head_idx: Vec<i32>,
    head_prev: Vec<Vec<String>>,
    effects: Vec<Effect>,
}

/// The state a frame snapshots, and the state `capture` diffs against.
pub struct NewState<'a> {
    pub parsed: &'a ParsedNote,
    pub ids: &'a [Uuid],
}

fn triple(b: &BlockRow) -> [i32; 3] {
    [b.tile_ord, b.start_in_tile, b.end_in_tile]
}

/// Compute the pre-image of a revision: `old` is the state on disk right now,
/// `new` is what is about to replace it. Call BEFORE reconcile mutates the
/// lanes — after that the pre-image is gone.
pub fn capture(
    old_blocks: &[BlockRow],
    old_tiles: &[String],
    old_preamble: &str,
    old_props: &serde_json::Value,
    old_path: Option<&str>,
    new_path: &str,
    new: &NewState,
) -> PreImage {
    let new_tiles = &new.parsed.tiles;
    let new_blocks = &new.parsed.doc.blocks;
    let new_preamble =
        &new.parsed.source[new.parsed.doc.preamble.start..new.parsed.doc.preamble.end];

    // Where each surviving block sits in the NEW state, so the pre-image only
    // records the slots that actually moved.
    let new_pos: std::collections::HashMap<[u8; 16], usize> = new
        .ids
        .iter()
        .enumerate()
        .map(|(i, id)| (*id.as_bytes(), i))
        .collect();

    let mut place_idx = Vec::new();
    let mut place_prev = Vec::new();
    let mut head_idx = Vec::new();
    let mut head_prev = Vec::new();
    for (i, ob) in old_blocks.iter().enumerate() {
        match new_pos.get(ob.id.as_bytes()) {
            Some(&n) => {
                let (t, s, e) = new.parsed.placement[n];
                if [t, s, e] != triple(ob) {
                    place_idx.push(i as i32);
                    place_prev.extend_from_slice(&triple(ob));
                }
                if new_blocks[n].heading_path != ob.heading_path {
                    head_idx.push(i as i32);
                    head_prev.push(ob.heading_path.clone());
                }
            }
            // A block the new state does not contain has no slot to inherit
            // from, so both facts are always recorded.
            None => {
                place_idx.push(i as i32);
                place_prev.extend_from_slice(&triple(ob));
                head_idx.push(i as i32);
                head_prev.push(ob.heading_path.clone());
            }
        }
    }

    let old_by_id: std::collections::HashMap<[u8; 16], &BlockRow> =
        old_blocks.iter().map(|b| (*b.id.as_bytes(), b)).collect();
    let mut effects = Vec::new();
    // Minted blocks: no pre-image, but the row is what makes blame's
    // first_revision knowable (D8).
    for (i, id) in new.ids.iter().enumerate() {
        if !old_by_id.contains_key(id.as_bytes()) {
            let _ = i;
            effects.push(Effect {
                block_id: *id,
                existed: false,
                prev: None,
                bind: "mint",
            });
        }
    }
    // Survivors whose content-visible columns changed, and removals. Position
    // is NOT content-visible: ord, tile_ord, spans and heading_path ride the
    // per-revision vectors above, so a structural insert costs one row here
    // rather than one per block after it.
    for ob in old_blocks {
        let pre = BlockPre {
            kind: ob.kind.clone(),
            content: ob.content.clone(),
            content_hash: ob.content_hash.clone(),
            block_ref_id: ob.block_ref_id.clone(),
            attrs: ob.attrs.clone(),
            parent_block: ob.parent_block,
        };
        match new_pos.get(ob.id.as_bytes()) {
            Some(&n) => {
                let nb = &new_blocks[n];
                let parent_new = nb.parent.map(|p| new.ids[p as usize]);
                let changed = ob.kind != nb.kind.tag()
                    || ob.content != nb.normalized_content
                    || ob.content_hash != nb.content_hash
                    || ob.block_ref_id != nb.block_ref_id
                    || ob.attrs != nb.attrs
                    || ob.parent_block.map(|u| *u.as_bytes()) != parent_new.map(|u| *u.as_bytes());
                if changed {
                    effects.push(Effect {
                        block_id: ob.id,
                        existed: true,
                        prev: Some(pre),
                        bind: "carry",
                    });
                }
            }
            None => effects.push(Effect {
                block_id: ob.id,
                existed: true,
                prev: Some(pre),
                bind: "remove",
            }),
        }
    }

    PreImage {
        prev_path: match old_path {
            Some(p) if p != new_path => Some(p.to_string()),
            _ => None,
        },
        prev_preamble: (old_preamble != new_preamble).then(|| old_preamble.to_string()),
        prev_props: (old_props != &new.parsed.doc.properties).then(|| old_props.clone()),
        tiles: diff(new_tiles, old_tiles),
        ids: diff(
            new.ids,
            &old_blocks.iter().map(|b| b.id).collect::<Vec<_>>(),
        ),
        place_idx,
        place_prev,
        head_idx,
        head_prev,
        effects,
    }
}

/// Write the pre-image. The revision row must already exist (H1 has an FK to
/// it), so this runs after `new_revision`.
pub fn record(vault: Uuid, note_id: Uuid, revision: Uuid, seq: i64, pre: &PreImage) {
    let opt_text = |v: &Option<String>| match v {
        Some(s) => s.as_str().into(),
        None => pgrx::datum::DatumWithOid::null::<String>(),
    };
    Spi::run_with_args(
        "INSERT INTO pgmind.note_revision
           (revision_id, note_id, vault_id, seq, prev_path, prev_preamble, prev_props,
            tile_ops, tile_payload, id_ops, id_payload,
            place_idx, place_prev, head_idx, head_prev)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        &[
            arg(revision),
            arg(note_id),
            arg(vault),
            seq.into(),
            opt_text(&pre.prev_path),
            opt_text(&pre.prev_preamble),
            match &pre.prev_props {
                Some(v) => JsonB(v.clone()).into(),
                None => pgrx::datum::DatumWithOid::null::<JsonB>(),
            },
            pre.tiles.ops.clone().into(),
            pre.tiles.payload.clone().into(),
            pre.ids.ops.clone().into(),
            pre.ids.payload.clone().into(),
            pre.place_idx.clone().into(),
            pre.place_prev.clone().into(),
            pre.head_idx.clone().into(),
            JsonB(json!(pre.head_prev)).into(),
        ],
    )
    .unwrap_or_else(|e| pgrx::error!("pgmind: SPI note_revision insert: {e}"));

    if pre.effects.is_empty() {
        return;
    }
    // Same shape as the write path's batched lanes: the one unbounded column
    // (prev_content) travels as text[] rather than inside the jsonb, and rows
    // flush in chunks, so a large note cannot hit jsonb's 256 MB ceiling.
    let rows: Vec<serde_json::Value> = pre
        .effects
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut hex = String::new();
            if let Some(p) = &e.prev {
                use std::fmt::Write;
                for b in &p.content_hash {
                    let _ = write!(hex, "{b:02x}");
                }
            }
            json!({
                "i": i as i64 + 1,
                "block_id": e.block_id.to_string(),
                "existed": e.existed,
                "bind": e.bind,
                "prev_kind": e.prev.as_ref().map(|p| p.kind.clone()),
                "prev_content_hash": e.prev.as_ref().map(|_| hex),
                "prev_block_ref_id": e.prev.as_ref().and_then(|p| p.block_ref_id.clone()),
                "prev_attrs": e.prev.as_ref().map(|p| p.attrs.clone()),
                "prev_parent_block": e.prev.as_ref().and_then(|p| p.parent_block).map(|u| u.to_string()),
            })
        })
        .collect();
    let contents: Vec<&str> = pre
        .effects
        .iter()
        .map(|e| e.prev.as_ref().map(|p| p.content.as_str()).unwrap_or(""))
        .collect();

    for (chunk, texts) in rows
        .chunks(LANE_CHUNK_ROWS)
        .zip(contents.chunks(LANE_CHUNK_ROWS))
    {
        // Re-index per chunk: unnest WITH ORDINALITY restarts at 1.
        let indexed: Vec<serde_json::Value> = chunk
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let mut v = v.clone();
                v["i"] = json!(i as i64 + 1);
                v
            })
            .collect();
        Spi::run_with_args(
            "INSERT INTO pgmind.block_revision
               (note_id, block_id, seq, vault_id, existed, bind, prev_kind, prev_content,
                prev_content_hash, prev_block_ref_id, prev_attrs, prev_parent_block)
             SELECT $1, r.block_id, $2, $3, r.existed, r.bind,
                    r.prev_kind::pgmind.block_kind,
                    CASE WHEN r.prev_kind IS NULL THEN NULL ELSE c.content END,
                    decode(r.prev_content_hash, 'hex'), r.prev_block_ref_id,
                    r.prev_attrs, r.prev_parent_block
               FROM jsonb_to_recordset($4::jsonb) AS r(
                      i int8, block_id uuid, existed boolean, bind text, prev_kind text,
                      prev_content_hash text, prev_block_ref_id text, prev_attrs jsonb,
                      prev_parent_block uuid)
               JOIN unnest($5::text[]) WITH ORDINALITY AS c(content, i) ON c.i = r.i",
            &[
                arg(note_id),
                seq.into(),
                arg(vault),
                JsonB(json!(indexed)).into(),
                texts.to_vec().into(),
            ],
        )
        .unwrap_or_else(|e| pgrx::error!("pgmind: SPI block_revision insert: {e}"));
    }
}

/// Write a `note_frame` when `seq` lands on the cadence (RFC-005 D3).
///
/// Without a cadence writer the only frame in the database would be the one
/// compaction leaves at the floor, and reconstruction consumes anchors at or
/// *above* its target — so deep `as_of` would walk to head every time and the
/// RFC's bounded-read claim would be unearned.
pub fn maybe_frame(vault: Uuid, note_id: Uuid, seq: i64, path: &str, new: &NewState) {
    let every = crate::FRAME_EVERY.get().max(1) as i64;
    if seq % every != 0 {
        return;
    }
    let p = new.parsed;
    let preamble = &p.source[p.doc.preamble.start..p.doc.preamble.end];
    let tiles: Vec<&str> = p.tiles.iter().map(|s| s.as_str()).collect();
    let ids: Vec<Uuid> = new.ids.to_vec();
    let kinds: Vec<&str> = p.doc.blocks.iter().map(|b| b.kind.tag()).collect();
    let contents: Vec<&str> = p
        .doc
        .blocks
        .iter()
        .map(|b| b.normalized_content.as_str())
        .collect();
    let refs: Vec<String> = p
        .doc
        .blocks
        .iter()
        .map(|b| b.block_ref_id.clone().unwrap_or_default())
        .collect();
    let parents: Vec<Option<Uuid>> = p
        .doc
        .blocks
        .iter()
        .map(|b| b.parent.map(|i| new.ids[i as usize]))
        .collect();
    let heads: Vec<&Vec<String>> = p.doc.blocks.iter().map(|b| &b.heading_path).collect();
    let attrs: Vec<&serde_json::Value> = p.doc.blocks.iter().map(|b| &b.attrs).collect();
    let placement: Vec<i32> = p
        .placement
        .iter()
        .flat_map(|&(t, s, e)| [t, s, e])
        .collect();

    Spi::run_with_args(
        "INSERT INTO pgmind.note_frame
           (note_id, seq, vault_id, path, preamble, props, tiles, block_ids, kinds,
            contents, head_paths, parents, block_refs, attrs, placement)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
         ON CONFLICT (note_id, seq) DO NOTHING",
        &[
            arg(note_id),
            seq.into(),
            arg(vault),
            path.into(),
            preamble.into(),
            JsonB(p.doc.properties.clone()).into(),
            tiles.into(),
            ids.into(),
            kinds.into(),
            contents.into(),
            JsonB(json!(heads)).into(),
            parents.into(),
            refs.into(),
            JsonB(json!(attrs)).into(),
            placement.into(),
        ],
    )
    .unwrap_or_else(|e| pgrx::error!("pgmind: SPI note_frame insert: {e}"));
}
