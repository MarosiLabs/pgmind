//! The five block operations (RFC-004 A2) and their splice mechanics
//! (RFC-003 D6): byte-surgical edits, full-note re-parse, outside-region
//! invariance (PM008), pins + scoped subtree carry, one revision per op.

use pgrx::prelude::*;
use pgrx::{heap_tuple::PgHeapTuple, Uuid};
use serde_json::json;

use crate::errors::{pm_error, Pm};
use crate::history;
use crate::store::{self, arg, BlockRow};
use crate::write::{self, parse_note, CarryInput, CarrySrc, CarryState, ParsedNote};
use crate::Markdown;

/// Byte offset of the start of the line containing `pos`. Seven copies of this
/// `rfind` lived inline in this file, where a one-byte error silently rewrites
/// user content.
fn line_start_of(src: &str, pos: usize) -> usize {
    src[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// Last byte covered by the ord range `[start, end)` — a subtree's extent.
fn span_end_of(parsed: &ParsedNote, start: usize, end: usize) -> usize {
    (start..end)
        .map(|i| parsed.doc.blocks[i].span.end)
        .max()
        .unwrap_or_else(|| parsed.doc.blocks[start].span.end)
}

/// Everything an op needs about its target's note, loaded once.
struct NoteCtx {
    vault: Uuid,
    note_id: Uuid,
    head: Uuid,
    /// Tiles as stored, so `reconcile` need not re-read them.
    tiles: Vec<String>,
    rows: Vec<BlockRow>,
    parsed: ParsedNote,
    /// Note-lane pre-image inputs (RFC-005 D4): both are what the history row
    /// records when a splice moves the preamble boundary or the note is moved.
    preamble: String,
    path: String,
}

impl NoteCtx {
    /// The note's full source. Held once, in `parsed`.
    fn src(&self) -> &str {
        &self.parsed.source
    }
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
    let note = store::note_by_id(note_id)
        .unwrap_or_else(|| pgrx::error!("pgmind: note row vanished mid-operation"));
    let tiles = store::tiles_of(note_id);
    let source = store::source_of(&note, &tiles);
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
        head: note.head_revision,
        tiles,
        rows,
        parsed,
        preamble: note.preamble.clone(),
        path: store::path_of(note_id),
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

/// Result of [`splice_replace`].
struct Splice {
    source: String,
    /// Rewritten byte range in the new source.
    edit: (usize, usize),
    /// Line the replacement starts on, for `find_root_at`.
    line_start: usize,
}

/// The byte splice shared by `update_block`, `split_block` and `merge_blocks`
/// (RFC-003 D6): keep the part of the target's first-line decoration the
/// fragment does not supply, re-prefix the fragment's continuation lines, and
/// rebuild the source around the replaced region.
///
/// This sequence used to be written out verbatim three times, and the copies
/// had already drifted — `split_block` carried an extra branch the others
/// lacked. (That branch was a no-op: it took the fragment verbatim when
/// `line_prefix` was empty, but an empty `line_prefix` means a top-level
/// target, whose `cont_prefix` is empty too, and `decorate` already returns
/// the fragment unchanged for an empty prefix.)
fn splice_replace(
    ctx: &NoteCtx,
    deco: &pgmind_core::Block,
    region_start: usize,
    region_end: usize,
    frag_text: &str,
    frag_root_is_item: bool,
) -> Splice {
    let keep = if deco.kind == pgmind_core::BlockKind::ListItem && frag_root_is_item {
        deco.first_line_strip_outer
    } else {
        deco.first_line_strip_full
    };
    let cont = if frag_root_is_item {
        &deco.line_prefix
    } else {
        &deco.cont_prefix
    };
    let src = ctx.src();
    let mut replacement = decorate(frag_text.trim_end_matches('\n'), cont);
    // Re-emit the region's own terminator, don't invent one. A note whose last
    // block has no trailing newline (`alpha\n\nbeta`) is byte-faithful storage,
    // and RFC-003 D6 says the final tile keeps exactly the trailing trivia it
    // had — `insert_blocks` and `move_block` already preserve it. Anywhere but
    // the very end of the source the newline is mandatory: without it the
    // replacement would merge into whatever follows.
    if region_end < src.len() || src[..region_end].ends_with('\n') {
        replacement.push('\n');
    }

    let line_start = line_start_of(src, region_start);
    // Nested spans start at their LINE start (decoration included), so the
    // preserved decoration is the first `keep` bytes of that line.
    let preserved = &src[line_start..(line_start + keep).min(region_end)];
    let mut source = String::with_capacity(src.len() + replacement.len());
    source.push_str(&src[..line_start]);
    source.push_str(preserved);
    source.push_str(&replacement);
    source.push_str(&src[region_end..]);
    let edit = (line_start, line_start + preserved.len() + replacement.len());
    Splice {
        source,
        edit,
        line_start,
    }
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

/// What an op tells [`commit_op`] about the splice it just performed. Named
/// fields rather than ten positional arguments, and `Option` rather than
/// `usize::MAX` sentinels whose meaning depended on arithmetic accidents
/// downstream.
#[derive(Default)]
struct OpCommit {
    /// Ord range replaced in the OLD parse. `None` = nothing was replaced.
    old_region: Option<(usize, usize)>,
    /// Rewritten byte range in the NEW source. `None` = the whole note was
    /// rewritten (move), so there is no outside set and every block must be
    /// pinned — which `commit_op` then checks explicitly.
    edit_range: Option<(usize, usize)>,
    /// (old idx → new idx) pins inside the region, in addition to outside pairs.
    region_pins: Vec<(usize, usize)>,
    /// Pins must keep (kind, hash) — true for move, whose bytes are untouched.
    pin_hash_strict: bool,
    /// Scoped subtree carry: (old idxs, new idxs).
    scoped: Vec<(Vec<usize>, Vec<usize>)>,
    /// New-parse index of the block carrying the surviving `^marker` (A5).
    /// Resolved to a UUID here, because identities do not exist until the
    /// carry has run.
    marker_to: Option<usize>,
    meta_extra: serde_json::Value,
    result_new_idxs: Vec<usize>,
}

/// Finish an op: outside-region invariance check (PM008), pins + scoped carry,
/// reconcile, revision.
///
/// Takes the caller's re-parse of the spliced source rather than the source
/// itself: every op already parsed it to locate its pins, and re-parsing here
/// meant a third full comrak pass and another whole-document copy per
/// operation (`ctx.parsed`, the probe, and this one) for an identical result.
fn commit_op(
    ctx: &NoteCtx,
    op: &str,
    new_parsed: ParsedNote,
    spec: OpCommit,
) -> PgHeapTuple<'static, pgrx::AllocatedByRust> {
    let OpCommit {
        old_region,
        edit_range,
        region_pins,
        pin_hash_strict,
        scoped,
        marker_to,
        meta_extra,
        result_new_idxs,
    } = spec;

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

    // RFC-003 D6's PM008 assertion also covers the case where there IS no
    // outside set: `move` rewrites the whole note and pins every block, so the
    // outside comparison below is 0-vs-0 and would let a re-parse that yields
    // extra blocks mint fresh IDs in silence. Assert the count and the pin
    // coverage directly.
    if edit_range.is_none() {
        if new_blocks.len() != old_blocks.len() {
            pm_error(
                Pm::SpliceRestructures,
                "splice changed the block count",
                &format!(
                    "{} blocks before, {} after",
                    old_blocks.len(),
                    new_blocks.len()
                ),
            );
        }
        if pinned_new.len() != new_blocks.len() {
            pm_error(
                Pm::SpliceRestructures,
                "whole-note splice left a block unaccounted for",
                &format!(
                    "{} of {} new blocks pinned",
                    pinned_new.len(),
                    new_blocks.len()
                ),
            );
        }
    }

    // Outside sets, in document order — pinned blocks (targets, ancestors,
    // move permutations) are handled by their pins, never by outside pairing.
    let old_outside: Vec<usize> = match old_region {
        Some((lo, hi)) => (0..old_blocks.len())
            .filter(|i| (*i < lo || *i >= hi) && !pinned_old.contains(i))
            .collect(),
        None => (0..old_blocks.len())
            .filter(|i| !pinned_old.contains(i))
            .collect(),
    };
    let new_outside: Vec<usize> = match edit_range {
        Some((lo, hi)) => (0..new_blocks.len())
            .filter(|i| {
                let sp = &new_blocks[*i].span;
                (sp.end <= lo || sp.start >= hi) && !pinned_new.contains(i)
            })
            .collect(),
        None => Vec::new(),
    };
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
    let old_src: Vec<CarrySrc> = old_blocks.iter().map(CarrySrc::from_block).collect();
    let new_src: Vec<CarrySrc> = new_blocks.iter().map(CarrySrc::from_block).collect();
    let old_ids: Vec<Uuid> = ctx.rows.iter().map(|r| r.id).collect();
    let input = CarryInput {
        old: &old_src,
        new: &new_src,
        old_ids: &old_ids,
    };
    let mut state = CarryState::new(old_blocks.len(), new_blocks.len());
    for (&oi, &ni) in old_outside.iter().zip(new_outside.iter()) {
        state.assign[ni] = Some(ctx.rows[oi].id);
        state.old_used[oi] = true;
    }
    for &(oi, ni) in &region_pins {
        state.assign[ni] = Some(ctx.rows[oi].id);
        state.old_used[oi] = true;
    }
    // RFC-004 A3 passes 1-2, scoped to each subtree. One normative
    // implementation, shared with knowledge.write via write::carry_scope.
    for (old_set, new_set) in &scoped {
        write::carry_scope(&input, old_set, new_set, &mut state);
    }
    let carry = write::finish_carry(&input, state);
    let ids = &carry.ids;
    // Pre-image first: reconcile is about to overwrite the bytes it records
    // (RFC-005 D4). The five block ops go through the same recorder as
    // knowledge.write, so history cannot drift between the two write paths.
    let new_state = history::NewState {
        parsed: &new_parsed,
        ids: &carry.ids,
    };
    let pre = history::capture(
        &ctx.rows,
        &ctx.tiles,
        &ctx.preamble,
        &store::properties_of(ctx.note_id),
        Some(&ctx.path),
        &ctx.path,
        &new_state,
    );
    write::reconcile(
        ctx.vault,
        ctx.note_id,
        &new_parsed,
        &ctx.rows,
        &ctx.tiles,
        &carry,
    );

    let mut meta = write::write_meta(op, &carry);
    let block_ids: Vec<Uuid> = result_new_idxs.iter().map(|&i| ids[i]).collect();
    if let serde_json::Value::Object(m) = &mut meta {
        if let serde_json::Value::Object(extra) = meta_extra {
            for (k, v) in extra {
                m.insert(k, v);
            }
        }
        // RFC-004 A4: `marker_to` names the surviving BLOCK by uuid and lives
        // INSIDE the split/merge object; `split.into` lists the resulting
        // blocks. All three are only knowable here, after the carry assigned
        // identities.
        if let Some(obj) = m.get_mut(op).and_then(|v| v.as_object_mut()) {
            obj.insert(
                "marker_to".into(),
                match marker_to {
                    Some(ni) => json!(ids[ni].to_string()),
                    None => serde_json::Value::Null,
                },
            );
            if op == "split" {
                obj.insert(
                    "into".into(),
                    json!(block_ids.iter().map(|u| u.to_string()).collect::<Vec<_>>()),
                );
            }
        }
    }
    // RFC-005 D4: revision.verb makes history() readable without reconstructing
    // anything. A4's provenance key stays short ("split"); the verb is the
    // public operation name.
    let verb = match op {
        "insert" => "insert_blocks",
        "update" => "update_block",
        "move" => "move_block",
        "split" => "split_block",
        "merge" => "merge_blocks",
        other => pgrx::error!("pgmind: unknown op {other}"),
    };
    let revision = write::new_revision(ctx.vault, ctx.note_id, Some(ctx.head), "api", verb, &meta);
    let seq = write::seq_of(revision);
    history::record(ctx.vault, ctx.note_id, revision, seq, &pre);
    history::maybe_frame(ctx.vault, ctx.note_id, seq, &ctx.path, &new_state);
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

/// Which surviving new block carries the `^marker` (RFC-004 A5), as a
/// new-parse index — `commit_op` resolves it to the uuid A4 asks for, because
/// identities do not exist until the carry has run.
///
/// Scans each candidate's whole subtree: for a list item the marker rides on
/// the item's INNER paragraph (RFC-002 D3 — the innermost block carries
/// `block_ref_id`), so scanning only the roots reported "no holder" for
/// markers that plainly survived.
fn marker_holder(parsed_new: &ParsedNote, roots: &[usize], marker: Option<&str>) -> Option<usize> {
    let m = marker?;
    for &r in roots {
        for i in r..subtree_end(parsed_new, r) {
            if parsed_new.doc.blocks[i].block_ref_id.as_deref() == Some(m) {
                return Some(i);
            }
        }
    }
    None
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
        let tiles = store::tiles_of(note.id);
        let source = store::source_of(&note, &tiles);
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
            tiles,
            rows,
            parsed,
            preamble: note.preamble.clone(),
            path: path.to_string(),
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
        let own_end = content_end(ctx.src(), own_start, own_end_raw);
        let sub_end = subtree_end(&ctx.parsed, idx);

        let frag_root_is_item = froot.kind == pgmind_core::BlockKind::ListItem;
        let frag_text = &frag.source[froot.span.start..froot.span.end];
        let sp = splice_replace(
            &ctx,
            target,
            own_start,
            own_end,
            frag_text,
            frag_root_is_item,
        );

        let probe = parse_note(&sp.source);
        let Some(pin_new) = find_root_at(&probe, sp.line_start, froot.kind) else {
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

        commit_op(
            &ctx,
            "update",
            probe,
            OpCommit {
                old_region: Some((idx, sub_end)),
                edit_range: Some(sp.edit),
                region_pins: pins,
                scoped: vec![(old_desc, new_desc)],
                meta_extra: json!({ "target": block_id.to_string() }),
                result_new_idxs: vec![pin_new],
                ..Default::default()
            },
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
        let rest: Vec<usize> = (0..n).filter(|i| *i < idx || *i >= t_end).collect();
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

        let pins: Vec<(usize, usize)> =
            order.iter().enumerate().map(|(ni, &oi)| (oi, ni)).collect();
        let target_new = order.iter().position(|&oi| oi == idx).unwrap();
        commit_op(
            &ctx,
            "move",
            parse_note(&new_source),
            OpCommit {
                // Whole note is the region and every block is pinned by the
                // permutation, so there is no outside set; commit_op checks
                // the block count and pin coverage instead.
                old_region: Some((0, n)),
                edit_range: None,
                region_pins: pins,
                pin_hash_strict: true,
                meta_extra: json!({ "target": block_id.to_string() }),
                result_new_idxs: vec![target_new],
                ..Default::default()
            },
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
        let own_end = content_end(ctx.src(), own_start, own_end_raw);
        let sub_end = subtree_end(&ctx.parsed, idx);
        let froot0 = &frag.doc.blocks[roots[0]];
        let frag_is_items = froot0.kind == pgmind_core::BlockKind::ListItem;
        // The whole fragment (all roots) splices in as one replacement.
        let sp = splice_replace(
            &ctx,
            target,
            own_start,
            own_end,
            &frag.source,
            frag_is_items,
        );
        let (new_source, edit, line_start) = (sp.source, sp.edit, sp.line_start);

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
        let marker_to = marker_holder(&probe, &new_roots, marker.as_deref());
        commit_op(
            &ctx,
            "split",
            probe,
            OpCommit {
                old_region: Some((idx, sub_end)),
                edit_range: Some(edit),
                region_pins: pins,
                scoped: vec![(old_desc, new_desc)],
                marker_to,
                // `into` is filled in by commit_op from result_new_idxs, once
                // the carry has assigned the resulting blocks their ids.
                meta_extra: json!({ "split": { "from": block_id.to_string() } }),
                result_new_idxs: new_roots,
                ..Default::default()
            },
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
        // Every member except the survivor is retired with its descendants, so
        // every member except the survivor must be checked. Testing `idxs[1..]`
        // was only equivalent while `keep` defaulted to the first member: with
        // an explicit `keep`, member 0's nested content was spliced away and
        // its rows deleted with no PM006.
        for &i in &idxs {
            if i != keep_idx && !container_children(&ctx.parsed, i).is_empty() {
                pm_error(
                    Pm::ContainerConstraint,
                    "a non-surviving merge member owns container children (v1)",
                    &format!("ord {i}"),
                );
            }
        }

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
        let frag_text = &frag.source[froot.span.start..froot.span.end];

        // Replace from the first member's line start through the end of the
        // run's last subtree line.
        let run_start = first_b.span.start;
        let run_raw_end = span_end_of(&ctx.parsed, first, run_end);
        let run_bytes_end = content_end(ctx.src(), run_start, run_raw_end);
        let sp = splice_replace(
            &ctx,
            first_b,
            run_start,
            run_bytes_end,
            frag_text,
            frag_root_is_item,
        );
        let (new_source, edit, line_start) = (sp.source, sp.edit, sp.line_start);

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
        let marker_to = marker_holder(&probe, &[pin_new], markers.first().map(|s| s.as_str()));
        commit_op(
            &ctx,
            "merge",
            probe,
            OpCommit {
                old_region: Some((first, run_end)),
                edit_range: Some(edit),
                region_pins: pins,
                scoped: vec![(old_desc, new_desc)],
                marker_to,
                meta_extra: json!({ "merge": { "into": keep_id.to_string(),
                               "from": block_ids.iter().map(|u| u.to_string()).collect::<Vec<_>>() } }),
                result_new_idxs: vec![pin_new],
                ..Default::default()
            },
        )
    }
}

/// Is `idx` a top-level document child (i.e. IS a tile)?
///
/// The block must span the whole tile, not merely start at it. Blockquotes are
/// not addressable, so a paragraph inside a top-level quote has `parent =
/// None` and starts at the tile boundary — matching on `t.start` alone
/// accepted it as top level, which let `insert_blocks` splice a new tile
/// outside the quote and `move_block` relocate the entire quote, both without
/// the PM005 the anchor guard is there to raise.
fn is_top_level_child(parsed: &ParsedNote, idx: usize) -> bool {
    let b = &parsed.doc.blocks[idx];
    b.parent.is_none()
        && b.kind != pgmind_core::BlockKind::ListItem
        && parsed
            .doc
            .top_level
            .iter()
            .any(|t| t.start == b.span.start && t.end == b.span.end)
}

/// Tile index of a top-level block, from the placement the parse already
/// computed (three ad-hoc rescans of `top_level` used to derive this, and they
/// disagreed on failure: one fell back to tile 0, two panicked).
fn tile_of(parsed: &ParsedNote, idx: usize) -> usize {
    parsed.placement[idx].0 as usize
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
    let old_trailing = trailing_newlines(ctx.src());
    let mut tiles: Vec<String> = ctx.parsed.tiles.clone();
    let frag_tiles: Vec<String> = frag.tiles.clone();
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
            let ti = tile_of(&ctx.parsed, ai);
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
    rebuild_body(&mut new_tiles, pos..pos + frag_tiles.len(), &old_trailing);
    let mut new_source = ctx.src()[..preamble_end].to_string();
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
        probe,
        OpCommit {
            edit_range: Some(edit),
            result_new_idxs: result_idxs,
            ..Default::default()
        },
    )
}

/// After separator synthesis the inserted tiles may have grown; measure them.
fn frag_tiles_len_after(tiles: &[String], pos: usize, count: usize) -> usize {
    tiles[pos..pos + count].iter().map(String::len).sum()
}

/// Separator synthesis at the seam a splice created (RFC-003 D6): the tiles in
/// `seam` and the tile immediately before it are blank-terminated, and the
/// final tile carries `old_trailing` exactly.
///
/// Scoped to the seam on purpose. Blank-terminating EVERY tile rewrote bytes
/// far outside the spliced span — `write('n', 'para\n# Heading\n')` produces
/// two tiles with no blank between them, and any later insert silently
/// rewrote them to `para\n\n# Heading\n\n`. PM008 cannot catch it, because
/// RFC-002 D7 strips trailing newline runs before hashing, so every outside
/// block's kind and content_hash are unchanged.
fn rebuild_body(tiles: &mut [String], seam: std::ops::Range<usize>, old_trailing: &str) {
    let n = tiles.len();
    if n == 0 {
        return;
    }
    let first = seam.start.saturating_sub(1);
    // Every seam tile that is followed by another must end with a blank line.
    for t in tiles
        .iter_mut()
        .take(seam.end.min(n.saturating_sub(1)))
        .skip(first)
    {
        let owned = std::mem::take(t);
        *t = blank_terminated(owned);
    }
    let body = tiles[n - 1].trim_end_matches('\n').to_string();
    tiles[n - 1] = body + old_trailing;
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
    let src = ctx.src();
    let anchor = &ctx.parsed.doc.blocks[ai];
    let a_end = subtree_end(&ctx.parsed, ai);
    // Insert position: line start of the anchor item, or after its subtree.
    let pos = if before {
        line_start_of(src, anchor.span.start)
    } else {
        span_end_of(&ctx.parsed, ai, a_end)
    };
    // The anchor's own list marker, to re-mark the fragment's items to.
    //
    // A block's span starts at its LINE start, so the enclosing container's
    // decoration occupies [0, strip_outer) of that line and the item's own
    // marker occupies [strip_outer, strip_full). Slicing (full - outer) bytes
    // from the line start instead returned the CONTAINER's prefix: for
    // `> - a` it yielded "> ", so `remark_item` re-marked the fragment with
    // '>' and the item was spliced in as a nested blockquote; for a nested
    // ordered list it yielded the indent, and the item dissolved into a
    // continuation line.
    let anchor_marker = slice_marker(src, anchor);
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
    let mut new_source = String::with_capacity(src.len() + spliced.len());
    new_source.push_str(&src[..pos]);
    new_source.push_str(&spliced);
    new_source.push_str(&src[pos..]);
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
        probe,
        OpCommit {
            edit_range: Some(edit),
            region_pins: anc_pins,
            result_new_idxs: result_idxs,
            ..Default::default()
        },
    )
}

/// A list item's OWN marker: the bytes of its first line between the enclosing
/// container's decoration and the end of its own. `get` rather than direct
/// indexing — these are parser-derived byte widths applied to user markdown,
/// and a slice that misses a char boundary must not panic inside a backend.
fn slice_marker<'a>(src: &'a str, item: &pgmind_core::Block) -> &'a str {
    let start = item.span.start + item.first_line_strip_outer;
    let end = item.span.start + item.first_line_strip_full;
    if end < start {
        return "";
    }
    src.get(start..end.min(src.len())).unwrap_or("")
}

/// Swap an item's marker for the destination list's (canonical decoration:
/// unordered marker char swap; ordered delimiter swap, numbers kept).
fn remark_item(item_text: &str, own_marker_len: usize, dest_marker: &str) -> String {
    let cut = own_marker_len.min(item_text.len());
    let (own, rest) = match (item_text.get(..cut), item_text.get(cut..)) {
        (Some(o), Some(r)) => (o, r),
        _ => ("", item_text),
    };
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
    let ti = tile_of(&ctx.parsed, idx);
    let taj = tile_of(&ctx.parsed, ai);
    let old_trailing = trailing_newlines(ctx.src());
    let mut tiles: Vec<String> = ctx.parsed.tiles.clone();
    let moved = tiles.remove(ti);
    let mut pos = if before { taj } else { taj + 1 };
    if ti < pos {
        pos -= 1;
    }
    tiles.insert(pos, moved);
    // Only the insertion seam needs separator synthesis; removing a tile
    // leaves its predecessor already blank-terminated.
    rebuild_body(&mut tiles, pos..pos + 1, &old_trailing);
    let mut s = ctx.src()[..ctx.parsed.doc.preamble.end].to_string();
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
    let src = ctx.src();
    let t_span_start = ctx.parsed.doc.blocks[idx].span.start;
    let m_start = line_start_of(src, t_span_start);
    let m_end = span_end_of(&ctx.parsed, idx, t_end);
    let moved = src[m_start..m_end].to_string();

    let a_span_start = ctx.parsed.doc.blocks[ai].span.start;
    let insert_at = if before {
        line_start_of(src, a_span_start)
    } else {
        span_end_of(&ctx.parsed, ai, a_end)
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
