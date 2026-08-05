//! The write path (RFC-003 D6) and the deterministic identity carry
//! (RFC-004 A3). One parser (pgmind-core), one pipeline, no triggers.

use pgrx::datum::DatumWithOid;
use pgrx::prelude::*;
use pgrx::{JsonB, Uuid};
use serde_json::json;

use crate::errors::{pm_error, Pm};
use crate::ids;
use crate::store::{self, arg, BlockRow};

/// A parsed note mapped onto the storage model: tiles + per-block tile-relative
/// facts (RFC-003 D2). Pure computation over a core `Document`.
pub struct ParsedNote {
    pub source: String,
    pub doc: pgmind_core::Document,
    /// Tile raw bytes per top-level child, in order. The absolute start is
    /// `doc.top_level[i].start` and is not duplicated here.
    pub tiles: Vec<String>,
    /// per block ord: (tile_ord, start_in_tile, end_in_tile)
    pub placement: Vec<(i32, i32, i32)>,
}

pub fn parse_note(source: &str) -> ParsedNote {
    let doc = pgmind_core::parse(source);
    let tiles: Vec<String> = doc
        .top_level
        .iter()
        .map(|sp| source[sp.start..sp.end].to_string())
        .collect();
    // Blocks and tiles are both in document order, so placement is a single
    // merge walk with a moving tile cursor. The `position()` rescan this
    // replaces was O(blocks x tiles) on the hottest path in the extension.
    let mut placement: Vec<(i32, i32, i32)> = Vec::with_capacity(doc.blocks.len());
    let mut ti = 0usize;
    for b in &doc.blocks {
        while ti < doc.top_level.len() && doc.top_level[ti].end <= b.span.start {
            ti += 1;
        }
        let escaped = match doc.top_level.get(ti) {
            Some(t) => !(b.span.start >= t.start && b.span.end <= t.end),
            None => true,
        };
        if escaped {
            pgrx::error!(
                "pgmind: internal — block span [{}, {}) escapes all tiles",
                b.span.start,
                b.span.end
            );
        }
        let t = &doc.top_level[ti];
        placement.push((
            ti as i32,
            (b.span.start - t.start) as i32,
            (b.span.end - t.start) as i32,
        ));
    }
    ParsedNote {
        source: source.to_string(),
        doc,
        tiles,
        placement,
    }
}

/// RFC-004 A3: the deterministic three-pass carry. Returns, per new block ord,
/// the carried old ID or a freshly minted one, plus the bookkeeping A4 needs.
pub struct Carry {
    pub ids: Vec<Uuid>,
    pub minted: Vec<Uuid>,
    pub removed: Vec<Uuid>,
    pub carried_ref: usize,
    pub carried_hash: usize,
}

/// One carry candidate, as RFC-004 A3 sees it — the same view whether it came
/// from a stored row or a fresh parse, so the block ops and `write()` can share
/// one implementation of the rule instead of each carrying a copy.
pub struct CarrySrc<'a> {
    pub block_ref_id: Option<&'a str>,
    pub content_hash: &'a [u8],
    pub heading_path: &'a [String],
}

impl<'a> CarrySrc<'a> {
    pub fn from_row(b: &'a BlockRow) -> Self {
        CarrySrc {
            block_ref_id: b.block_ref_id.as_deref(),
            content_hash: b.content_hash.as_slice(),
            heading_path: &b.heading_path,
        }
    }
    pub fn from_block(b: &'a pgmind_core::Block) -> Self {
        CarrySrc {
            block_ref_id: b.block_ref_id.as_deref(),
            content_hash: &b.content_hash,
            heading_path: &b.heading_path,
        }
    }
}

#[derive(Default)]
pub struct CarryTally {
    pub carried_ref: usize,
    pub carried_hash: usize,
}

/// The immutable candidate view the A3 passes read.
pub struct CarryInput<'a> {
    pub old: &'a [CarrySrc<'a>],
    pub new: &'a [CarrySrc<'a>],
    pub old_ids: &'a [Uuid],
}

