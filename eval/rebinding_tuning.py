"""Threshold study for RFC-004 Part B (τ, τ_split).

Part B says its thresholds "are set by the Phase 3 revision of this RFC after
corpus tuning, not invented today". This module is that tuning: a faithful
simulation of the *drafted* pipeline, swept over the corpus, so the revision
proposes numbers that were measured.

Faithful means faithful to the draft, including its limits — same-kind only,
order-monotonic, bigram Dice, containment for split/merge. The point is to
learn what the drafted design is worth, not to quietly design a better one and
report its score under the same name. Where the simulation must decide
something the draft leaves open, the choice is named in a comment.

Nothing here runs inside Postgres and nothing here is shipped. It reads the
same before/after block lists the gate observes, reconstructs the residual that
passes 1-2 hand to Part B, and scores what the stages would have done with it.
"""

from __future__ import annotations

import re

from rebinding import classify

TOKEN = re.compile(r"[0-9a-z]+")


def bigrams(text: str) -> dict:
    """Token unigram+bigram multiset.

    The draft says "token-bigram Dice". Bigrams alone are unusable at the short
    end: a one-word fragment has no bigrams at all, so it shares nothing with
    anything, and `split/split-list-item` — where a three-word item is split
    and one fragment is the single word "beta" — is unsolvable by construction
    rather than by threshold. Unigrams make short and long content comparable
    on the same scale; bigrams keep word order mattering.
    """
    toks = TOKEN.findall(text.lower())
    out = {}
    for g in toks + [f"{a} {b}" for a, b in zip(toks, toks[1:])]:
        out[g] = out.get(g, 0) + 1
    return out


def dice(a: dict, b: dict) -> float:
    if not a or not b:
        return 0.0
    inter = sum(min(n, b.get(g, 0)) for g, n in a.items())
    return 2 * inter / (sum(a.values()) + sum(b.values()))


def containment(part: dict, whole: dict) -> float:
    """How much of `part` the `whole` covers. Part B's draft says "bigram
    overlap ≥ τ_split against the concatenation"; overlap of a fragment against
    a larger whole is asymmetric, so Dice would penalise exactly the size
    difference that defines a split. Containment is the reading that makes the
    rule mean anything."""
    if not part:
        return 0.0
    return sum(min(n, whole.get(g, 0)) for g, n in part.items()) / sum(part.values())


def residual(obs: dict) -> tuple:
    """What passes 1-2 leave for Part B: old blocks whose ID did not survive,
    and new blocks that were minted. Taken from the observed run rather than
    re-derived, so the study starts from what the engine actually does."""
    kept = {b[2] for b in obs["after"]}
    old = [b for b in obs["before"] if b[2] not in kept]
    carried_to = {a for _, a in obs["carried"]}
    new = [b for b in obs["after"] if b[0] not in carried_to]
    return old, new


def split_run(o: tuple, new: list, tau_split: float) -> int:
    """If `o` was split, `(ord of the fragment that must keep the ID, the run's
    coverage score)`; `(-1, 0.0)` if no run covers it. A run is ≥2 consecutive
    **same-kind peers in the residual** whose concatenation covers `o`.

    Consecutive among same-kind peers, not adjacent by ord and not adjacent in
    the residual: a list item is a `list_item` block plus the paragraph inside
    it, so two sibling items' `list_item` fragments have that paragraph sitting
    between them on both counts. Either stricter reading silently never fires
    on lists — measured, and it is why the drafted stage 2 misses
    `split/split-list-item` at every threshold it is given.

    Containment is required in BOTH directions: the run must cover `o`, and
    every fragment in it must itself be made of `o`. One direction is not
    enough and the corpus says so — a moved-and-edited paragraph is covered by
    the run {unrelated new paragraph, the moved one}, so a one-way rule calls
    it a split and hands the ID to the unrelated block. The reverse direction
    is what makes `split/split-decoy-lead-in` a decoy rather than a trap.
    """
    og, best = bigrams(o[3]), None
    peers = [x for x in new if x[1] == o[1]]
    for s in range(len(peers)):
        for e in range(s + 2, min(s + 5, len(peers) + 1)):
            run = peers[s:e]
            if any(containment(bigrams(x[3]), og) < tau_split for x in run):
                continue                       # some fragment is not from `o`
            score = containment(og, bigrams(" ".join(x[3] for x in run)))
            if score >= tau_split and (best is None or score > best[1]):
                best = (run[0][0], score)
    return best or (-1, 0.0)


