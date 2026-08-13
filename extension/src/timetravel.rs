//! RFC-005 D3: reading history.
//!
//! Reconstruction starts at the nearest anchor **at or above** the target —
//! current state, or a `note_frame` — and applies pre-images backwards. Cost is
//! bounded by `pgmind.frame_every`, not by the note's depth.
//!
//! Two rules from D3 are load-bearing here and are enforced, not assumed:
//! reconstruction never invokes the parser (X2 — structure comes from the
//! stored vectors), and it never returns a partially-reconstructed document. A
//! read below `note.history_floor` raises PM011 rather than returning current
//! state dressed up as the past, and a concurrent write landing mid-read raises
//! rather than mixing two snapshots.

use std::collections::HashMap;

use pgrx::prelude::*;
use pgrx::{JsonB, Uuid};

use crate::errors::{pm_error, Pm};
use crate::store::{self, arg};

const OP_KEEP: i32 = 0;
const OP_INS: i32 = 1;

/// One block's content-visible state at some revision.
#[derive(Clone)]
pub struct BlockAt {
    pub kind: String,
    pub content: String,
    pub block_ref_id: Option<String>,
    pub attrs: serde_json::Value,
    pub parent: Option<Uuid>,
}

/// A note's full state at some revision.
pub struct StateAt {
    pub path: String,
    pub preamble: String,
    pub props: serde_json::Value,
    pub tiles: Vec<String>,
    pub ids: Vec<Uuid>,
    pub place: Vec<(i32, i32, i32)>,
    pub heads: Vec<Vec<String>>,
    pub blocks: HashMap<[u8; 16], BlockAt>,
}

impl StateAt {
    pub fn source(&self) -> String {
        let mut s = String::with_capacity(
            self.preamble.len() + self.tiles.iter().map(String::len).sum::<usize>(),
        );
        s.push_str(&self.preamble);
        for t in &self.tiles {
            s.push_str(t);
        }
        s
    }
}

/// One `note_revision` row as reconstruction reads it: seq, the three note-lane
/// pre-images, the two KEEP/INS scripts, and the changed positional slots.
type NoteRevRow = (
    i64,
    Option<String>,
    Option<String>,
    Option<serde_json::Value>,
    Vec<i32>,
    Vec<String>,
    Vec<i32>,
    Vec<Uuid>,
    Vec<i32>,
    Vec<i32>,
    Vec<i32>,
    serde_json::Value,
);

/// Rebuild the older vector from the newer one and a KEEP/INS script.
fn apply<T: Clone>(newer: &[T], ops: &[i32], payload: &[T]) -> Vec<T> {
    let mut out = Vec::new();
    let mut pi = 0usize;
    let mut i = 0usize;
    while i < ops.len() {
        match ops[i] {
            OP_KEEP => {
                let (start, len) = (ops[i + 1] as usize, ops[i + 2] as usize);
                if start + len > newer.len() {
                    pgrx::error!(
                        "pgmind: corrupt history script (KEEP {start}+{len} of {})",
                        newer.len()
                    );
                }
                out.extend_from_slice(&newer[start..start + len]);
                i += 3;
            }
            OP_INS => {
                let count = ops[i + 1] as usize;
                if pi + count > payload.len() {
                    pgrx::error!("pgmind: corrupt history script (INS {count} beyond payload)");
                }
                out.extend_from_slice(&payload[pi..pi + count]);
                pi += count;
                i += 2;
            }
            other => pgrx::error!("pgmind: corrupt history script (op {other})"),
        }
    }
    out
}

