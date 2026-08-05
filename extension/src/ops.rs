//! The five block operations (RFC-004 A2) and their splice mechanics
//! (RFC-003 D6): byte-surgical edits, full-note re-parse, outside-region
//! invariance (PM008), pins + scoped subtree carry, one revision per op.

use pgrx::prelude::*;
use pgrx::{heap_tuple::PgHeapTuple, Uuid};
use serde_json::json;

use crate::errors::{pm_error, Pm};
use crate::ids;
use crate::store::{self, arg, BlockRow};
use crate::write::{self, parse_note, Carry, ParsedNote};
use crate::Markdown;

/// Everything an op needs about its target's note, loaded once.
struct NoteCtx {
    vault: Uuid,
    note_id: Uuid,
    head: Uuid,
    source: String,
    rows: Vec<BlockRow>,
    parsed: ParsedNote,
}

fn load_ctx_by_block(block_id: Uuid) -> (NoteCtx, usize) {
    let vault = store::current_vault();
    let found: Option<(Uuid, Uuid)> = Spi::connect(|client| {
        let rows = client
            .select(
                "SELECT b.note_id, b.vault_id FROM pgmind.block b
                 JOIN pgmind.note n ON n.id = b.note_id
                 WHERE b.id = $1 AND n.tombstoned_at IS NULL",
                Some(1),
                &[arg(block_id)],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure loading block: {e}"));
        if rows.is_empty() {
            return None;
        }
        let row = rows.first();
        Some((row.get(1).unwrap().unwrap(), row.get(2).unwrap().unwrap()))
    });
    let Some((note_id, block_vault)) = found else {
        pm_error(
            Pm::BlockNotFound,
            "block not found",
            &format!("id {block_id}"),
        );
    };
    if block_vault != vault {
        pm_error(
            Pm::BlockNotFound,
            "block not in the current vault",
            &format!("id {block_id}"),
        );
    }
    let (head, preamble): (Uuid, String) = Spi::connect(|client| {
        let rows = client
            .select(
                "SELECT head_revision, preamble FROM pgmind.note WHERE id = $1",
                Some(1),
                &[arg(note_id)],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure loading note: {e}"));
        let row = rows.first();
        (row.get(1).unwrap().unwrap(), row.get(2).unwrap().unwrap())
    });
    let mut source = preamble;
    for t in store::tiles_of(note_id) {
        source.push_str(&t);
    }
    let rows = store::blocks_of(note_id);
    let parsed = parse_note(&source);
    if rows.len() != parsed.doc.blocks.len() {
        pgrx::error!(
            "pgmind: stored blocks disagree with parse for note {note_id} — run pgmind.verify_note"
        );
    }
    let idx = rows
        .iter()
        .position(|r| r.id == block_id)
        .unwrap_or_else(|| pgrx::error!("pgmind: block row vanished mid-operation"));
    let ctx = NoteCtx {
        vault,
        note_id,
        head,
        source,
        rows,
        parsed,
    };
    (ctx, idx)
}

/// Subtree of `idx` as a contiguous ord range [idx, end).
fn subtree_end(parsed: &ParsedNote, idx: usize) -> usize {
    let blocks = &parsed.doc.blocks;
    let mut end = idx + 1;
    while end < blocks.len() {
        let mut p = blocks[end].parent;
        let mut inside = false;
        while let Some(pi) = p {
            if pi as usize == idx {
                inside = true;
                break;
            }
            p = blocks[pi as usize].parent;
        }
        if !inside {
            break;
        }
        end += 1;
    }
    end
}

/// Container children of block `idx`: direct children that are items, or
/// content nested deeper than the block's own continuation prefix (i.e.
/// reached through a nested list/quote).
fn container_children(parsed: &ParsedNote, idx: usize) -> Vec<usize> {
    let blocks = &parsed.doc.blocks;
    blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.parent == Some(idx as u32))
        .filter(|(_, b)| {
            b.kind == pgmind_core::BlockKind::ListItem || b.cont_prefix != blocks[idx].cont_prefix
        })
        .map(|(i, _)| i)
        .collect()
}

/// The byte range an op may rewrite for target `idx`: the block's span, cut
/// short at its first container child (whose bytes are preserved). PM006 when
/// own content is non-contiguous (content after a container child — v1).
fn own_range(parsed: &ParsedNote, idx: usize) -> (usize, usize) {
    let b = &parsed.doc.blocks[idx];
    let cc = container_children(parsed, idx);
    let cut = cc
        .iter()
        .map(|&i| parsed.doc.blocks[i].span.start)
        .min()
        .unwrap_or(b.span.end);
    // v1: any direct content line after a container child is out of scope.
    for (i, other) in parsed.doc.blocks.iter().enumerate() {
        if other.parent == Some(idx as u32) && !cc.contains(&i) && other.span.start >= cut {
            pm_error(
                Pm::ContainerConstraint,
                "target's own content is non-contiguous",
                &format!(
                    "block {} has direct content after a nested container (v1 limit)",
                    parsed.doc.blocks[idx].ord
                ),
            );
        }
    }
    (b.span.start, cut.min(b.span.end))
}

/// End of the block's CONTENT within [start, end): top-level spans tile the
/// document, so they include trailing blank-line trivia which a splice must
/// preserve (replacing it merges neighbors — verified the hard way).
fn content_end(source: &str, start: usize, end: usize) -> usize {
    let seg = &source[start..end];
    let trimmed = seg.trim_end_matches('\n');
    if trimmed.len() == seg.len() {
        end
    } else {
        start + trimmed.len() + 1 // keep exactly one newline after content
    }
}

/// Parse a fragment and return (parse, parentless ords) — fragment arity is
/// the count of parentless addressable blocks (RFC-004 A2).
fn parse_fragment(fragment: &str) -> (ParsedNote, Vec<usize>) {
    let parsed = parse_note(fragment);
    if parsed.doc.preamble.end != 0 {
        pm_error(
            Pm::FragmentArity,
            "fragment must not carry frontmatter",
            "frontmatter belongs to write(), not block operations",
        );
    }
    let roots: Vec<usize> = parsed
        .doc
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.parent.is_none())
        .map(|(i, _)| i)
        .collect();
    (parsed, roots)
}