def crosses(pair: tuple, anchors: set) -> bool:
    """Would binding `pair` cross an already-bound pair? Stage 1's own
    monotonicity is enforced within the residual, which is not enough: the
    deterministic passes have already bound blocks the residual cannot see, so
    a locally monotonic stage 1 still produces a globally crossing result.
    Measured on `near-duplicate/dup-edit-first-of-two`, where the composite
    binds (1→2) and (2→1)."""
    b, a = pair
    return any((b - ab) * (a - aa) <= 0 for ab, aa in anchors)


def stage1(old: list, new: list, tau: float, anchors: set = frozenset(),
           tau_split: float = None) -> list:
    """Modified-in-place: same-kind, score ≥ τ, order-monotonic.

    Maximum-weight monotonic matching by DP — the draft says alignment "must be
    order-monotonic (no crossing matches)" without saying how ties resolve;
    maximising total score is the reading that does not depend on iteration
    order.

    `anchors` extends monotonicity to the bindings the deterministic passes
    already made. `tau_split`, when given, enables split-run redirection: an old
    block covered by a run of new fragments binds to the run's FIRST fragment
    (A2's convention) instead of to whichever fragment happens to score
    highest. Both are corrections the corpus forced; both are off by default so
    the drafted pipeline can be measured as drafted.
    """
    n, m = len(old), len(new)
    w = [[0.0] * m for _ in range(n)]
    for i, o in enumerate(old):
        og = bigrams(o[3])
        keep, keep_score = split_run(o, new, tau_split) if tau_split is not None else (-1, 0.0)
        for j, x in enumerate(new):
            if o[1] == x[1] and not crosses((o[0], x[0]), anchors):
                if keep >= 0 and x[0] != keep:
                    continue          # a split fragment that is not the first
                # A split's first fragment scores as the RUN's coverage: on its
                # own it is a small piece of the original and would fall under τ.
                s = keep_score if keep == x[0] else dice(og, bigrams(x[3]))
                if s >= tau:
                    w[i][j] = s
    best = [[0.0] * (m + 1) for _ in range(n + 1)]
    for i in range(n - 1, -1, -1):
        for j in range(m - 1, -1, -1):
            take = w[i][j] + best[i + 1][j + 1] if w[i][j] else 0.0
            best[i][j] = max(take, best[i + 1][j], best[i][j + 1])
    out, i, j = [], 0, 0
    while i < n and j < m:
        if w[i][j] and best[i][j] == w[i][j] + best[i + 1][j + 1]:
            out.append((old[i][0], new[j][0], w[i][j]))
            i, j = i + 1, j + 1
        elif best[i + 1][j] >= best[i][j + 1]:
            i += 1
        else:
            j += 1
    return out


def stage2(old: list, new: list, tau_split: float) -> list:
    """Splits and merges over stage-1 leftovers. One old block against ≥2
    adjacent new blocks (split, first fragment carries) and the mirror image
    (merge, dominant source carries). Adjacency is by ord, and runs are capped
    at 4 — an uncapped scan is O(n·m²) on documents the engine will accept."""
    out, used_old, used_new = [], set(), set()
    for o in old:
        if o[0] in used_old:
            continue
        og, hit = bigrams(o[3]), None
        for s in range(len(new)):
            for e in range(s + 2, min(s + 5, len(new) + 1)):
                run = new[s:e]
                if any(x[0] in used_new for x in run) or any(x[1] != o[1] for x in run):
                    continue
                if run[-1][0] - run[0][0] != len(run) - 1:
                    continue                       # not adjacent in the document
                joined = bigrams(" ".join(x[3] for x in run))
                score = containment(og, joined)
                if score >= tau_split and (hit is None or score > hit[1]):
                    hit = (run, score)
        if hit:
            run, score = hit
            out.append((o[0], run[0][0], score))   # A2: first fragment keeps
            used_old.add(o[0])
            used_new.update(x[0] for x in run)
    for x in new:
        if x[0] in used_new:
            continue
        xg, hit = bigrams(x[3]), None
        for s in range(len(old)):
            for e in range(s + 2, min(s + 5, len(old) + 1)):
                run = old[s:e]
                if any(o[0] in used_old for o in run) or any(o[1] != x[1] for o in run):
                    continue
                if run[-1][0] - run[0][0] != len(run) - 1:
                    continue
                joined = bigrams(" ".join(o[3] for o in run))
                score = containment(joined, xg)
                if score >= tau_split and (hit is None or score > hit[1]):
                    hit = (run, score)
        if hit:
            run, score = hit
            dominant = max(run, key=lambda o: len(o[3]))   # A2: dominant source keeps
            out.append((dominant[0], x[0], score))
            used_new.add(x[0])
            used_old.update(o[0] for o in run)
    # The consumed sets are not the same as the bound ones: a split binds only
    # the first fragment but consumes them all, and a later stage must not get
    # to re-bind the retired ones.
    return out, used_old, used_new


