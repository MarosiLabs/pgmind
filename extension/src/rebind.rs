//! RFC-004 Part B — heuristic rebinding.
//!
//! Runs on the whole-document write path only, between A3's deterministic
//! passes and pass 3. The block ops never come here: they carry explicit
//! intent, and inferring identity for a caller who told us what they meant
//! would be strictly worse than obeying them.
//!
//! One stage, not three. The drafted pipeline had a similarity aligner, then a
//! containment stage for splits and merges; measurement against the adversarial
//! edit corpus (`eval/corpora/pgmind/rebinding/`) killed both halves of that
//! shape. Splits had to move *into* the aligner as a candidate filter, because
//! a split fragment is similar to its parent and a plain aligner hands the ID
//! to whichever fragment scores highest — not the first, which is what A2 says
//! gets it. Merges needed no stage at all: a merged block is overwhelmingly
//! similar to its dominant source, so the aligner already binds it.
//!
//! Everything here is pure computation over already-loaded rows (Law 2), scoped
//! to one note (Law 7). Nothing is stored that a reader cannot see is inferred:
//! every binding carries a confidence into `block_revision` (Law 8, RFC-005 H2).

use crate::write::{CarryInput, CarryState};

/// Minimum similarity for an alignment. Measured, not chosen: recall on the
/// corpus's inferred class is flat for every τ ≤ 0.5 and falls above it, so
/// this is the highest threshold that still reaches maximum recall.
const TAU: f32 = 0.5;

/// Minimum bidirectional containment for a split run. This one does NOT
/// discriminate — every value from 0.3 to 0.9 gives an identical result on the
/// corpus, because what decides a split is the two-way test, not where its
/// threshold sits. Kept as a named constant with a stated default rather than
/// advertised as a tunable nothing constrains.
const TAU_SPLIT: f32 = 0.6;

/// A split run is 2..=4 consecutive same-kind peers. Beyond four the scan cost
/// grows without the corpus showing anything to find.
const MAX_RUN: usize = 4;

/// O(n·m) alignment cells, on the write path, inside the transaction — so it
/// gets a ceiling. Over it, the whole heuristic is skipped and every unmatched
/// block mints (RFC-004 B3). The residual is *unmatched* blocks only, so an
/// ordinary edit is orders of magnitude below this; what blows it is a large
/// document rewritten wholesale, where carrying nothing is the right answer.
pub const CELL_BUDGET: usize = 40_000;

/// Feature multiset: token unigrams ∪ bigrams, hashed and sorted.
///
/// The draft said "token-bigram Dice". Bigrams alone are unusable at the short
/// end — a one-word fragment has none, so it shares nothing with anything, and
/// a list item split into "beta" / "and its consequences" is unsolvable by
/// construction rather than by threshold. Unigrams put short and long content
/// on the same scale; bigrams keep word order mattering.
///
/// Sorted `u64` rather than a map of strings: multiset intersection is then a
/// linear merge with no allocation per comparison, which is what makes the
/// O(n·m) loop affordable enough to have a budget at all.
pub(crate) fn features(text: &str) -> Vec<u64> {
    let lower = text.to_lowercase();
    let toks: Vec<u64> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(fnv1a)
        .collect();
    let mut out = Vec::with_capacity(toks.len() * 2);
    out.extend_from_slice(&toks);
    for w in toks.windows(2) {
        // Mix the pair so (a,b) and (b,a) differ: word order is the only thing
        // bigrams are here to contribute.
        out.push(w[0].wrapping_mul(0x100_0000_01b3) ^ w[1].rotate_left(17));
    }
    out.sort_unstable();
    out
}

pub(crate) fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Multiset intersection size of two sorted feature vectors.
pub(crate) fn intersect(a: &[u64], b: &[u64]) -> usize {
    let (mut i, mut j, mut n) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                n += 1;
                i += 1;
                j += 1;
            }
        }
    }
    n
}

pub(crate) fn dice(a: &[u64], b: &[u64]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    2.0 * intersect(a, b) as f32 / (a.len() + b.len()) as f32
}

/// How much of `part` appears in `whole`. Asymmetric on purpose: a fragment is
/// smaller than the block it came from, and Dice would penalise exactly the
/// size difference that defines a split.
pub(crate) fn containment(part: &[u64], whole: &[u64]) -> f32 {
    if part.is_empty() {
        return 0.0;
    }
    intersect(part, whole) as f32 / part.len() as f32
}

/// One binding the heuristic made.
pub struct Rebound {
    pub new_idx: usize,
    pub old_idx: usize,
    pub score: f32,
}

/// Why a note carried no inferred bindings, when that is worth recording.
pub enum Outcome {
    Ran(Vec<Rebound>),
    /// Residual too large; every unmatched block mints (B3's declared fallback).
    OverBudget { cells: usize },
}