fn fragment_has_container_children(frag: &ParsedNote, roots: &[usize]) -> bool {
    roots
        .iter()
        .any(|&r| !container_children(frag, r).is_empty())
}

/// Re-prefix a fragment's lines for its destination context: line 1 is spliced
/// verbatim (the preserved old decoration precedes it); lines 2+ get `prefix`.
fn decorate(fragment: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        return fragment.to_string();
    }
    let mut out = String::with_capacity(fragment.len());
    for (i, line) in fragment.split_inclusive('\n').enumerate() {
        if i > 0 && !line.trim_end_matches(['\n', '\r']).is_empty() {
            out.push_str(prefix);
        } else if i > 0 {
            // blank continuation line: quote prefixes only, no trailing spaces
            out.push_str(prefix.trim_end());
        }
        out.push_str(line);
    }
    out
}

/// Ensure a segment ends with a blank line (separator synthesis, RFC-003 D6).
fn blank_terminated(mut s: String) -> String {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    if !s.ends_with("\n\n") {
        s.push('\n');
    }
    s
}

fn trailing_newlines(s: &str) -> String {
    s[s.trim_end_matches('\n').len()..].to_string()
}

/// The op result composite (RFC-003 D6).
fn op_result(revision: Uuid, block_ids: Vec<Uuid>) -> PgHeapTuple<'static, pgrx::AllocatedByRust> {
    let mut tuple = PgHeapTuple::new_composite_type("pgmind.op_result")
        .unwrap_or_else(|e| pgrx::error!("pgmind: op_result composite missing: {e}"));
    tuple
        .set_by_name("revision", revision)
        .unwrap_or_else(|e| pgrx::error!("pgmind: op_result.revision: {e}"));
    tuple
        .set_by_name("block_ids", block_ids)
        .unwrap_or_else(|e| pgrx::error!("pgmind: op_result.block_ids: {e}"));
    tuple
}

/// Finish an op: outside-region invariance check (PM008), pins + scoped carry,
/// reconcile, revision. `edit_range` is the rewritten byte range in the NEW
/// source; `old_region` the ord range replaced in the OLD parse.
#[allow(clippy::too_many_arguments)]
fn commit_op(
    ctx: &NoteCtx,
    op: &str,
    new_source: String,
    old_region: (usize, usize),
    edit_range: (usize, usize),
    // (old idx → new idx) pins inside the region, in addition to outside pairs
    region_pins: Vec<(usize, usize)>,
    // pins must keep (kind, hash) — true for move, whose bytes are untouched
    pin_hash_strict: bool,
    // scoped subtree carry: (old idxs, new idxs)
    scoped: Vec<(Vec<usize>, Vec<usize>)>,
    meta_extra: serde_json::Value,
    result_new_idxs: Vec<usize>,
) -> PgHeapTuple<'static, pgrx::AllocatedByRust> {
    let new_parsed = parse_note(&new_source);
    let old_blocks = &ctx.parsed.doc.blocks;
    let new_blocks = &new_parsed.doc.blocks;

    for &(oi, ni) in &region_pins {
        if oi >= old_blocks.len() || ni >= new_blocks.len() {
            pm_error(
                Pm::SpliceRestructures,
                "splice restructures adjacent content",
                &format!("pin ({oi}, {ni}) out of range after re-parse"),
            );
        }
        if pin_hash_strict
            && (old_blocks[oi].kind != new_blocks[ni].kind
                || old_blocks[oi].content_hash != new_blocks[ni].content_hash)
        {
            pm_error(
                Pm::SpliceRestructures,
                "move changed block content",
                &format!("old ord {oi} vs new ord {ni}"),
            );
        }
    }

    let pinned_old: std::collections::HashSet<usize> =
        region_pins.iter().map(|(o, _)| *o).collect();
    let pinned_new: std::collections::HashSet<usize> =
        region_pins.iter().map(|(_, n)| *n).collect();

    // Outside sets, in document order — pinned blocks (targets, ancestors,
    // move permutations) are handled by their pins, never by outside pairing.
    let old_outside: Vec<usize> = (0..old_blocks.len())
        .filter(|i| (*i < old_region.0 || *i >= old_region.1) && !pinned_old.contains(i))
        .collect();
    let new_outside: Vec<usize> = (0..new_blocks.len())
        .filter(|i| {
            let sp = &new_blocks[*i].span;
            (sp.end <= edit_range.0 || sp.start >= edit_range.1) && !pinned_new.contains(i)
        })
        .collect();
    if old_outside.len() != new_outside.len() {
        pm_error(
            Pm::SpliceRestructures,
            "splice restructures adjacent content",
            &format!(
                "{} blocks outside the edit before, {} after",
                old_outside.len(),
                new_outside.len()
            ),
        );
    }
    for (&oi, &ni) in old_outside.iter().zip(new_outside.iter()) {
        let (ob, nb) = (&old_blocks[oi], &new_blocks[ni]);
        if ob.kind != nb.kind || ob.content_hash != nb.content_hash {
            pm_error(
                Pm::SpliceRestructures,
                "splice restructures adjacent content",
                &format!("block at old ord {oi} changed kind or content (new ord {ni})"),
            );
        }
    }

    // Assignments: outside pairs, then region pins, then scoped carry.
    let mut assign: Vec<Option<Uuid>> = vec![None; new_blocks.len()];
    let mut old_used = vec![false; old_blocks.len()];
    for (&oi, &ni) in old_outside.iter().zip(new_outside.iter()) {
        assign[ni] = Some(ctx.rows[oi].id);
        old_used[oi] = true;
    }
    for &(oi, ni) in &region_pins {
        assign[ni] = Some(ctx.rows[oi].id);
        old_used[oi] = true;
    }
    let mut carried_ref = 0usize;
    let mut carried_hash = 0usize;
    for (old_set, new_set) in &scoped {
        // A3 passes scoped to the subtree (RFC-004 A2 "subtree carry").
        use std::collections::HashMap;
        let mut claimable: HashMap<&str, usize> = HashMap::new();
        for &oi in old_set {
            if old_used[oi] {
                continue;
            }
            if let Some(r) = old_blocks[oi].block_ref_id.as_deref() {
                claimable.entry(r).or_insert(oi);
            }
        }
        for &ni in new_set {
            if assign[ni].is_some() {
                continue;
            }
            if let Some(rid) = new_blocks[ni].block_ref_id.as_deref() {
                if let Some(&oi) = claimable.get(rid) {
                    if !old_used[oi] {
                        assign[ni] = Some(ctx.rows[oi].id);
                        old_used[oi] = true;
                        carried_ref += 1;
                    }
                }
            }
        }
        let mut queues: HashMap<&[u8; 32], std::collections::VecDeque<usize>> = HashMap::new();
        for &oi in old_set {
            if !old_used[oi] {
                queues
                    .entry(&old_blocks[oi].content_hash)
                    .or_default()
                    .push_back(oi);
            }
        }
        for &ni in new_set {
            if assign[ni].is_some() {
                continue;
            }
            if let Some(q) = queues.get_mut(&new_blocks[ni].content_hash) {
                if let Some(oi) = q.pop_front() {
                    assign[ni] = Some(ctx.rows[oi].id);
                    old_used[oi] = true;
                    carried_hash += 1;
                }
            }
        }
    }

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
    let removed: Vec<Uuid> = (0..old_blocks.len())
        .filter(|&i| !old_used[i])
        .map(|i| ctx.rows[i].id)
        .collect();

    let carry = Carry {
        ids: ids.clone(),
        minted,
        removed,
        carried_ref,
        carried_hash,
    };
    write::reconcile(ctx.vault, ctx.note_id, &new_parsed, &ctx.rows, &carry);

    let mut meta = write::write_meta(op, &carry);
    if let (serde_json::Value::Object(m), serde_json::Value::Object(extra)) =
        (&mut meta, meta_extra)
    {
        for (k, v) in extra {
            m.insert(k, v);
        }
    }
    let revision = write::new_revision(ctx.vault, ctx.note_id, Some(ctx.head), "api", &meta);
    let block_ids: Vec<Uuid> = result_new_idxs.iter().map(|&i| ids[i]).collect();
    op_result(revision, block_ids)
}