def simulate(obs: dict, tau: float, tau_split: float, order: str = "1-2") -> set:
    """Every binding the engine would report with Part B in place.

    `order` selects which heuristic stage sees the residual first. The draft
    runs stage 1 then stage 2; "2-1" is the reverse.

    The reversal is worth measuring because stage 1 demonstrably breaks A2's
    first-keeps convention — a split fragment is similar to its parent, so
    stage 1 binds the parent to whichever fragment scores highest, which is
    often not the first. Running split detection first is the obvious fix and
    it does not work: on this corpus it repairs one case, fires on the decoy,
    and comes out behind on both recall and precision. Kept so the RFC revision
    can cite the measurement instead of repeating the intuition."""
    old, new = residual(obs)
    anchors = {tuple(p) for p in obs["carried"]}
    if order == "2-1":
        s2, done_old, done_new = stage2(old, new, tau_split)
        s1 = stage1([o for o in old if o[0] not in done_old],
                    [x for x in new if x[0] not in done_new], tau)
    elif order in ("split-aware", "monotone", "both"):
        # The two corrections the corpus forced, measured separately so the
        # revision can adopt one without the other:
        #   split-aware - split detection becomes a candidate filter INSIDE the
        #                 aligner, so A2's first-keeps holds instead of losing
        #                 to whichever fragment scores highest;
        #   monotone    - stage 1's order-monotonicity extends to the bindings
        #                 the deterministic passes already made.
        s1 = stage1(old, new, tau,
                    anchors=anchors if order in ("monotone", "both") else frozenset(),
                    tau_split=tau_split if order in ("split-aware", "both") else None)
        s2 = []
    else:
        s1 = stage1(old, new, tau)
        s2, _, _ = stage2([o for o in old if o[0] not in {b for b, _, _ in s1}],
                          [x for x in new if x[0] not in {a for _, a, _ in s1}], tau_split)
    return anchors | {(b, a) for b, a, _ in s1 + s2}


def evaluate(observations: list, tau: float, tsp: float, order: str = "1-2") -> dict:
    """One operating point, scored the same way the gate scores the engine."""
    tp = fp = fn = 0
    inferred_want = inferred_got = 0
    misbound = []
    for case, obs in observations:
        truth = set(case.same)
        got = simulate(obs, tau, tsp, order)
        tp += len(truth & got)
        fp += len(got - truth)
        fn += len(truth - got)
        misbound += [{"case": case.id, "binding": list(p)}
                     for p in sorted(got - truth) if any(p[0] == t[0] for t in truth)]
        want = {p for p in truth if classify(obs, p) == "inferred"}
        inferred_want += len(want)
        inferred_got += len(want & got)
    return {
        "tau": round(tau, 2), "tau_split": round(tsp, 2), "order": order,
        "tp": tp, "fp": fp, "fn": fn,
        "misbound": len(misbound), "misbound_detail": misbound,
        "recall": round(tp / (tp + fn), 4) if tp + fn else None,
        "precision": round(tp / (tp + fp), 4) if tp + fp else None,
        "recall_inferred": round(inferred_got / inferred_want, 4) if inferred_want else None,
    }


# Dice never exceeds 1.0, so this switches stage 1 off entirely — the ablation
# that says what stage 2 is worth on its own.
STAGE1_OFF = 1.01


def sweep(observations: list, taus: list, splits: list, detail: bool = False) -> list:
    """One row per (τ, τ_split). `observations` is [(case, obs)]."""
    grid = []
    for tau in taus:
        for tsp in splits:
            row = evaluate(observations, tau, tsp)
            if not detail:
                row.pop("misbound_detail")
            grid.append(row)
    return grid