/// Detect whether `o` was split, and if so which fragment must keep its ID.
///
/// Returns `(index into `new` of the first fragment, the run's coverage)`.
/// A run is 2..=MAX_RUN consecutive **same-kind peers of the residual** —
/// consecutive among peers, not adjacent by ord and not adjacent in the
/// residual, because a list item is a `list_item` block *plus the paragraph
/// inside it*, so two sibling items' same-kind fragments are never adjacent on
/// either reading. Both stricter readings silently never fire on lists.
///
/// Containment is required in BOTH directions: the run must cover `o`, and
/// every fragment in it must itself be made of `o`. One direction is not
/// enough — a moved-and-edited paragraph is "covered" by the run {unrelated new
/// paragraph, the moved one}, so a one-way rule calls it a split and hands the
/// ID to a block the author had never written before.
pub(crate) fn split_run(o_feats: &[u64], peers: &[(usize, &[u64])]) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;
    for s in 0..peers.len() {
        for e in (s + 2)..=(s + MAX_RUN).min(peers.len()) {
            let run = &peers[s..e];
            if run
                .iter()
                .any(|(_, f)| containment(f, o_feats) < TAU_SPLIT)
            {
                continue; // some fragment is not made of `o`
            }
            let mut joined: Vec<u64> = run.iter().flat_map(|(_, f)| f.iter().copied()).collect();
            joined.sort_unstable();
            let score = containment(o_feats, &joined);
            if score >= TAU_SPLIT && best.is_none_or(|(_, b)| score > b) {
                best = Some((run[0].0, score));
            }
        }
    }
    best
}

/// Align the residual. Maximum-weight order-monotonic matching by DP: the RFC
/// says no crossing matches and does not say how ties resolve, and maximising
/// total score is the reading that does not depend on iteration order.
///
/// Monotonicity is enforced *within the residual only*. Extending it to the
/// bindings the deterministic passes already made was measured and refused
/// (B5): it buys precision 0.969 → 0.989 and costs four inferred bindings,
/// every one of them a move. A rebinder that cannot follow a block across a
/// document is refusing the case it is most needed for.
pub fn rebind(input: &CarryInput, state: &CarryState) -> Outcome {
    let old_idxs: Vec<usize> = (0..input.old.len()).filter(|&i| !state.old_used[i]).collect();
    let new_idxs: Vec<usize> = (0..input.new.len())
        .filter(|&i| state.assign[i].is_none())
        .collect();
    if old_idxs.is_empty() || new_idxs.is_empty() {
        return Outcome::Ran(Vec::new());
    }
    let cells = old_idxs.len().saturating_mul(new_idxs.len());
    if cells > CELL_BUDGET {
        return Outcome::OverBudget { cells };
    }

    // Once per block, never per pair — the loop below is quadratic and this is
    // the difference between a budget that is generous and one that is a joke.
    let old_f: Vec<Vec<u64>> = old_idxs
        .iter()
        .map(|&i| features(input.old[i].content))
        .collect();
    let new_f: Vec<Vec<u64>> = new_idxs
        .iter()
        .map(|&i| features(input.new[i].content))
        .collect();

    let (n, m) = (old_idxs.len(), new_idxs.len());
    let mut w = vec![0.0f32; n * m];
    for (i, &oi) in old_idxs.iter().enumerate() {
        let kind = input.old[oi].kind;
        let peers: Vec<(usize, &[u64])> = new_idxs
            .iter()
            .enumerate()
            .filter(|(_, &ni)| input.new[ni].kind == kind)
            .map(|(j, _)| (j, new_f[j].as_slice()))
            .collect();
        let keep = split_run(&old_f[i], &peers);
        for (j, &ni) in new_idxs.iter().enumerate() {
            if input.new[ni].kind != kind {
                continue;
            }
            let s = match keep {
                // A split's first fragment scores as the RUN's coverage: on its
                // own it is a small piece of the original and would fall under τ.
                Some((first, score)) if first == j => score,
                Some(_) => continue, // a fragment that is not the first one
                None => dice(&old_f[i], &new_f[j]),
            };
            if s >= TAU {
                w[i * m + j] = s;
            }
        }
    }

    // best[i][j] = best achievable score aligning old[i..] with new[j..].
    let mut best = vec![0.0f32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            let cell = w[i * m + j];
            let take = if cell > 0.0 {
                cell + best[(i + 1) * (m + 1) + (j + 1)]
            } else {
                0.0
            };
            best[i * (m + 1) + j] = take
                .max(best[(i + 1) * (m + 1) + j])
                .max(best[i * (m + 1) + (j + 1)]);
        }
    }

    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        let cell = w[i * m + j];
        if cell > 0.0 && best[i * (m + 1) + j] == cell + best[(i + 1) * (m + 1) + (j + 1)] {
            out.push(Rebound {
                new_idx: new_idxs[j],
                old_idx: old_idxs[i],
                score: cell,
            });
            i += 1;
            j += 1;
        } else if best[(i + 1) * (m + 1) + j] >= best[i * (m + 1) + (j + 1)] {
            i += 1;
        } else {
            j += 1;
        }
    }
    Outcome::Ran(out)
}