/// Ancestor chain of `idx` (enclosing addressable blocks), outermost first.
fn ancestors(parsed: &ParsedNote, idx: usize) -> Vec<usize> {
    let mut chain = Vec::new();
    let mut p = parsed.doc.blocks[idx].parent;
    while let Some(pi) = p {
        chain.push(pi as usize);
        p = parsed.doc.blocks[pi as usize].parent;
    }
    chain.reverse();
    chain
}

/// Pin the target's enclosing ancestors to the new parse's (their spans cover
/// the edit, so outside pairing can never carry them). Depth mismatch = the
/// splice restructured the nesting.
fn ancestor_pins(
    old_parsed: &ParsedNote,
    old_idx: usize,
    new_parsed: &ParsedNote,
    new_idx: usize,
) -> Vec<(usize, usize)> {
    let old_anc = ancestors(old_parsed, old_idx);
    let new_anc = ancestors(new_parsed, new_idx);
    if old_anc.len() != new_anc.len() {
        pm_error(
            Pm::SpliceRestructures,
            "splice changed the target's container nesting",
            &format!("ancestor depth {} → {}", old_anc.len(), new_anc.len()),
        );
    }
    old_anc.into_iter().zip(new_anc).collect()
}

/// The replacement's root in the new parse: starts at `line_start`, prefer the
/// expected kind (an item and its first paragraph share a first line).
fn find_root_at(
    probe: &ParsedNote,
    line_start: usize,
    want_kind: pgmind_core::BlockKind,
) -> Option<usize> {
    let at: Vec<usize> = probe
        .doc
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.span.start == line_start)
        .map(|(i, _)| i)
        .collect();
    at.iter()
        .copied()
        .find(|&i| probe.doc.blocks[i].kind == want_kind)
        .or(at.first().copied())
}

/// Which surviving new block carries the `^marker`, for A4/A5 provenance.
fn marker_holder(
    parsed_new: &ParsedNote,
    ids: &[usize],
    marker: Option<&str>,
) -> serde_json::Value {
    match marker {
        None => serde_json::Value::Null,
        Some(m) => ids
            .iter()
            .find(|&&i| parsed_new.doc.blocks[i].block_ref_id.as_deref() == Some(m))
            .map(|_| serde_json::Value::String(m.to_string()))
            .unwrap_or(serde_json::Value::Null),
    }
}

// ------------------------------------------------------------------
// The five operations (SQL surface in the knowledge schema)
// ------------------------------------------------------------------

#[pg_schema]
mod knowledge {
    use super::*;

