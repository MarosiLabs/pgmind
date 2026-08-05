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
    /// (tile raw, absolute start) per top-level child, in order.
    pub tiles: Vec<(String, usize)>,
    /// per block ord: (tile_ord, start_in_tile, end_in_tile)
    pub placement: Vec<(i32, i32, i32)>,
}

pub fn parse_note(source: &str) -> ParsedNote {
    let doc = pgmind_core::parse(source);
    let tiles: Vec<(String, usize)> = doc
        .top_level
        .iter()
        .map(|sp| (source[sp.start..sp.end].to_string(), sp.start))
        .collect();
    let placement = doc
        .blocks
        .iter()
        .map(|b| {
            let ti = doc
                .top_level
                .iter()
                .position(|t| b.span.start >= t.start && b.span.end <= t.end)
                .unwrap_or_else(|| {
                    pgrx::error!(
                        "pgmind: internal — block span [{}, {}) escapes all tiles",
                        b.span.start,
                        b.span.end
                    )
                });
            let t = &doc.top_level[ti];
            (
                ti as i32,
                (b.span.start - t.start) as i32,
                (b.span.end - t.start) as i32,
            )
        })
        .collect();
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

pub fn carry(old: &[BlockRow], new: &[pgmind_core::Block]) -> Carry {
    let mut old_matched = vec![false; old.len()];
    let mut assign: Vec<Option<Uuid>> = vec![None; new.len()];
    let mut carried_ref = 0usize;
    let mut carried_hash = 0usize;

    // Pass 1 — ^id claims (RFC-004 A3): per ref value, ONLY the lowest-ord old
    // holder is claimable (duplicate holders behave as unmarked), and only the
    // lowest-ord new claimant wins (later claimants fall through to pass 2).
    use std::collections::HashMap;
    let mut claimable: HashMap<&str, usize> = HashMap::new();
    for (oi, ob) in old.iter().enumerate() {
        if let Some(r) = ob.block_ref_id.as_deref() {
            claimable.entry(r).or_insert(oi);
        }
    }
    for (ni, nb) in new.iter().enumerate() {
        if let Some(rid) = nb.block_ref_id.as_deref() {
            if let Some(&oi) = claimable.get(rid) {
                if !old_matched[oi] {
                    assign[ni] = Some(old[oi].id);
                    old_matched[oi] = true;
                    carried_ref += 1;
                }
            }
        }
    }

    // Pass 2 — exact content match, k-th ↔ k-th in document order.
    let mut queues: HashMap<&[u8], std::collections::VecDeque<usize>> = HashMap::new();
    for (oi, ob) in old.iter().enumerate() {
        if !old_matched[oi] {
            queues
                .entry(ob.content_hash.as_slice())
                .or_default()
                .push_back(oi);
        }
    }
    for (ni, nb) in new.iter().enumerate() {
        if assign[ni].is_some() {
            continue;
        }
        if let Some(q) = queues.get_mut(nb.content_hash.as_slice()) {
            if let Some(oi) = q.pop_front() {
                assign[ni] = Some(old[oi].id);
                old_matched[oi] = true;
                carried_hash += 1;
            }
        }
    }

    // Pass 3 — mint the rest; unmatched old rows are removed.
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
    let removed: Vec<Uuid> = old
        .iter()
        .enumerate()
        .filter(|(oi, _)| !old_matched[*oi])
        .map(|(_, ob)| ob.id)
        .collect();

    Carry {
        ids,
        minted,
        removed,
        carried_ref,
        carried_hash,
    }
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
pub fn reconcile(vault: Uuid, note_id: Uuid, parsed: &ParsedNote, old: &[BlockRow], c: &Carry) {
    // --- byte lane: tiles by set-diff on (ord, raw) ---
    let old_tiles: Vec<String> = store::tiles_of(note_id);
    let new_tiles = &parsed.tiles;
    for (ord, (raw, _)) in new_tiles.iter().enumerate() {
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

    // Desired edges: (src_block, kind, dst_path, dst_heading, dst_block_ref, alias)
    let mut desired_edges: BTreeSet<(String, String, String, String, String, String)> =
        BTreeSet::new();
    for l in &parsed.doc.links {
        let kind = match l.kind {
            pgmind_core::LinkKind::Wikilink => "wikilink",
            pgmind_core::LinkKind::Transclusion => "transclusion",
            pgmind_core::LinkKind::Blockref => "blockref",
            pgmind_core::LinkKind::Mdlink => "mdlink",
        };
        let (heading, block_ref) = match &l.anchor {
            Some(a) if a.starts_with('^') => (String::new(), a[1..].to_string()),
            Some(a) => (a.clone(), String::new()),
            None => (String::new(), String::new()),
        };
        desired_edges.insert((
            c.ids[l.block as usize].to_string(),
            kind.to_string(),
            l.target.clone(),
            heading,
            block_ref,
            l.alias.clone().unwrap_or_default(),
        ));
    }
    // Existing edges keyed the same way ('' stands in for NULL).
    type EdgeKey = (String, String, String, String, String, String);
    let existing: Vec<(i64, EdgeKey)> = Spi::connect(|client| {
        client
            .select(
                "SELECT id, src_block::text, kind::text, dst_path,
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
                        row.get::<String>(2).unwrap().unwrap(),
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
    for (id, key) in &existing {
        if !desired_edges.contains(key) {
            Spi::run_with_args("DELETE FROM pgmind.edge WHERE id = $1", &[(*id).into()])
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI edge delete: {e}"));
        }
    }
    for key in &desired_edges {
        if existing_keys.contains(key) {
            continue;
        }
        let (src_block, kind, dst_path, heading, block_ref, alias) = key;
        let r = store::resolve_target(vault, dst_path);
        let src: Uuid = Spi::get_one_with_args("SELECT $1::uuid", &[src_block.as_str().into()])
            .unwrap()
            .unwrap();
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

    // Tags: (block_id or '', tag) — note-level rows have block_id NULL.
    let mut desired_tags: BTreeSet<(String, String)> = BTreeSet::new();
    for t in &parsed.doc.tags {
        let block = t
            .block
            .map(|b| c.ids[b as usize].to_string())
            .unwrap_or_default();
        desired_tags.insert((block, t.tag.clone()));
    }
    let existing_tags: Vec<(i64, (String, String))> = Spi::connect(|client| {
        client
            .select(
                "SELECT id, coalesce(block_id::text, ''), tag FROM pgmind.tag WHERE note_id = $1",
                None,
                &[arg(note_id)],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI tag fetch: {e}"))
            .map(|row| {
                (
                    row.get::<i64>(1).unwrap().unwrap(),
                    (
                        row.get::<String>(2).unwrap().unwrap(),
                        row.get::<String>(3).unwrap().unwrap(),
                    ),
                )
            })
            .collect()
    });
    let existing_tag_keys: BTreeSet<_> = existing_tags.iter().map(|(_, k)| k.clone()).collect();
    for (id, key) in &existing_tags {
        if !desired_tags.contains(key) {
            Spi::run_with_args("DELETE FROM pgmind.tag WHERE id = $1", &[(*id).into()])
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI tag delete: {e}"));
        }
    }
    for (block, tag) in &desired_tags {
        if existing_tag_keys.contains(&(block.clone(), tag.clone())) {
            continue;
        }
        let block_arg = if block.is_empty() {
            DatumWithOid::null::<Uuid>()
        } else {
            let u: Uuid = Spi::get_one_with_args("SELECT $1::uuid", &[block.as_str().into()])
                .unwrap()
                .unwrap();
            u.into()
        };
        Spi::run_with_args(
            "INSERT INTO pgmind.tag (vault_id, note_id, block_id, tag) VALUES ($1, $2, $3, $4)",
            &[arg(vault), arg(note_id), block_arg, tag.as_str().into()],
        )
        .unwrap_or_else(|e| pgrx::error!("pgmind: SPI tag insert: {e}"));
    }
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

    let existing = store::note_by_path(vault, &path);
    if let Some(ref note) = existing {
        // Idempotence short-circuit: byte-identical ⇒ current head, no revision.
        if store::source_of(note) == source {
            return note.head_revision;
        }
    }

    let parsed = parse_note(source);
    let properties = JsonB(parsed.doc.properties.clone());
    let preamble = &parsed.source[parsed.doc.preamble.start..parsed.doc.preamble.end];

    match existing {
        Some(note) => {
            let old = store::blocks_of(note.id);
            let c = carry(&old, &parsed.doc.blocks);
            Spi::run_with_args(
                "UPDATE pgmind.note SET properties = $2, preamble = $3 WHERE id = $1",
                &[arg(note.id), properties.into(), preamble.into()],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI note update: {e}"));
            reconcile(vault, note.id, &parsed, &old, &c);
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
            reconcile(vault, note_id, &parsed, &[], &c);
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