/// Resolve a target to `(note, seq)`, raising the errors D3 and D10 require.
/// PM010 ("no such revision") and PM011 ("no longer reconstructable") mean
/// opposite things to whoever debugs them, so they are never interchanged.
pub fn resolve_at(path: &str, at: At, vault: Option<&str>) -> (store::NoteRow, i64) {
    let note = store::note_by_path_or_err(store::resolve_vault(vault), path);
    let (head_seq, floor) = head_and_floor(note.id);
    let seq = match at {
        At::Revision(rev) => {
            let found: Option<i64> = Spi::get_one_with_args(
                "SELECT seq FROM pgmind.revision WHERE id = $1 AND note_id = $2",
                &[arg(rev), arg(note.id)],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure resolving revision: {e}"));
            match found {
                Some(s) => s,
                None => pm_error(
                    Pm::UnknownRevision,
                    "not a revision of this note",
                    &format!("revision {rev} on {path}"),
                ),
            }
        }
        At::Seq(s) => {
            if s > head_seq {
                pm_error(
                    Pm::UnknownRevision,
                    "seq is beyond the note's head",
                    &format!("seq {s} > head {head_seq}"),
                );
            }
            s
        }
        At::Time(ts) => {
            let found: Option<i64> = Spi::get_one_with_args(
                "SELECT max(seq) FROM pgmind.revision WHERE note_id = $1 AND created_at <= $2",
                &[arg(note.id), ts.into()],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure resolving timestamp: {e}"));
            match found {
                Some(s) => s,
                None => pm_error(
                    Pm::HistoryUnavailable,
                    "no revision at or before that time",
                    path,
                ),
            }
        }
    };
    if seq < floor {
        pm_error(
            Pm::HistoryUnavailable,
            "history below the retention floor was compacted or excised",
            &format!("seq {seq} < floor {floor} on {path}"),
        );
    }
    (note, seq)
}

pub enum At {
    Revision(Uuid),
    Seq(i64),
    Time(pgrx::datum::TimestampWithTimeZone),
}

fn head_and_floor(note: Uuid) -> (i64, i64) {
    Spi::connect(|client| {
        client
            .select(
                "SELECT (SELECT seq FROM pgmind.revision WHERE id = n.head_revision), n.history_floor
                   FROM pgmind.note n WHERE n.id = $1",
                None,
                &[arg(note)],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure reading head: {e}"))
            .map(|r| {
                (
                    r.get::<i64>(1).unwrap().unwrap_or(0),
                    r.get::<i64>(2).unwrap().unwrap_or(0),
                )
            })
            .next()
            .unwrap_or((0, 0))
    })
}

/// Current state, as the reconstruction anchor of last resort.
fn current_state(note: &store::NoteRow) -> StateAt {
    let rows = store::blocks_of(note.id);
    let tiles = store::tiles_of(note.id);
    let mut blocks = HashMap::new();
    let mut ids = Vec::with_capacity(rows.len());
    let mut place = Vec::with_capacity(rows.len());
    let mut heads = Vec::with_capacity(rows.len());
    for b in &rows {
        ids.push(b.id);
        place.push((b.tile_ord, b.start_in_tile, b.end_in_tile));
        heads.push(b.heading_path.clone());
        blocks.insert(
            *b.id.as_bytes(),
            BlockAt {
                kind: b.kind.clone(),
                content: b.content.clone(),
                block_ref_id: b.block_ref_id.clone(),
                attrs: b.attrs.clone(),
                parent: b.parent_block,
            },
        );
    }
    StateAt {
        path: store::path_of(note.id),
        preamble: note.preamble.clone(),
        props: store::properties_of(note.id),
        tiles,
        ids,
        place,
        heads,
        blocks,
    }
}

/// The nearest frame at or above `target`, if one exists.
fn frame_at_or_above(note: Uuid, target: i64) -> Option<(i64, StateAt)> {
    Spi::connect(|client| {
        let rows = client
            .select(
                "SELECT seq, path, preamble, props, tiles, block_ids, kinds, contents,
                        head_paths, parents, block_refs, attrs, placement
                   FROM pgmind.note_frame
                  WHERE note_id = $1 AND seq >= $2
                  ORDER BY seq LIMIT 1",
                Some(1),
                &[arg(note), target.into()],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure reading frame: {e}"));
        if rows.is_empty() {
            return None;
        }
        let r = rows.first();
        let seq: i64 = r.get(1).unwrap().unwrap();
        let ids: Vec<Uuid> = r
            .get::<Vec<Option<Uuid>>>(6)
            .unwrap()
            .unwrap()
            .into_iter()
            .flatten()
            .collect();
        let kinds: Vec<String> = flat_text(r.get(7).unwrap());
        let contents: Vec<String> = flat_text(r.get(8).unwrap());
        let heads: Vec<Vec<String>> =
            serde_json::from_value(r.get::<JsonB>(9).unwrap().unwrap().0).unwrap_or_default();
        let parents: Vec<Option<Uuid>> = r.get::<Vec<Option<Uuid>>>(10).unwrap().unwrap();
        let refs: Vec<String> = flat_text(r.get(11).unwrap());
        let attrs: Vec<serde_json::Value> =
            serde_json::from_value(r.get::<JsonB>(12).unwrap().unwrap().0).unwrap_or_default();
        let placement: Vec<i32> = r
            .get::<Vec<Option<i32>>>(13)
            .unwrap()
            .unwrap()
            .into_iter()
            .flatten()
            .collect();
        let mut blocks = HashMap::new();
        for (i, id) in ids.iter().enumerate() {
            blocks.insert(
                *id.as_bytes(),
                BlockAt {
                    kind: kinds.get(i).cloned().unwrap_or_default(),
                    content: contents.get(i).cloned().unwrap_or_default(),
                    block_ref_id: refs.get(i).filter(|s| !s.is_empty()).cloned(),
                    attrs: attrs.get(i).cloned().unwrap_or(serde_json::Value::Null),
                    parent: parents.get(i).copied().flatten(),
                },
            );
        }
        Some((
            seq,
            StateAt {
                path: r.get::<String>(2).unwrap().unwrap(),
                preamble: r.get::<String>(3).unwrap().unwrap(),
                props: r.get::<JsonB>(4).unwrap().unwrap().0,
                tiles: flat_text(r.get(5).unwrap()),
                place: placement.chunks(3).map(|c| (c[0], c[1], c[2])).collect(),
                heads,
                ids,
                blocks,
            },
        ))
    })
}

fn flat_text(v: Option<Vec<Option<String>>>) -> Vec<String> {
    v.unwrap_or_default().into_iter().flatten().collect()
}

/// Reconstruct a note's state at `seq`.
pub fn state_at(note: &store::NoteRow, seq: i64) -> StateAt {
    let (head_seq, _) = head_and_floor(note.id);
    let (anchor_seq, mut state) = match frame_at_or_above(note.id, seq) {
        // A frame above the target is only worth using if it is closer than head.
        Some((fseq, fstate)) if fseq < head_seq => (fseq, fstate),
        _ => (head_seq, current_state(note)),
    };
    if anchor_seq == seq {
        return state;
    }

    // Pre-images of every revision above the target, newest first. One query:
    // walking them one statement at a time would take a fresh snapshot per
    // statement (pgrx runs SPI read-only only when the transaction has no XID),
    // and a concurrent commit partway through would silently yield a document
    // one revision off.
    let rows: Vec<NoteRevRow> = Spi::connect(|client| {
        client
            .select(
                "SELECT seq, prev_path, prev_preamble, prev_props,
                            tile_ops, tile_payload, id_ops, id_payload,
                            place_idx, place_prev, head_idx, head_prev
                       FROM pgmind.note_revision
                      WHERE note_id = $1 AND seq > $2 AND seq <= $3
                      ORDER BY seq DESC",
                None,
                &[arg(note.id), seq.into(), anchor_seq.into()],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure reading history: {e}"))
            .map(|r| {
                (
                    r.get::<i64>(1).unwrap().unwrap(),
                    r.get::<String>(2).unwrap(),
                    r.get::<String>(3).unwrap(),
                    r.get::<JsonB>(4).unwrap().map(|j| j.0),
                    int_vec(r.get(5).unwrap()),
                    flat_text(r.get(6).unwrap()),
                    int_vec(r.get(7).unwrap()),
                    r.get::<Vec<Option<Uuid>>>(8)
                        .unwrap()
                        .unwrap_or_default()
                        .into_iter()
                        .flatten()
                        .collect(),
                    int_vec(r.get(9).unwrap()),
                    int_vec(r.get(10).unwrap()),
                    int_vec(r.get(11).unwrap()),
                    r.get::<JsonB>(12)
                        .unwrap()
                        .map(|j| j.0)
                        .unwrap_or(serde_json::Value::Null),
                )
            })
            .collect()
    });

    // Effect rows for the same range, grouped by seq.
    let mut effects: HashMap<i64, Vec<(Uuid, bool, Option<BlockAt>)>> = HashMap::new();
    Spi::connect(|client| {
        for r in client
            .select(
                "SELECT seq, block_id, existed, prev_kind::text, prev_content,
                        prev_block_ref_id, prev_attrs, prev_parent_block
                   FROM pgmind.block_revision
                  WHERE note_id = $1 AND seq > $2 AND seq <= $3",
                None,
                &[arg(note.id), seq.into(), anchor_seq.into()],
            )
            .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure reading effects: {e}"))
        {
            let s: i64 = r.get(1).unwrap().unwrap();
            let id: Uuid = r.get(2).unwrap().unwrap();
            let existed: bool = r.get(3).unwrap().unwrap();
            let prev = existed.then(|| BlockAt {
                kind: r.get::<String>(4).unwrap().unwrap_or_default(),
                content: r.get::<String>(5).unwrap().unwrap_or_default(),
                block_ref_id: r.get::<String>(6).unwrap(),
                attrs: r
                    .get::<JsonB>(7)
                    .unwrap()
                    .map(|j| j.0)
                    .unwrap_or(serde_json::Value::Null),
                parent: r.get::<Uuid>(8).unwrap(),
            });
            effects.entry(s).or_default().push((id, existed, prev));
        }
    });

    for (
        rseq,
        prev_path,
        prev_preamble,
        prev_props,
        tile_ops,
        tile_payload,
        id_ops,
        id_payload,
        place_idx,
        place_prev,
        head_idx,
        head_prev,
    ) in rows
    {
        // Position is inherited by block id from the newer state; only the
        // slots the pre-image names are overridden (RFC-005 D4).
        let post_place: HashMap<[u8; 16], (i32, i32, i32)> = state
            .ids
            .iter()
            .zip(state.place.iter())
            .map(|(id, p)| (*id.as_bytes(), *p))
            .collect();
        let post_heads: HashMap<[u8; 16], Vec<String>> = state
            .ids
            .iter()
            .zip(state.heads.iter())
            .map(|(id, h)| (*id.as_bytes(), h.clone()))
            .collect();

        state.tiles = apply(&state.tiles, &tile_ops, &tile_payload);
        state.ids = apply(&state.ids, &id_ops, &id_payload);
        if let Some(p) = prev_path {
            state.path = p;
        }
        if let Some(p) = prev_preamble {
            state.preamble = p;
        }
        if let Some(p) = prev_props {
            state.props = p;
        }

        let place_over: HashMap<usize, (i32, i32, i32)> = place_idx
            .iter()
            .enumerate()
            .map(|(k, &i)| {
                (
                    i as usize,
                    (
                        place_prev[k * 3],
                        place_prev[k * 3 + 1],
                        place_prev[k * 3 + 2],
                    ),
                )
            })
            .collect();
        let head_list: Vec<Vec<String>> = serde_json::from_value(head_prev).unwrap_or_default();
        let head_over: HashMap<usize, Vec<String>> = head_idx
            .iter()
            .enumerate()
            .map(|(k, &i)| (i as usize, head_list.get(k).cloned().unwrap_or_default()))
            .collect();

        state.place = state
            .ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                place_over
                    .get(&i)
                    .copied()
                    .or_else(|| post_place.get(id.as_bytes()).copied())
                    .unwrap_or((0, 0, 0))
            })
            .collect();
        state.heads = state
            .ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                head_over
                    .get(&i)
                    .cloned()
                    .or_else(|| post_heads.get(id.as_bytes()).cloned())
                    .unwrap_or_default()
            })
            .collect();

        for (id, existed, prev) in effects.remove(&rseq).unwrap_or_default() {
            match (existed, prev) {
                // The block did not exist before this revision.
                (false, _) => {
                    state.blocks.remove(id.as_bytes());
                }
                (true, Some(p)) => {
                    state.blocks.insert(*id.as_bytes(), p);
                }
                (true, None) => {
                    // Redacted pre-image (D7): the row survives, the content
                    // does not. Report it as erased rather than inheriting the
                    // newer value, which would be a lie about the past.
                    state.blocks.insert(
                        *id.as_bytes(),
                        BlockAt {
                            kind: "paragraph".into(),
                            content: String::new(),
                            block_ref_id: None,
                            attrs: serde_json::json!({"redacted": true}),
                            parent: None,
                        },
                    );
                }
            }
        }
    }
    state
}

fn int_vec(v: Option<Vec<Option<i32>>>) -> Vec<i32> {
    v.unwrap_or_default().into_iter().flatten().collect()
}

/// The `knowledge.*` time-travel surface (RFC-005 D3).
#[pgrx::pg_schema]
pub mod knowledge {
    use super::*;
    use pgrx::iter::TableIterator;

    /// One row per revision, newest first. Readable without reconstructing
    /// anything — which is why `verb` is on the revision row.
    #[allow(clippy::type_complexity)]
    #[pg_extern]
    fn history(
        path: &str,
        limit_n: default!(i64, 50),
        vault: default!(Option<String>, "NULL"),
    ) -> TableIterator<
        'static,
        (
            name!(revision, Uuid),
            name!(seq, i64),
            name!(verb, String),
            name!(author, String),
            name!(source, String),
            name!(message, Option<String>),
            name!(created_at, pgrx::datum::TimestampWithTimeZone),
            name!(blocks_changed, i64),
            name!(reconstructable, bool),
        ),
    > {
        let note = store::note_by_path_or_err(store::resolve_vault(vault.as_deref()), path);
        let rows = Spi::connect(|client| {
            client
                .select(
                    "SELECT r.id, r.seq, r.verb, r.author, r.source, r.message, r.created_at,
                            (SELECT count(*) FROM pgmind.block_revision br
                              WHERE br.note_id = r.note_id AND br.seq = r.seq),
                            r.seq >= n.history_floor
                       FROM pgmind.revision r JOIN pgmind.note n ON n.id = r.note_id
                      WHERE r.note_id = $1 ORDER BY r.seq DESC LIMIT $2",
                    None,
                    &[arg(note.id), limit_n.into()],
                )
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure reading history: {e}"))
                .map(|r| {
                    (
                        r.get::<Uuid>(1).unwrap().unwrap(),
                        r.get::<i64>(2).unwrap().unwrap(),
                        r.get::<String>(3).unwrap().unwrap(),
                        r.get::<String>(4).unwrap().unwrap(),
                        r.get::<String>(5).unwrap().unwrap(),
                        r.get::<String>(6).unwrap(),
                        r.get::<pgrx::datum::TimestampWithTimeZone>(7)
                            .unwrap()
                            .unwrap(),
                        r.get::<i64>(8).unwrap().unwrap_or(0),
                        r.get::<bool>(9).unwrap().unwrap_or(true),
                    )
                })
                .collect::<Vec<_>>()
        });
        TableIterator::new(rows)
    }

    #[pg_extern(name = "read_as_of")]
    fn read_as_of_rev(
        path: &str,
        at: Uuid,
        vault: default!(Option<String>, "NULL"),
    ) -> crate::Markdown {
        let (note, seq) = resolve_at(path, At::Revision(at), vault.as_deref());
        crate::Markdown(state_at(&note, seq).source())
    }

    #[pg_extern(name = "read_as_of")]
    fn read_as_of_seq(
        path: &str,
        at: i64,
        vault: default!(Option<String>, "NULL"),
    ) -> crate::Markdown {
        let (note, seq) = resolve_at(path, At::Seq(at), vault.as_deref());
        crate::Markdown(state_at(&note, seq).source())
    }

    #[pg_extern(name = "read_as_of")]
    fn read_as_of_time(
        path: &str,
        at: pgrx::datum::TimestampWithTimeZone,
        vault: default!(Option<String>, "NULL"),
    ) -> crate::Markdown {
        let (note, seq) = resolve_at(path, At::Time(at), vault.as_deref());
        crate::Markdown(state_at(&note, seq).source())
    }

    fn blocks_rows(
        note: &store::NoteRow,
        seq: i64,
    ) -> Vec<(i32, Uuid, String, String, Vec<String>)> {
        let s = state_at(note, seq);
        s.ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let b = s.blocks.get(id.as_bytes());
                (
                    i as i32,
                    *id,
                    b.map(|b| b.kind.clone()).unwrap_or_default(),
                    b.map(|b| b.content.clone()).unwrap_or_default(),
                    s.heads.get(i).cloned().unwrap_or_default(),
                )
            })
            .collect()
    }

    #[allow(clippy::type_complexity)]
    #[pg_extern(name = "blocks_as_of")]
    fn blocks_as_of_rev(
        path: &str,
        at: Uuid,
        vault: default!(Option<String>, "NULL"),
    ) -> TableIterator<
        'static,
        (
            name!(ord, i32),
            name!(block_id, Uuid),
            name!(kind, String),
            name!(content, String),
            name!(heading_path, Vec<String>),
        ),
    > {
        let (note, seq) = resolve_at(path, At::Revision(at), vault.as_deref());
        TableIterator::new(blocks_rows(&note, seq))
    }

    #[allow(clippy::type_complexity)]
    #[pg_extern(name = "blocks_as_of")]
    fn blocks_as_of_seq(
        path: &str,
        at: i64,
        vault: default!(Option<String>, "NULL"),
    ) -> TableIterator<
        'static,
        (
            name!(ord, i32),
            name!(block_id, Uuid),
            name!(kind, String),
            name!(content, String),
            name!(heading_path, Vec<String>),
        ),
    > {
        let (note, seq) = resolve_at(path, At::Seq(at), vault.as_deref());
        TableIterator::new(blocks_rows(&note, seq))
    }

    /// Block-level diff between two points. `moved` is knowable because
    /// position lives in the per-revision vectors, not in the effect rows.
    #[allow(clippy::type_complexity)]
    #[pg_extern]
    fn diff(
        path: &str,
        from_at: Uuid,
        to_at: Uuid,
        vault: default!(Option<String>, "NULL"),
    ) -> TableIterator<
        'static,
        (
            name!(block_id, Uuid),
            name!(change, String),
            name!(before, Option<String>),
            name!(after, Option<String>),
        ),
    > {
        let (note, from_seq) = resolve_at(path, At::Revision(from_at), vault.as_deref());
        let (_, to_seq) = resolve_at(path, At::Revision(to_at), vault.as_deref());
        let a = state_at(&note, from_seq);
        let b = state_at(&note, to_seq);
        let pos = |s: &StateAt| -> HashMap<[u8; 16], usize> {
            s.ids
                .iter()
                .enumerate()
                .map(|(i, id)| (*id.as_bytes(), i))
                .collect()
        };
        let (pa, pb) = (pos(&a), pos(&b));
        let mut out = Vec::new();
        for id in &a.ids {
            let k = id.as_bytes();
            match pb.get(k) {
                None => out.push((
                    *id,
                    "removed".to_string(),
                    a.blocks.get(k).map(|x| x.content.clone()),
                    None,
                )),
                Some(&j) => {
                    let (x, y) = (a.blocks.get(k), b.blocks.get(k));
                    let (cx, cy) = (x.map(|v| v.content.clone()), y.map(|v| v.content.clone()));
                    if cx != cy {
                        out.push((*id, "changed".to_string(), cx, cy));
                    } else if pa.get(k) != Some(&j) {
                        out.push((*id, "moved".to_string(), cx, cy));
                    }
                }
            }
        }
        for id in &b.ids {
            let k = id.as_bytes();
            if !pa.contains_key(k) {
                out.push((
                    *id,
                    "added".to_string(),
                    None,
                    b.blocks.get(k).map(|x| x.content.clone()),
                ));
            }
        }
        TableIterator::new(out)
    }

    /// Who last changed each block, and when it first appeared.
    ///
    /// A block_revision row exists at exactly the revisions where the block
    /// changed, so the newest one IS the last change — including the mint,
    /// which is a change like any other. Filtering these to `existed` (the
    /// pre-image flag: false ⇒ minted here) instead reported NULL author for
    /// every block nobody had edited since writing it, which is most of a
    /// vault and made the whole view useless.
    ///
    /// `first_revision` is NULL when the block's `existed = false` row was
    /// compacted away — the oldest and most-cited blocks are exactly the ones
    /// retention blinds, so the API reports the floor rather than returning it
    /// as though it were an origin (RFC-005 D8). For the same reason the
    /// attribution columns go NULL once retention has taken every row for a
    /// block: unknown is reported as unknown, never as the wrong author.
    #[allow(clippy::type_complexity)]
    #[pg_extern]
    fn blame(
        path: &str,
        vault: default!(Option<String>, "NULL"),
    ) -> TableIterator<
        'static,
        (
            name!(block_id, Uuid),
            name!(ord, i32),
            name!(first_revision, Option<Uuid>),
            name!(last_changed_revision, Option<Uuid>),
            name!(author, Option<String>),
            name!(source, Option<String>),
            name!(confidence, Option<f32>),
            name!(changed_at, Option<pgrx::datum::TimestampWithTimeZone>),
            name!(history_floor, i64),
        ),
    > {
        let note = store::note_by_path_or_err(store::resolve_vault(vault.as_deref()), path);
        let (_, floor) = head_and_floor(note.id);
        let rows = Spi::connect(|client| {
            client
                .select(
                    "SELECT b.id, b.ord,
                            (SELECT r.id FROM pgmind.block_revision br
                               JOIN pgmind.revision r ON r.note_id = br.note_id AND r.seq = br.seq
                              WHERE br.block_id = b.id AND NOT br.existed
                              ORDER BY br.seq LIMIT 1),
                            lc.id, lc.author, lc.source, lc.confidence, lc.created_at
                       FROM pgmind.block b
                       LEFT JOIN LATERAL (
                            SELECT r.id, r.author, r.source, br.confidence, r.created_at
                              FROM pgmind.block_revision br
                              JOIN pgmind.revision r ON r.note_id = br.note_id AND r.seq = br.seq
                             WHERE br.block_id = b.id
                             ORDER BY br.seq DESC LIMIT 1) lc ON true
                      WHERE b.note_id = $1 ORDER BY b.ord",
                    None,
                    &[arg(note.id)],
                )
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure in blame: {e}"))
                .map(|r| {
                    (
                        r.get::<Uuid>(1).unwrap().unwrap(),
                        r.get::<i32>(2).unwrap().unwrap(),
                        r.get::<Uuid>(3).unwrap(),
                        r.get::<Uuid>(4).unwrap(),
                        r.get::<String>(5).unwrap(),
                        r.get::<String>(6).unwrap(),
                        r.get::<f32>(7).unwrap(),
                        r.get::<pgrx::datum::TimestampWithTimeZone>(8).unwrap(),
                        floor,
                    )
                })
                .collect::<Vec<_>>()
        });
        TableIterator::new(rows)
    }

    /// Tombstone the note and record a revision. The live lanes are cleared —
    /// current state must stay current state — and history keeps everything, so
    /// `undelete_note` is a reconstruction rather than a resurrection of rows
    /// that were never really gone.
    #[pg_extern(name = "delete_note", requires = ["pgmind_storage"])]
    fn delete_note(
        path: &str,
        expected_head: default!(Option<Uuid>, "NULL"),
        vault: default!(Option<String>, "NULL"),
    ) -> Uuid {
        let vault = store::resolve_vault(vault.as_deref());
        let note = store::note_by_path_or_err(vault, path);
        store::lock_path_name(vault, path);
        let (locked_path, head) = store::lock_note_row(note.id);
        if locked_path != path {
            pm_error(
                Pm::NoteNotFound,
                "note was renamed by a concurrent transaction",
                &format!("path {path:?}"),
            );
        }
        store::cas_check(expected_head, head, path);

        // The pre-image of a deletion is the whole note: every block gets a
        // removal effect row, and the tile script rebuilds the bytes.
        let rows = store::blocks_of(note.id);
        let tiles = store::tiles_of(note.id);
        let empty = crate::write::parse_note("");
        let pre = crate::history::capture(
            &rows,
            &tiles,
            &note.preamble,
            &store::properties_of(note.id),
            Some(path),
            path,
            &crate::history::NewState {
                parsed: &empty,
                ids: &[],
                confidence: &[],
            },
        );
        for stmt in [
            "DELETE FROM pgmind.tag   WHERE note_id = $1",
            "DELETE FROM pgmind.edge  WHERE src_note = $1",
            "DELETE FROM pgmind.block WHERE note_id = $1",
            "DELETE FROM pgmind.tile  WHERE note_id = $1",
            "UPDATE pgmind.note SET tombstoned_at = now(), preamble = '',
                    properties = '{}'::jsonb WHERE id = $1",
        ] {
            Spi::run_with_args(stmt, &[arg(note.id)])
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure deleting note: {e}"));
        }
        let rev = crate::write::new_revision(
            vault,
            note.id,
            Some(head),
            "api",
            "delete_note",
            // Not `{"deleted": true}`: `verb` already says so, and RFC-011 D3
            // forbids putting a per-revision fact in the blob when a typed
            // column carries it. Retired 2026-08-06; the provenance gate found it.
            &serde_json::json!({}),
        );
        let seq = crate::write::seq_of(rev);
        crate::history::record(vault, note.id, rev, seq, &pre);
        rev
    }

    /// Reconstruct the note as of the revision before its deletion.
    #[pg_extern(name = "undelete_note", requires = ["pgmind_storage"])]
    fn undelete_note(path: &str, vault: default!(Option<String>, "NULL")) -> Uuid {
        let vault = store::resolve_vault(vault.as_deref());
        store::lock_path_name(vault, path);
        store::assert_path_free(vault, path);
        let found: Option<(Uuid, i64)> = Spi::connect(|client| {
            client
                .select(
                    "SELECT n.id, (SELECT seq FROM pgmind.revision WHERE id = n.head_revision)
                       FROM pgmind.note n
                      WHERE n.vault_id = $1 AND n.path = $2 AND n.tombstoned_at IS NOT NULL
                      ORDER BY n.tombstoned_at DESC LIMIT 1",
                    Some(1),
                    &[arg(vault), path.into()],
                )
                .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure finding tombstone: {e}"))
                .map(|r| {
                    (
                        r.get::<Uuid>(1).unwrap().unwrap(),
                        r.get::<i64>(2).unwrap().unwrap_or(0),
                    )
                })
                .next()
        });
        let Some((note_id, head_seq)) = found else {
            pm_error(
                Pm::NoteNotFound,
                "no deleted note at that path",
                &format!("path {path:?}"),
            );
        };
        let note = store::note_by_id_any(note_id)
            .unwrap_or_else(|| pgrx::error!("pgmind: tombstoned note row vanished"));
        if head_seq == 0 {
            pm_error(
                Pm::HistoryUnavailable,
                "the note has no revision before its deletion",
                path,
            );
        }
        let before = state_at(&note, head_seq - 1);
        Spi::run_with_args(
            "UPDATE pgmind.note SET tombstoned_at = NULL WHERE id = $1",
            &[arg(note_id)],
        )
        .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure clearing tombstone: {e}"));
        // Re-writing the reconstructed source restores both lanes through the
        // ordinary write path, so identity carries by the ordinary A3 rules
        // rather than by a second, private restore path that could drift.
        crate::write::write_note_in(vault, path, &before.source(), None)
    }

    /// Rename a note. Both paths are locked (lexicographic order, so two
    /// concurrent moves cannot deadlock), and the D5 creation-repair pass runs
    /// for BOTH the old and the new path: a rename can promote a basename match
    /// to exact and demote another in the same step, and edges are rewritten
    /// rather than guessed.
    #[pg_extern(name = "move_note", requires = ["pgmind_storage"])]
    fn move_note(
        path: &str,
        new_path: &str,
        expected_head: default!(Option<Uuid>, "NULL"),
        vault: default!(Option<String>, "NULL"),
    ) -> Uuid {
        let vault = store::resolve_vault(vault.as_deref());
        let target = pgmind_core::path::path_normalize(new_path);
        if !pgmind_core::path::path_is_valid(&target) {
            pm_error(
                Pm::InvalidPath,
                "invalid note path",
                &format!("path {new_path:?}"),
            );
        }
        let mut names = [path, target.as_str()];
        names.sort_unstable();
        for n in names {
            store::lock_path_name(vault, n);
        }
        let note = store::note_by_path_or_err(vault, path);
        let (locked_path, head) = store::lock_note_row(note.id);
        if locked_path != path {
            pm_error(
                Pm::NoteNotFound,
                "note was renamed by a concurrent transaction",
                &format!("path {path:?}"),
            );
        }
        store::cas_check(expected_head, head, path);
        if target != path {
            store::assert_path_free(vault, &target);
        }

        let rows = store::blocks_of(note.id);
        let tiles = store::tiles_of(note.id);
        let source = store::source_of(&note, &tiles);
        let parsed = crate::write::parse_note(&source);
        let ids: Vec<Uuid> = rows.iter().map(|b| b.id).collect();
        let pre = crate::history::capture(
            &rows,
            &tiles,
            &note.preamble,
            &store::properties_of(note.id),
            Some(path),
            &target,
            &crate::history::NewState {
                parsed: &parsed,
                ids: &ids,
                confidence: &[],
            },
        );
        Spi::run_with_args(
            "UPDATE pgmind.note SET path = $2 WHERE id = $1",
            &[arg(note.id), target.as_str().into()],
        )
        .unwrap_or_else(|e| pgrx::error!("pgmind: SPI failure moving note: {e}"));
        let rev = crate::write::new_revision(
            vault,
            note.id,
            Some(head),
            "api",
            "move_note",
            &serde_json::json!({"move": {"from": path, "to": target}}),
        );
        let seq = crate::write::seq_of(rev);
        crate::history::record(vault, note.id, rev, seq, &pre);
        // Both directions: the vacated path may demote edges that resolved to
        // it, and the new path may promote edges that were dangling or
        // basename-resolved elsewhere.
        store::repair_edges_on_creation(vault, path);
        store::repair_edges_on_creation(vault, &target);
        rev
    }
}