    /// RFC-004 A2: every parsed block mints. Anchors at top level (or
    /// item-level when the fragment parses as a single list).
    #[pg_extern(requires = ["pgmind_storage"])]
    fn insert_blocks(
        path: &str,
        fragment: Markdown,
        before: default!(Option<Uuid>, "NULL"),
        after: default!(Option<Uuid>, "NULL"),
    ) -> pgrx::composite_type!('static, "pgmind.op_result") {
        if before.is_some() && after.is_some() {
            pm_error(
                Pm::InvalidAnchor,
                "give before OR after, not both",
                "insert_blocks",
            );
        }
        let vault = store::current_vault();
        let note = store::note_by_path_or_err(vault, path);
        let source = store::source_of(&note);
        let parsed = parse_note(&source);
        let rows = store::blocks_of(note.id);
        if rows.len() != parsed.doc.blocks.len() {
            pgrx::error!("pgmind: stored blocks disagree with parse — run pgmind.verify_note");
        }
        let (frag, roots) = parse_fragment(&fragment.0);
        if roots.is_empty() {
            pm_error(
                Pm::FragmentArity,
                "fragment contains no blocks",
                "insert_blocks",
            );
        }
        let ctx = NoteCtx {
            vault,
            note_id: note.id,
            head: note.head_revision,
            source: source.clone(),
            rows,
            parsed,
        };

        let anchor = before.or(after);
        if let Some(anchor_id) = anchor {
            let ai = ctx
                .rows
                .iter()
                .position(|r| r.id == anchor_id)
                .unwrap_or_else(|| {
                    pm_error(
                        Pm::BlockNotFound,
                        "anchor block not found",
                        &format!("id {anchor_id}"),
                    )
                });
            let ab = &ctx.parsed.doc.blocks[ai];
            if ab.kind == pgmind_core::BlockKind::ListItem {
                return insert_item_level(&ctx, ai, &frag, &roots, before.is_some());
            }
            if ab.parent.is_some() || !is_top_level_child(&ctx.parsed, ai) {
                pm_error(
                    Pm::InvalidAnchor,
                    "anchor must be a top-level block or a list item",
                    &format!("id {anchor_id}"),
                );
            }
            insert_top_level(&ctx, Some(ai), &frag, before.is_some())
        } else {
            insert_top_level(&ctx, None, &frag, false)
        }
    }

    /// RFC-004 A2: ID kept by caller assertion; fragment arity exactly 1;
    /// wholesale subtree replacement with subtree carry.
    #[pg_extern(requires = ["pgmind_storage"])]
    fn update_block(
        block_id: Uuid,
        fragment: Markdown,
    ) -> pgrx::composite_type!('static, "pgmind.op_result") {
        let (ctx, idx) = load_ctx_by_block(block_id);
        let (frag, roots) = parse_fragment(&fragment.0);
        if roots.len() != 1 {
            pm_error(
                Pm::FragmentArity,
                "update_block fragment must contain exactly one block",
                &format!("found {}", roots.len()),
            );
        }
        if fragment_has_container_children(&frag, &roots) {
            pm_error(
                Pm::ContainerConstraint,
                "fragment must not contain nested containers (v1)",
                "update_block",
            );
        }
        let target = &ctx.parsed.doc.blocks[idx];
        let froot = &frag.doc.blocks[roots[0]];
        let target_cc = container_children(&ctx.parsed, idx);
        if !target_cc.is_empty() && froot.kind != pgmind_core::BlockKind::ListItem {
            pm_error(
                Pm::ContainerConstraint,
                "target has container children; replacement must stay a list item (v1)",
                &format!("target {}", block_id),
            );
        }

        let (own_start, own_end_raw) = own_range(&ctx.parsed, idx);
        let own_end = content_end(&ctx.source, own_start, own_end_raw);
        let sub_end = subtree_end(&ctx.parsed, idx);

        // Preserved decoration on line 1, replacement text, continuation prefix.
        let frag_root_is_item = froot.kind == pgmind_core::BlockKind::ListItem;
        let keep = if target.kind == pgmind_core::BlockKind::ListItem && frag_root_is_item {
            target.first_line_strip_outer
        } else {
            target.first_line_strip_full
        };
        let frag_text = &frag.source[froot.span.start..froot.span.end];
        let cont = if frag_root_is_item {
            target.line_prefix.clone()
        } else {
            target.cont_prefix.clone()
        };
        let mut replacement = decorate(frag_text.trim_end_matches('\n'), &cont);
        replacement.push('\n');

        let line_start = ctx.source[..own_start]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        // Nested spans start at their LINE start (decoration included), so the
        // preserved decoration is the first `keep` bytes of that line.
        let preserved = &ctx.source[line_start..(line_start + keep).min(own_end)];
        let mut new_source = String::with_capacity(ctx.source.len());
        new_source.push_str(&ctx.source[..line_start]);
        new_source.push_str(preserved);
        new_source.push_str(&replacement);
        new_source.push_str(&ctx.source[own_end..]);

        let edit = (line_start, line_start + preserved.len() + replacement.len());
        let probe = parse_note(&new_source);
        let Some(pin_new) = find_root_at(&probe, line_start, froot.kind) else {
            pm_error(
                Pm::SpliceRestructures,
                "replacement dissolved the target",
                &format!("block {block_id}"),
            );
        };
        let mut pins = ancestor_pins(&ctx.parsed, idx, &probe, pin_new);
        pins.push((idx, pin_new));
        // Region: old subtree; new = pinned root's subtree (subtree carry).
        let probe_sub_end = subtree_end(&probe, pin_new);
        let old_desc: Vec<usize> = (idx + 1..sub_end).collect();
        let new_desc: Vec<usize> = (pin_new + 1..probe_sub_end).collect();

        let marker = ctx.parsed.doc.blocks[idx].block_ref_id.clone();
        commit_op(
            &ctx,
            "update",
            new_source,
            (idx, sub_end),
            edit,
            pins,
            false,
            vec![(old_desc, new_desc)],
            json!({ "target": block_id.to_string(),
                    "marker_to": marker_holder(&probe, &[pin_new], marker.as_deref()) }),
            vec![pin_new],
        )
    }