/// Assignments accumulated across scopes, before pass 3 runs. The block ops
/// seed it with their outside pairs and pins, then carry one subtree at a time;
/// `write()` runs a single whole-note scope.
pub struct CarryState {
    pub assign: Vec<Option<Uuid>>,
    pub old_used: Vec<bool>,
    pub tally: CarryTally,
}

impl CarryState {
    pub fn new(old_len: usize, new_len: usize) -> Self {
        CarryState {
            assign: vec![None; new_len],
            old_used: vec![false; old_len],
            tally: CarryTally::default(),
        }
    }
}

/// Pass-2 pairing key: the content hash, plus the section when the tier
/// requires it.
type HashKey<'a> = (&'a [u8], Option<&'a [String]>);

/// RFC-004 A3 passes 1-2 over one candidate scope. This is the single
/// normative implementation of A3 — it used to exist twice (here and inline in
/// `ops::commit_op`) and the copies had already drifted on whether an
/// already-used holder stays claimable.
pub fn carry_scope(
    input: &CarryInput,
    old_idxs: &[usize],
    new_idxs: &[usize],
    state: &mut CarryState,
) {
    use std::collections::{HashMap, VecDeque};
    let (old, new, old_ids) = (input.old, input.new, input.old_ids);

    // Pass 1 — ^id claims: per ref value, ONLY the lowest-ord unused old holder
    // is claimable (duplicate holders behave as unmarked), and only the
    // lowest-ord new claimant wins (later claimants fall through to pass 2).
    let mut claimable: HashMap<&str, usize> = HashMap::new();
    for &oi in old_idxs {
        if state.old_used[oi] {
            continue;
        }
        if let Some(r) = old[oi].block_ref_id {
            claimable.entry(r).or_insert(oi);
        }
    }
    for &ni in new_idxs {
        if state.assign[ni].is_some() {
            continue;
        }
        if let Some(rid) = new[ni].block_ref_id {
            if let Some(&oi) = claimable.get(rid) {
                if !state.old_used[oi] {
                    state.assign[ni] = Some(old_ids[oi]);
                    state.old_used[oi] = true;
                    state.tally.carried_ref += 1;
                }
            }
        }
    }

    // Pass 2 — exact content match, k-th ↔ k-th in document order, in two
    // tiers. Tier 0 requires the same heading_path; tier 1 drops it.
    //
    // The tiering matters because content_hash covers only (kind, normalized
    // content) — NOT heading_path. With a single untiered pass, deleting a
    // section made its paragraph's ID get handed to an identical paragraph in
    // a surviving section, while the surviving paragraph's own ID was deleted:
    // an ID silently changing what it points at, which A1 forbids. Tier 1
    // still lets a renamed heading carry its section's blocks.
    for tier in 0..2 {
        let mut queues: HashMap<HashKey, VecDeque<usize>> = HashMap::new();
        for &oi in old_idxs {
            if !state.old_used[oi] {
                let key = (
                    old[oi].content_hash,
                    (tier == 0).then_some(old[oi].heading_path),
                );
                queues.entry(key).or_default().push_back(oi);
            }
        }
        for &ni in new_idxs {
            if state.assign[ni].is_some() {
                continue;
            }
            let key = (
                new[ni].content_hash,
                (tier == 0).then_some(new[ni].heading_path),
            );
            if let Some(q) = queues.get_mut(&key) {
                if let Some(oi) = q.pop_front() {
                    state.assign[ni] = Some(old_ids[oi]);
                    state.old_used[oi] = true;
                    state.tally.carried_hash += 1;
                }
            }
        }
    }
}