    /// RFC-004 A2: ID kept; content and hash unchanged; position recomputed.
    /// v1: reorders siblings within one container or at top level.
    #[pg_extern(requires = ["pgmind_storage"])]
    fn move_block(
        block_id: Uuid,
        before: default!(Option<Uuid>, "NULL"),
        after: default!(Option<Uuid>, "NULL"),
    ) -> pgrx::composite_type!('static, "pgmind.op_result") {
        if before.is_some() == after.is_some() {
            pm_error(
                Pm::InvalidAnchor,
                "give exactly one of before/after",
                "move_block",
            );
        }
        let (ctx, idx) = load_ctx_by_block(block_id);
        let anchor_id = before.or(after).unwrap();
        let ai = ctx
            .rows
            .iter()
            .position(|r| r.id == anchor_id)
            .unwrap_or_else(|| {
                pm_error(
                    Pm::BlockNotFound,
                    "anchor block not found",
                    &format!("id {anchor_id}"),
                )
            });
        let tb = &ctx.parsed.doc.blocks[idx];
        let ab = &ctx.parsed.doc.blocks[ai];
        if tb.parent != ab.parent {
            pm_error(
                Pm::ContainerConstraint,
                "move_block reorders siblings only (v1)",
                &format!(
                    "target parent {:?}, anchor parent {:?}",
                    tb.parent, ab.parent
                ),
            );
        }
        let t_top = is_top_level_child(&ctx.parsed, idx);
        let a_top = is_top_level_child(&ctx.parsed, ai);
        if t_top != a_top || (!t_top && ctx.parsed.placement[idx].0 != ctx.parsed.placement[ai].0) {
            pm_error(
                Pm::ContainerConstraint,
                "move_block reorders siblings within one container (v1)",
                "target and anchor live in different containers",
            );
        }
        let t_end = subtree_end(&ctx.parsed, idx);
        let a_end = subtree_end(&ctx.parsed, ai);
        if (ai >= idx && ai < t_end) || (idx >= ai && idx < a_end) {
            pm_error(
                Pm::InvalidAnchor,
                "anchor inside the moved subtree",
                "move_block",
            );
        }

        let new_source = if t_top {
            move_tiles(&ctx, idx, ai, before.is_some())
        } else {
            move_lines(&ctx, idx, t_end, ai, a_end, before.is_some())
        };

        // All blocks are pinned positionally: new order = old order with the
        // subtree relocated. Compute the permutation, then let commit_op's
        // outside machinery verify hashes (region = whole note, pins = all).
        let n = ctx.parsed.doc.blocks.len();
        let moved: Vec<usize> = (idx..t_end).collect();
        let mut rest: Vec<usize> = (0..n).filter(|i| *i < idx || *i >= t_end).collect();
        let insert_at = {
            // position within `rest` where the moved run lands
            let a_pos = rest.iter().position(|&i| i == ai).unwrap();
            if before.is_some() {
                a_pos
            } else {
                let a_last = rest.iter().position(|&i| i == a_end - 1).unwrap_or(a_pos);
                a_last + 1
            }
        };
        let mut order: Vec<usize> = Vec::with_capacity(n);
        order.extend(&rest[..insert_at]);
        order.extend(&moved);
        order.extend(&rest[insert_at..]);
        rest.clear();

        let pins: Vec<(usize, usize)> =
            order.iter().enumerate().map(|(ni, &oi)| (oi, ni)).collect();
        let target_new = order.iter().position(|&oi| oi == idx).unwrap();
        commit_op(
            &ctx,
            "move",
            new_source,
            (0, n),          // whole note is "region": every block is pinned
            (0, usize::MAX), // no outside set — the permutation carries all
            pins,
            true,
            vec![],
            json!({ "target": block_id.to_string() }),
            vec![target_new],
        )
    }

    /// RFC-004 A2: fragment arity ≥ 2; first keeps the ID, rest mint.
    #[pg_extern(requires = ["pgmind_storage"])]
    fn split_block(
        block_id: Uuid,
        fragment: Markdown,
    ) -> pgrx::composite_type!('static, "pgmind.op_result") {
        let (ctx, idx) = load_ctx_by_block(block_id);
        let (frag, roots) = parse_fragment(&fragment.0);
        if roots.len() < 2 {
            pm_error(
                Pm::FragmentArity,
                "split_block fragment must contain at least two blocks",
                &format!("found {}", roots.len()),
            );
        }
        if fragment_has_container_children(&frag, &roots) {
            pm_error(
                Pm::ContainerConstraint,
                "fragment must not contain nested containers (v1)",
                "split_block",
            );
        }
        let target = &ctx.parsed.doc.blocks[idx];
        if !container_children(&ctx.parsed, idx).is_empty() {
            pm_error(
                Pm::ContainerConstraint,
                "split of a block with container children (v1)",
                &format!("target {block_id}"),
            );
        }
        if target.kind == pgmind_core::BlockKind::ListItem {
            let all_items = roots
                .iter()
                .all(|&r| frag.doc.blocks[r].kind == pgmind_core::BlockKind::ListItem);
            if !all_items {
                pm_error(
                    Pm::FragmentArity,
                    "splitting a list item requires a fragment that is a single list",
                    "split_block",
                );
            }
        }

        let (own_start, own_end_raw) = own_range(&ctx.parsed, idx);
        let own_end = content_end(&ctx.source, own_start, own_end_raw);
        let sub_end = subtree_end(&ctx.parsed, idx);
        let froot0 = &frag.doc.blocks[roots[0]];
        let frag_is_items = froot0.kind == pgmind_core::BlockKind::ListItem;
        let keep = if target.kind == pgmind_core::BlockKind::ListItem && frag_is_items {
            target.first_line_strip_outer
        } else {
            target.first_line_strip_full
        };
        let cont = if frag_is_items {
            target.line_prefix.clone()
        } else {
            target.cont_prefix.clone()
        };
        // Whole fragment (all roots) splices in, decorated; top-level targets
        // take it verbatim (tiles separate naturally).
        let frag_text = frag.source.trim_end_matches('\n');
        let mut replacement = if target.line_prefix.is_empty() && !frag_is_items {
            frag_text.to_string()
        } else {
            decorate(frag_text, &cont)
        };
        replacement.push('\n');

        let line_start = ctx.source[..own_start]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let preserved = &ctx.source[line_start..(line_start + keep).min(own_end)];
        let mut new_source = String::with_capacity(ctx.source.len());
        new_source.push_str(&ctx.source[..line_start]);
        new_source.push_str(preserved);
        new_source.push_str(&replacement);
        new_source.push_str(&ctx.source[own_end..]);
        let edit = (line_start, line_start + preserved.len() + replacement.len());

        let probe = parse_note(&new_source);
        let Some(first_new) = find_root_at(&probe, line_start, froot0.kind) else {
            pm_error(
                Pm::SpliceRestructures,
                "split dissolved the target",
                &format!("block {block_id}"),
            );
        };
        let mut pins = ancestor_pins(&ctx.parsed, idx, &probe, first_new);
        // Fragment roots in the new parse: siblings of first_new inside the
        // edited range, in order.
        let anc_new = ancestors(&probe, first_new);
        let new_roots: Vec<usize> = probe
            .doc
            .blocks
            .iter()
            .enumerate()
            .filter(|(i, b)| {
                b.span.start >= edit.0 && b.span.start < edit.1 && ancestors(&probe, *i) == anc_new
            })
            .map(|(i, _)| i)
            .collect();
        if new_roots.len() != roots.len() {
            pm_error(
                Pm::SpliceRestructures,
                "split fragments merged or restructured after splice",
                &format!(
                    "expected {} fragments, found {}",
                    roots.len(),
                    new_roots.len()
                ),
            );
        }
        pins.push((idx, first_new));
        let old_desc: Vec<usize> = (idx + 1..sub_end).collect();
        let first_sub_end = subtree_end(&probe, first_new);
        let new_desc: Vec<usize> = (first_new + 1..first_sub_end).collect();
        let marker = target.block_ref_id.clone();
        commit_op(
            &ctx,
            "split",
            new_source,
            (idx, sub_end),
            edit,
            pins,
            false,
            vec![(old_desc, new_desc)],
            json!({ "split": { "from": block_id.to_string() },
                    "marker_to": marker_holder(&probe, &new_roots, marker.as_deref()) }),
            new_roots,
        )
    }

    /// RFC-004 A2: ≥2 contiguous siblings become one caller-written block;
    /// `keep` (default first) survives, the rest retire with their descendants.
    #[pg_extern(requires = ["pgmind_storage"])]
    fn merge_blocks(
        block_ids: Vec<Uuid>,
        fragment: Markdown,
        keep: default!(Option<Uuid>, "NULL"),
    ) -> pgrx::composite_type!('static, "pgmind.op_result") {
        if block_ids.len() < 2 {
            pm_error(
                Pm::ContainerConstraint,
                "merge needs at least two blocks",
                "merge_blocks",
            );
        }
        let (ctx, _) = load_ctx_by_block(block_ids[0]);
        let mut idxs: Vec<usize> = block_ids
            .iter()
            .map(|id| {
                ctx.rows
                    .iter()
                    .position(|r| r.id == *id)
                    .unwrap_or_else(|| {
                        pm_error(
                            Pm::BlockNotFound,
                            "block not found in note",
                            &format!("id {id}"),
                        )
                    })
            })
            .collect();
        idxs.sort_unstable();
        idxs.dedup();
        if idxs.len() != block_ids.len() {
            pm_error(
                Pm::ContainerConstraint,
                "duplicate blocks in merge set",
                "merge_blocks",
            );
        }
        let parent = ctx.parsed.doc.blocks[idxs[0]].parent;
        let mut cursor = idxs[0];
        for &i in &idxs {
            if ctx.parsed.doc.blocks[i].parent != parent || i != cursor {
                pm_error(
                    Pm::ContainerConstraint,
                    "merge set must be contiguous siblings",
                    &format!("ord {i}"),
                );
            }
            cursor = subtree_end(&ctx.parsed, i);
        }
        for &i in &idxs[1..] {
            if !container_children(&ctx.parsed, i).is_empty() {
                pm_error(
                    Pm::ContainerConstraint,
                    "a non-surviving merge member owns container children (v1)",
                    &format!("ord {i}"),
                );
            }
        }
        let keep_id = keep.unwrap_or(ctx.rows[idxs[0]].id);
        let keep_idx = idxs
            .iter()
            .copied()
            .find(|&i| ctx.rows[i].id == keep_id)
            .unwrap_or_else(|| {
                pm_error(
                    Pm::BlockNotFound,
                    "keep must be in the merge set",
                    &format!("id {keep_id}"),
                )
            });

        let (frag, roots) = parse_fragment(&fragment.0);
        if roots.len() != 1 {
            pm_error(
                Pm::FragmentArity,
                "merge fragment must contain exactly one block",
                &format!("found {}", roots.len()),
            );
        }
        if fragment_has_container_children(&frag, &roots) {
            pm_error(
                Pm::ContainerConstraint,
                "fragment must not contain nested containers (v1)",
                "merge_blocks",
            );
        }

        let first = idxs[0];
        let run_end = cursor; // end of the last member's subtree
        let first_b = &ctx.parsed.doc.blocks[first];
        let froot = &frag.doc.blocks[roots[0]];
        let frag_root_is_item = froot.kind == pgmind_core::BlockKind::ListItem;
        let keep_deco = if first_b.kind == pgmind_core::BlockKind::ListItem && frag_root_is_item {
            first_b.first_line_strip_outer
        } else {
            first_b.first_line_strip_full
        };
        let cont = if frag_root_is_item {
            first_b.line_prefix.clone()
        } else {
            first_b.cont_prefix.clone()
        };
        let frag_text = &frag.source[froot.span.start..froot.span.end];
        let mut replacement = decorate(frag_text.trim_end_matches('\n'), &cont);
        replacement.push('\n');

        // Replace from the first member's line start through the end of the
        // run's last subtree line.
        let run_start = ctx.parsed.doc.blocks[first].span.start;
        let run_raw_end = (first..run_end)
            .map(|i| ctx.parsed.doc.blocks[i].span.end)
            .max()
            .unwrap();
        let run_bytes_end = content_end(&ctx.source, run_start, run_raw_end);
        let line_start = ctx.source[..run_start]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let preserved = &ctx.source[line_start..(line_start + keep_deco).min(run_bytes_end)];
        let mut new_source = String::with_capacity(ctx.source.len());
        new_source.push_str(&ctx.source[..line_start]);
        new_source.push_str(preserved);
        new_source.push_str(&replacement);
        new_source.push_str(&ctx.source[run_bytes_end..]);
        let edit = (line_start, line_start + preserved.len() + replacement.len());

        let probe = parse_note(&new_source);
        let Some(pin_new) = find_root_at(&probe, line_start, froot.kind) else {
            pm_error(
                Pm::SpliceRestructures,
                "merge dissolved into nothing",
                "merge_blocks",
            );
        };
        let mut pins = ancestor_pins(&ctx.parsed, first, &probe, pin_new);
        pins.push((keep_idx, pin_new));
        let keep_sub_end = subtree_end(&ctx.parsed, keep_idx);
        let old_desc: Vec<usize> = (keep_idx + 1..keep_sub_end).collect();
        let probe_sub_end = subtree_end(&probe, pin_new);
        let new_desc: Vec<usize> = (pin_new + 1..probe_sub_end).collect();
        let markers: Vec<String> = idxs
            .iter()
            .filter_map(|&i| ctx.parsed.doc.blocks[i].block_ref_id.clone())
            .collect();
        commit_op(
            &ctx,
            "merge",
            new_source,
            (first, run_end),
            edit,
            pins,
            false,
            vec![(old_desc, new_desc)],
            json!({ "merge": { "into": keep_id.to_string(),
                               "from": block_ids.iter().map(|u| u.to_string()).collect::<Vec<_>>() },
                    "marker_to": marker_holder(&probe, &[pin_new],
                                               markers.first().map(|s| s.as_str())) }),
            vec![pin_new],
        )
    }
}