/// RFC-004 A3 pass 3 — mint whatever no scope carried; unmatched old rows are
/// removed.
pub fn finish_carry(input: &CarryInput, state: CarryState) -> Carry {
    let CarryState {
        assign,
        old_used,
        tally,
    } = state;
    let mut minted = Vec::new();
    let ids: Vec<Uuid> = assign
        .into_iter()
        .map(|a| {
            a.unwrap_or_else(|| {
                let id = ids::mint();
                minted.push(id);
                id
            })
        })
        .collect();
    let removed: Vec<Uuid> = input
        .old_ids
        .iter()
        .enumerate()
        .filter(|(oi, _)| !old_used[*oi])
        .map(|(_, id)| *id)
        .collect();
    Carry {
        ids,
        minted,
        removed,
        carried_ref: tally.carried_ref,
        carried_hash: tally.carried_hash,
    }
}

pub fn carry(old: &[BlockRow], new: &[pgmind_core::Block]) -> Carry {
    let old_src: Vec<CarrySrc> = old.iter().map(CarrySrc::from_row).collect();
    let new_src: Vec<CarrySrc> = new.iter().map(CarrySrc::from_block).collect();
    let old_ids: Vec<Uuid> = old.iter().map(|b| b.id).collect();
    let input = CarryInput {
        old: &old_src,
        new: &new_src,
        old_ids: &old_ids,
    };
    let all_old: Vec<usize> = (0..old.len()).collect();
    let all_new: Vec<usize> = (0..new.len()).collect();

    let mut state = CarryState::new(old.len(), new.len());
    carry_scope(&input, &all_old, &all_new, &mut state);
    finish_carry(&input, state)
}

fn capped(ids: &[Uuid]) -> serde_json::Value {
    const CAP: usize = 200;
    let strs: Vec<String> = ids.iter().take(CAP).map(|u| u.to_string()).collect();
    if ids.len() > CAP {
        json!({ "list": strs, "truncated": true, "count": ids.len() })
    } else {
        json!(strs)
    }
}

pub fn write_meta(op: &str, c: &Carry) -> serde_json::Value {
    json!({
        "op": op,
        "minted": capped(&c.minted),
        "carried": { "ref": c.carried_ref, "hash": c.carried_hash },
        "removed": capped(&c.removed),
    })
}

/// Insert the revision row and swap the note head. Returns the revision id.
pub fn new_revision(
    vault: Uuid,
    note: Uuid,
    parent: Option<Uuid>,
    source: &str,
    meta: &serde_json::Value,
) -> Uuid {
    let rev = ids::mint();
    Spi::run_with_args(
        "INSERT INTO pgmind.revision (id, vault_id, note_id, parent, source, meta)
         VALUES ($1, $2, $3, $4, $5, $6)",
        &[
            arg(rev),
            arg(vault),
            arg(note),
            parent
                .map(DatumWithOid::from)
                .unwrap_or_else(DatumWithOid::null::<Uuid>),
            source.into(),
            JsonB(meta.clone()).into(),
        ],
    )
    .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure inserting revision: {e}"));
    Spi::run_with_args(
        "UPDATE pgmind.note SET head_revision = $2 WHERE id = $1",
        &[arg(note), arg(rev)],
    )
    .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure swapping head: {e}"));
    rev
}