/// Is `idx` a top-level document child (its own tile)?
fn is_top_level_child(parsed: &ParsedNote, idx: usize) -> bool {
    let b = &parsed.doc.blocks[idx];
    b.parent.is_none()
        && parsed.doc.top_level.iter().any(|t| t.start == b.span.start)
        && b.kind != pgmind_core::BlockKind::ListItem
}

/// Top-level insert: fragment tiles join the tile sequence with separator
/// synthesis; the note's final trailing trivia is preserved (RFC-003 D6).
fn insert_top_level(
    ctx: &NoteCtx,
    anchor: Option<usize>,
    frag: &ParsedNote,
    before: bool,
) -> PgHeapTuple<'static, pgrx::AllocatedByRust> {
    let preamble_end = ctx.parsed.doc.preamble.end;
    let old_trailing = trailing_newlines(&ctx.source);
    let mut tiles: Vec<String> = ctx
        .parsed
        .tiles
        .iter()
        .map(|(raw, _)| raw.clone())
        .collect();
    let frag_tiles: Vec<String> = frag.tiles.iter().map(|(raw, _)| raw.clone()).collect();
    if frag_tiles.is_empty() {
        pm_error(
            Pm::FragmentArity,
            "fragment contains no blocks",
            "insert_blocks",
        );
    }
    let pos = match anchor {
        None => tiles.len(),
        Some(ai) => {
            let a_start = ctx.parsed.doc.blocks[ai].span.start;
            let ti = ctx
                .parsed
                .doc
                .top_level
                .iter()
                .position(|t| t.start == a_start)
                .unwrap_or(0);
            if before {
                ti
            } else {
                ti + 1
            }
        }
    };
    let mut new_tiles: Vec<String> = Vec::with_capacity(tiles.len() + frag_tiles.len());
    new_tiles.extend(tiles.drain(..pos));
    new_tiles.extend(frag_tiles.iter().cloned());
    new_tiles.append(&mut tiles);
    rebuild_body(&mut new_tiles, &old_trailing);
    let mut new_source = ctx.source[..preamble_end].to_string();
    let body: String = new_tiles.concat();
    new_source.push_str(&body);

    // Region in the new source: the inserted tiles' byte range.
    let insert_start = preamble_end + new_tiles[..pos].iter().map(String::len).sum::<usize>();
    let insert_len = frag_tiles_len_after(&new_tiles, pos, frag_tiles.len());
    let edit = (insert_start, insert_start + insert_len);

    let probe = parse_note(&new_source);
    let result_idxs: Vec<usize> = probe
        .doc
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.span.start >= edit.0 && b.span.start < edit.1)
        .map(|(i, _)| i)
        .collect();
    commit_op(
        ctx,
        "insert",
        new_source,
        (usize::MAX, usize::MAX), // empty old region
        edit,
        vec![],
        false,
        vec![],
        json!({}),
        result_idxs,
    )
}

/// After separator synthesis the inserted tiles may have grown; measure them.
fn frag_tiles_len_after(tiles: &[String], pos: usize, count: usize) -> usize {
    tiles[pos..pos + count].iter().map(String::len).sum()
}

/// Separator synthesis over a tile sequence: every tile but the last ends with
/// a blank line; the final tile carries `old_trailing` exactly.
fn rebuild_body(tiles: &mut [String], old_trailing: &str) {
    let n = tiles.len();
    for (i, t) in tiles.iter_mut().enumerate() {
        if i + 1 < n {
            let owned = std::mem::take(t);
            *t = blank_terminated(owned);
        } else {
            let body = t.trim_end_matches('\n').to_string();
            *t = body + old_trailing;
        }
    }
}

/// Item-level insert: fragment items splice adjacent to the anchor item,
/// re-marked to the destination list's marker/numbering style.
fn insert_item_level(
    ctx: &NoteCtx,
    ai: usize,
    frag: &ParsedNote,
    roots: &[usize],
    before: bool,
) -> PgHeapTuple<'static, pgrx::AllocatedByRust> {
    let all_items = roots
        .iter()
        .all(|&r| frag.doc.blocks[r].kind == pgmind_core::BlockKind::ListItem);
    if !all_items {
        pm_error(
            Pm::InvalidAnchor,
            "item-level insert requires the fragment to be a single list",
            "insert_blocks",
        );
    }
    let anchor = &ctx.parsed.doc.blocks[ai];
    let a_end = subtree_end(&ctx.parsed, ai);
    // Insert position: line start of the anchor item, or after its subtree.
    let (pos, _line_start) = if before {
        let ls = ctx.source[..anchor.span.start]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        (ls, ls)
    } else {
        let end = (ai..a_end)
            .map(|i| ctx.parsed.doc.blocks[i].span.end)
            .max()
            .unwrap();
        (end, end)
    };
    // Re-mark fragment items to the anchor's marker style.
    let anchor_line_start = ctx.source[..anchor.span.start]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let anchor_marker = &ctx.source[anchor.span.start
        ..anchor.span.start
            + (anchor.first_line_strip_full - anchor.first_line_strip_outer)
                .min(ctx.source.len() - anchor.span.start)];
    let _ = anchor_line_start;
    let mut spliced = String::new();
    for &r in roots {
        let item = &frag.doc.blocks[r];
        let item_text = &frag.source[item.span.start..item.span.end];
        let own_marker_len = item.first_line_strip_full - item.first_line_strip_outer;
        let remarked = remark_item(item_text, own_marker_len, anchor_marker);
        let decorated = decorate(remarked.trim_end_matches('\n'), &anchor.line_prefix);
        spliced.push_str(&anchor.line_prefix);
        spliced.push_str(&decorated);
        spliced.push('\n');
    }
    let mut new_source = String::with_capacity(ctx.source.len() + spliced.len());
    new_source.push_str(&ctx.source[..pos]);
    new_source.push_str(&spliced);
    new_source.push_str(&ctx.source[pos..]);
    let edit = (pos, pos + spliced.len());

    let probe = parse_note(&new_source);
    let result_idxs: Vec<usize> = probe
        .doc
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.span.start >= edit.0 && b.span.start < edit.1)
        .map(|(i, _)| i)
        .collect();
    // Ancestors of the inserted items (the anchor's enclosing chain) intersect
    // the edit range and must be pinned explicitly.
    let anc_pins: Vec<(usize, usize)> = match result_idxs.first() {
        Some(&first_new) => ancestor_pins(&ctx.parsed, ai, &probe, first_new)
            .into_iter()
            .filter(|&(oi, _)| oi != ai)
            .collect(),
        None => vec![],
    };
    commit_op(
        ctx,
        "insert",
        new_source,
        (usize::MAX, usize::MAX),
        edit,
        anc_pins,
        false,
        vec![],
        json!({}),
        result_idxs,
    )
}

/// Swap an item's marker for the destination list's (canonical decoration:
/// unordered marker char swap; ordered delimiter swap, numbers kept).
fn remark_item(item_text: &str, own_marker_len: usize, dest_marker: &str) -> String {
    let own = &item_text[..own_marker_len.min(item_text.len())];
    let rest = &item_text[own_marker_len.min(item_text.len())..];
    let dest_trim = dest_marker.trim_start();
    let own_trim = own.trim_start();
    let dest_is_ordered = dest_trim.starts_with(|c: char| c.is_ascii_digit());
    let own_is_ordered = own_trim.starts_with(|c: char| c.is_ascii_digit());
    let new_marker = if dest_is_ordered == own_is_ordered {
        if dest_is_ordered {
            // keep own number, swap delimiter to the destination's
            let delim = dest_trim
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .chars()
                .next()
                .unwrap_or('.');
            let digits: String = own_trim
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            let tail: String = own_trim
                .chars()
                .skip_while(|c| c.is_ascii_digit())
                .skip(1) // own delimiter
                .collect();
            format!("{digits}{delim}{tail}")
        } else {
            let dest_char = dest_trim.chars().next().unwrap_or('-');
            let mut s: Vec<char> = own_trim.chars().collect();
            if !s.is_empty() {
                s[0] = dest_char;
            }
            s.into_iter().collect()
        }
    } else {
        // ordered vs unordered mismatch: use the destination marker verbatim
        dest_marker.to_string()
    };
    format!("{new_marker}{rest}")
}

/// Top-level move: tile-array surgery + separator synthesis.
fn move_tiles(ctx: &NoteCtx, idx: usize, ai: usize, before: bool) -> String {
    let t_start = ctx.parsed.doc.blocks[idx].span.start;
    let a_start = ctx.parsed.doc.blocks[ai].span.start;
    let ti = ctx
        .parsed
        .doc
        .top_level
        .iter()
        .position(|t| t.start == t_start)
        .unwrap();
    let taj = ctx
        .parsed
        .doc
        .top_level
        .iter()
        .position(|t| t.start == a_start)
        .unwrap();
    let old_trailing = trailing_newlines(&ctx.source);
    let mut tiles: Vec<String> = ctx
        .parsed
        .tiles
        .iter()
        .map(|(raw, _)| raw.clone())
        .collect();
    let moved = tiles.remove(ti);
    let mut pos = if before { taj } else { taj + 1 };
    if ti < pos {
        pos -= 1;
    }
    tiles.insert(pos, moved);
    rebuild_body(&mut tiles, &old_trailing);
    let mut s = ctx.source[..ctx.parsed.doc.preamble.end].to_string();
    for t in &tiles {
        s.push_str(t);
    }
    s
}

/// Sibling move inside one container: relocate the subtree's full lines.
fn move_lines(
    ctx: &NoteCtx,
    idx: usize,
    t_end: usize,
    ai: usize,
    a_end: usize,
    before: bool,
) -> String {
    let src = &ctx.source;
    let t_span_start = ctx.parsed.doc.blocks[idx].span.start;
    let t_span_end = (idx..t_end)
        .map(|i| ctx.parsed.doc.blocks[i].span.end)
        .max()
        .unwrap();
    let m_start = src[..t_span_start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let m_end = t_span_end;
    let moved = src[m_start..m_end].to_string();

    let a_span_start = ctx.parsed.doc.blocks[ai].span.start;
    let a_span_end = (ai..a_end)
        .map(|i| ctx.parsed.doc.blocks[i].span.end)
        .max()
        .unwrap();
    let insert_at = if before {
        src[..a_span_start].rfind('\n').map(|i| i + 1).unwrap_or(0)
    } else {
        a_span_end
    };

    let mut without = String::with_capacity(src.len());
    without.push_str(&src[..m_start]);
    without.push_str(&src[m_end..]);
    let adjusted = if insert_at >= m_end {
        insert_at - (m_end - m_start)
    } else {
        insert_at
    };
    let mut out = String::with_capacity(src.len());
    out.push_str(&without[..adjusted]);
    out.push_str(&moved);
    out.push_str(&without[adjusted..]);
    out
}