/// Reconcile both lanes and extraction from a full parse (RFC-003 D6 steps
/// 6-8). `carry.ids` maps new block ords to identities. Ordering is normative:
/// minted-parent INSERTs and carried UPDATEs precede removal DELETEs.
pub fn reconcile(
    vault: Uuid,
    note_id: Uuid,
    parsed: &ParsedNote,
    old: &[BlockRow],
    old_tiles: &[String],
    c: &Carry,
) {
    // --- lane 0: preamble + properties, re-derived from this parse ---
    //
    // Every op that reconciles must maintain this, not just knowledge.write:
    // a splice can move the preamble/tile boundary, and leaving the note row
    // stale makes knowledge.read() return preamble ‖ tiles bytes that were
    // never written (RFC-003 D2). Guarded so an unchanged note writes nothing.
    let preamble = &parsed.source[parsed.doc.preamble.start..parsed.doc.preamble.end];
    Spi::run_with_args(
        "UPDATE pgmind.note SET properties = $2, preamble = $3
         WHERE id = $1 AND (properties IS DISTINCT FROM $2 OR preamble IS DISTINCT FROM $3)",
        &[
            arg(note_id),
            JsonB(parsed.doc.properties.clone()).into(),
            preamble.into(),
        ],
    )
    .unwrap_or_else(|e| pgrx::error!("pgmind: SPI note lane update: {e}"));

    // --- byte lane: tiles by set-diff on (ord, raw) ---
    let new_tiles = &parsed.tiles;
    for (ord, raw) in new_tiles.iter().enumerate() {
        if let Some(old_raw) = old_tiles.get(ord) {
            if old_raw != raw {
                Spi::run_with_args(
                    "UPDATE pgmind.tile SET raw = $3 WHERE note_id = $1 AND ord = $2",
                    &[arg(note_id), (ord as i32).into(), raw.as_str().into()],
                )
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI tile update: {e}"));
            }
        } else {
            Spi::run_with_args(
                "INSERT INTO pgmind.tile (note_id, vault_id, ord, raw) VALUES ($1, $2, $3, $4)",
                &[
                    arg(note_id),
                    arg(vault),
                    (ord as i32).into(),
                    raw.as_str().into(),
                ],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI tile insert: {e}"));
        }
    }
    if old_tiles.len() > new_tiles.len() {
        Spi::run_with_args(
            "DELETE FROM pgmind.tile WHERE note_id = $1 AND ord >= $2",
            &[arg(note_id), (new_tiles.len() as i32).into()],
        )
        .unwrap_or_else(|e| pgrx::error!("pgmind: SPI tile delete: {e}"));
    }

    // --- semantic lane: blocks ---
    let old_by_id: std::collections::HashMap<[u8; 16], &BlockRow> =
        old.iter().map(|b| (*b.id.as_bytes(), b)).collect();
    let removed_set: std::collections::HashSet<[u8; 16]> =
        c.removed.iter().map(|u| *u.as_bytes()).collect();

    // (a) INSERT minted blocks in ord order (pre-order ⇒ parents before children).
    // (b) UPDATE carried blocks whose stored columns changed.
    for (ord, nb) in parsed.doc.blocks.iter().enumerate() {
        let id = c.ids[ord];
        let parent_id = nb.parent.map(|p| c.ids[p as usize]);
        let (tile_ord, s, e) = parsed.placement[ord];
        let attrs = JsonB(nb.attrs.clone());
        let heading_path = nb.heading_path.clone();
        let hash = nb.content_hash.to_vec();
        if let Some(ob) = old_by_id.get(id.as_bytes()) {
            let unchanged = ob.ord == ord as i32
                && ob.parent_block.map(|u| *u.as_bytes()) == parent_id.map(|u| *u.as_bytes())
                && ob.kind == nb.kind.tag()
                && ob.heading_path == heading_path
                && ob.content == nb.normalized_content
                && ob.content_hash == hash
                && ob.block_ref_id == nb.block_ref_id
                && ob.tile_ord == tile_ord
                && ob.start_in_tile == s
                && ob.end_in_tile == e
                && ob.attrs == nb.attrs;
            if unchanged {
                continue;
            }
            Spi::run_with_args(
                "UPDATE pgmind.block SET ord = $2, parent_block = $3, kind = $4::pgmind.block_kind,
                        heading_path = $5, content = $6, content_hash = $7, block_ref_id = $8,
                        tile_ord = $9, start_in_tile = $10, end_in_tile = $11, attrs = $12
                 WHERE id = $1",
                &[
                    arg(id),
                    (ord as i32).into(),
                    parent_id
                        .map(DatumWithOid::from)
                        .unwrap_or_else(DatumWithOid::null::<Uuid>),
                    nb.kind.tag().into(),
                    heading_path.into(),
                    nb.normalized_content.as_str().into(),
                    hash.into(),
                    nb.block_ref_id
                        .as_deref()
                        .map(DatumWithOid::from)
                        .unwrap_or_else(DatumWithOid::null::<String>),
                    tile_ord.into(),
                    s.into(),
                    e.into(),
                    attrs.into(),
                ],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI block update: {e}"));
        } else {
            Spi::run_with_args(
                "INSERT INTO pgmind.block
                   (id, note_id, vault_id, ord, parent_block, kind, heading_path, content,
                    content_hash, block_ref_id, tile_ord, start_in_tile, end_in_tile, attrs)
                 VALUES ($1, $2, $3, $4, $5, $6::pgmind.block_kind, $7, $8, $9, $10, $11, $12, $13, $14)",
                &[
                    arg(id),
                    arg(note_id),
                    arg(vault),
                    (ord as i32).into(),
                    parent_id
                        .map(DatumWithOid::from)
                        .unwrap_or_else(DatumWithOid::null::<Uuid>),
                    nb.kind.tag().into(),
                    heading_path.into(),
                    nb.normalized_content.as_str().into(),
                    hash.into(),
                    nb.block_ref_id
                        .as_deref()
                        .map(DatumWithOid::from)
                        .unwrap_or_else(DatumWithOid::null::<String>),
                    tile_ord.into(),
                    s.into(),
                    e.into(),
                    attrs.into(),
                ],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI block insert: {e}"));
        }
    }
    // (c) DELETE removed rows last, in one subtree-safe statement.
    if !removed_set.is_empty() {
        let removed: Vec<Uuid> = c.removed.clone();
        Spi::run_with_args(
            "DELETE FROM pgmind.block WHERE id = ANY($1)",
            &[removed.into()],
        )
        .unwrap_or_else(|e| pgrx::error!("pgmind: SPI block delete: {e}"));
    }

    reconcile_extraction(vault, note_id, parsed, c);
}

/// RFC-003 D6 steps 7-8: extraction reconcile by set-diff on natural keys,
/// against the full parse (document-global), deduped per key.
fn reconcile_extraction(vault: Uuid, note_id: Uuid, parsed: &ParsedNote, c: &Carry) {
    use std::collections::BTreeSet;

    // Desired edges: (src_block, kind, dst_path, dst_heading, dst_block_ref, alias).
    // Keyed on the block's raw UUID bytes ([u8; 16] is Ord) rather than its
    // text form: the previous String key had to be converted back with a
    // `SELECT $1::uuid` round trip per inserted row.
    type EdgeKey = ([u8; 16], String, String, String, String, String);
    let mut desired_edges: BTreeSet<EdgeKey> = BTreeSet::new();
    for l in &parsed.doc.links {
        let (heading, block_ref) = store::split_anchor(&l.anchor);
        desired_edges.insert((
            *c.ids[l.block as usize].as_bytes(),
            l.kind.tag().to_string(),
            l.target.clone(),
            heading,
            block_ref,
            l.alias.clone().unwrap_or_default(),
        ));
    }
    // Existing edges keyed the same way ('' stands in for NULL).
    let existing: Vec<(i64, EdgeKey)> = Spi::connect(|client| {
        client
            .select(
                "SELECT id, src_block, kind::text, dst_path,
                            coalesce(dst_heading, ''), coalesce(dst_block_ref, ''),
                            coalesce(alias, '')
                     FROM pgmind.edge WHERE src_note = $1",
                None,
                &[arg(note_id)],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI edge fetch: {e}"))
            .map(|row| {
                (
                    row.get::<i64>(1).unwrap().unwrap(),
                    (
                        *row.get::<Uuid>(2).unwrap().unwrap().as_bytes(),
                        row.get::<String>(3).unwrap().unwrap(),
                        row.get::<String>(4).unwrap().unwrap(),
                        row.get::<String>(5).unwrap().unwrap(),
                        row.get::<String>(6).unwrap().unwrap(),
                        row.get::<String>(7).unwrap().unwrap(),
                    ),
                )
            })
            .collect()
    });
    let existing_keys: BTreeSet<_> = existing.iter().map(|(_, k)| k.clone()).collect();
    let stale: Vec<i64> = existing
        .iter()
        .filter(|(_, k)| !desired_edges.contains(k))
        .map(|(id, _)| *id)
        .collect();
    if !stale.is_empty() {
        Spi::run_with_args(
            "DELETE FROM pgmind.edge WHERE id = ANY($1)",
            &[stale.into()],
        )
        .unwrap_or_else(|e| pgrx::error!("pgmind: SPI edge delete: {e}"));
    }
    for key in &desired_edges {
        if existing_keys.contains(key) {
            continue;
        }
        let (src_block, kind, dst_path, heading, block_ref, alias) = key;
        let r = store::resolve_target(vault, dst_path);
        let src = Uuid::from_bytes(*src_block);
        let opt = |s: &str| -> DatumWithOid<'static> {
            if s.is_empty() {
                DatumWithOid::null::<String>()
            } else {
                s.to_string().into()
            }
        };
        Spi::run_with_args(
            "INSERT INTO pgmind.edge
               (vault_id, src_note, src_block, kind, dst_path, dst_heading, dst_block_ref,
                alias, dst_note, resolved_via, dangling_reason)
             VALUES ($1, $2, $3, $4::pgmind.edge_kind, $5, $6, $7, $8, $9, $10, $11)",
            &[
                arg(vault),
                arg(note_id),
                arg(src),
                kind.as_str().into(),
                dst_path.as_str().into(),
                opt(heading),
                opt(block_ref),
                opt(alias),
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
        .unwrap_or_else(|e| pgrx::error!("pgmind: SPI edge insert: {e}"));
    }

    // Tags: (block_id, tag) — note-level rows have block_id NULL.
    type TagKey = (Option<[u8; 16]>, String);
    let mut desired_tags: BTreeSet<TagKey> = BTreeSet::new();
    for t in &parsed.doc.tags {
        let block = t.block.map(|b| *c.ids[b as usize].as_bytes());
        desired_tags.insert((block, t.tag.clone()));
    }
    let existing_tags: Vec<(i64, TagKey)> = Spi::connect(|client| {
        client
            .select(
                "SELECT id, block_id, tag FROM pgmind.tag WHERE note_id = $1",
                None,
                &[arg(note_id)],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI tag fetch: {e}"))
            .map(|row| {
                (
                    row.get::<i64>(1).unwrap().unwrap(),
                    (
                        row.get::<Uuid>(2).unwrap().map(|u| *u.as_bytes()),
                        row.get::<String>(3).unwrap().unwrap(),
                    ),
                )
            })
            .collect()
    });
    let existing_tag_keys: BTreeSet<_> = existing_tags.iter().map(|(_, k)| k.clone()).collect();
    let stale_tags: Vec<i64> = existing_tags
        .iter()
        .filter(|(_, k)| !desired_tags.contains(k))
        .map(|(id, _)| *id)
        .collect();
    if !stale_tags.is_empty() {
        Spi::run_with_args(
            "DELETE FROM pgmind.tag WHERE id = ANY($1)",
            &[stale_tags.into()],
        )
        .unwrap_or_else(|e| pgrx::error!("pgmind: SPI tag delete: {e}"));
    }
    for key in &desired_tags {
        if existing_tag_keys.contains(key) {
            continue;
        }
        let (block, tag) = key;
        let block_arg = match block {
            Some(b) => Uuid::from_bytes(*b).into(),
            None => DatumWithOid::null::<Uuid>(),
        };
        Spi::run_with_args(
            "INSERT INTO pgmind.tag (vault_id, note_id, block_id, tag) VALUES ($1, $2, $3, $4)",
            &[arg(vault), arg(note_id), block_arg, tag.as_str().into()],
        )
        .unwrap_or_else(|e| pgrx::error!("pgmind: SPI tag insert: {e}"));
    }
}

/// Serialize writers to one `(vault, path)` for the rest of the transaction.
///
/// Without it, two sessions both read the pre-image and the second reconciles
/// against a stale `old`: block rows the first session already deleted compare
/// equal on every stored column, so `reconcile`'s `if unchanged { continue }`
/// emits no SQL for them and they stay deleted — the byte lane advances while
/// the semantic lane silently loses rows, breaking RFC-003 D2's mirror
/// invariant with no error raised. For a path that does not exist yet it also
/// turns a racing pair of INSERTs into a wait instead of a bare 23505 out of
/// the `note_live_path` partial unique index.
///
/// Transaction-scoped, so it is released at commit or abort with no unlock
/// path to get wrong. Distinct paths hash to distinct keys and do not contend.
fn lock_path(vault: Uuid, path: &str) {
    Spi::run_with_args(
        "SELECT pg_advisory_xact_lock(hashtextextended($1::text || '/' || $2, 0))",
        &[arg(vault), path.into()],
    )
    .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure taking the write lock: {e}"));
}

/// `knowledge.write` (RFC-003 D6): upsert a whole note deterministically.
pub fn write_note(path_raw: &str, source: &str) -> Uuid {
    let vault = store::current_vault();
    let path = pgmind_core::path::path_normalize(path_raw);
    if !pgmind_core::path::path_is_valid(&path) {
        pm_error(
            Pm::InvalidPath,
            "invalid note path",
            &format!("path {path_raw:?}"),
        );
    }

    lock_path(vault, &path);

    let existing = store::note_by_path(vault, &path);
    let old_tiles: Vec<String> = match existing {
        Some(ref note) => {
            let tiles = store::tiles_of(note.id);
            // Idempotence short-circuit: byte-identical ⇒ current head, no revision.
            let mut current = note.preamble.clone();
            for t in &tiles {
                current.push_str(t);
            }
            if current == source {
                return note.head_revision;
            }
            tiles
        }
        None => Vec::new(),
    };

    let parsed = parse_note(source);
    let properties = JsonB(parsed.doc.properties.clone());
    let preamble = &parsed.source[parsed.doc.preamble.start..parsed.doc.preamble.end];

    match existing {
        Some(note) => {
            let old = store::blocks_of(note.id);
            let c = carry(&old, &parsed.doc.blocks);
            // reconcile() owns the note row's preamble/properties now.
            reconcile(vault, note.id, &parsed, &old, &old_tiles, &c);
            new_revision(
                vault,
                note.id,
                Some(note.head_revision),
                "api",
                &write_meta("write", &c),
            )
        }
        None => {
            let note_id = ids::mint();
            let rev_id = ids::mint();
            Spi::run_with_args(
                "INSERT INTO pgmind.note (id, vault_id, path, properties, preamble, head_revision)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    arg(note_id),
                    arg(vault),
                    path.as_str().into(),
                    properties.into(),
                    preamble.into(),
                    arg(rev_id),
                ],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI note insert: {e}"));
            let c = carry(&[], &parsed.doc.blocks);
            reconcile(vault, note_id, &parsed, &[], &[], &c);
            Spi::run_with_args(
                "INSERT INTO pgmind.revision (id, vault_id, note_id, parent, source, meta)
                 VALUES ($1, $2, $3, NULL, 'api', $4)",
                &[
                    arg(rev_id),
                    arg(vault),
                    arg(note_id),
                    JsonB(write_meta("write", &c)).into(),
                ],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI revision insert: {e}"));
            // Repair other notes' edges now that this path exists (RFC-003 D5).
            store::repair_edges_on_creation(vault, &path);
            rev_id
        }
    }
}
