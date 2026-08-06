# RFC-004: Block Identity & Rebinding Semantics

- **Status:** **Frozen 2026-08-06 (Phase 3 exited).** Part A accepted 2026-08-05 for Phase 2; Part B revised from corpus measurement and accepted 2026-08-06. (per plan §16: "accepted (living)" split). Two corrections were made at implementation, both measured, both recorded in place: the drafted **stage 2 was dropped** (B3) and the **alignment budget was declared** (B6) — the revision itself said the budget MUST exist before any code was written. Amended same day after adversarial review: fragment arity (parentless counting), subtree carry, op return contract, container-children constraints, PM008, marker/ID divergence. Amended again after Phase-2 code review: A3 pass 2 is section-first (two tiers), A4 `marker_to` is a uuid inside the split/merge object and `carried` is emitted for every carrying op. **Those post-review amendments were accepted by the owner 2026-08-05.** Part B's own Phase-3 gate — the adversarial edit corpus — has since been built and its results published, which is what made the Part B revision possible.
- **Phase:** 2-3
- **Owner:** project author
- **Created:** 2026-08-05 · **Accepted:** 2026-08-05 (Part A; amendments re-accepted the same day), 2026-08-06 (Part B as revised) · **Frozen:** 2026-08-06

## 1. Context

The audit's #1 finding (C1) is that stable block identity cannot come from a parser: every system that ships it — Notion, Yjs, ProseMirror — mints identity on the **write path**, and every attempt to recover identity from plain-text diffing is heuristic (GumTree/XyDiff literature). The handbook made this Law 4; the plan made it this RFC. Identity is what makes a block citable across edits (`path#^block @ revision`), what `blame` and `patch_block` address, and what keys the embedding hooks so users never re-embed unchanged content (Law 5).

This RFC has two parts with different maturity by design. **Part A** — what each write operation does to IDs, including the *deterministic* carry rules when a whole document is rewritten — is normative for Phase 2 and proposed for acceptance now. **Part B** — the heuristic rebinding pipeline for external whole-document replacement (sync, re-import) — is the project's #1 research problem. It was a structured draft with two invented thresholds; on 2026-08-06 it was rewritten from measurement against the adversarial edit corpus, accepted, and shipped. Splitting maturity this way kept Phase 2 purely deterministic — nothing accepted for Phase 2 involves a threshold or a similarity score — and it is why the corpus could measure a real baseline instead of grading a heuristic against itself.

Storage columns live in RFC-003; hashes and the block taxonomy are RFC-002 (frozen).

---

## Part A — Write-path identity (normative for Phase 2)

### A1. Minting

- A block ID is a **UUIDv7** (RFC 9562), minted **by the extension** at the moment a write creates a block. Never by parsing, never by the client, never `gen_random_uuid()` in SQL (UUIDv7's time-ordered prefix keeps the `block` PK index append-friendly; server-side `uuidv7()` is PG18-only, so the extension mints on all supported majors).
- IDs are **opaque**: consumers MUST NOT derive meaning (including creation time) from the bytes; ordering comes from `ord`, time from `created_at`/revisions.
- An ID is never reused, and never survives its note (cross-note identity does not exist; a copied block is a new block — A2).
- **Invariant (Law 4/5):** two blocks with identical content are still two blocks (same hash, different IDs); one block whose content changes is still one block (same ID, different hash). Every rule below is an application of this sentence.

### A2. Identity semantics of each operation

**The five block operations** (`insert_blocks`, `update_block`, `move_block`, `split_block`, `merge_blocks`) run in the caller's transaction, produce one `revision` row (RFC-003 D3), and return the named composite **`pgmind.op_result (revision uuid, block_ids uuid[])`** (RFC-003 D6) — `block_ids` lists the blocks the operation created or targeted, in document order. `knowledge.write()` is *not* under this contract: it returns the revision uuid alone (RFC-003 D6 governs), and its byte-identical short-circuit returns the existing head with **no** new revision row.

Three definitions used throughout:

- **Fragment arity** — the number of **parentless addressable blocks** in the fragment's standalone parse (blocks whose `parent` is NULL, i.e. not nested inside another addressable block; containers, having no block rows, never affect the count; descendants ride along). `new text` and `- new text` both have arity 1 (the latter's inner paragraph is a descendant); `- a\n- b` has arity 2. `PM004` is raised against this count.
- **Subtree carry** — whenever a fragment-taking op (`update_block`, `split_block`, `merge_blocks`) rewrites a target's subtree, the fragment's *descendant* blocks are matched against the target's existing non-container descendants by the A3 passes **scoped to that subtree** (pass 1 `^id` claims, pass 2 exact hash in document order, pass 3 mint/remove), with the explicitly targeted block's ID forced by the op's own rule. In the common case — an item whose inner paragraph text changed — the paragraph's hash changed and no `^id` claims it, so the inner paragraph **mints** (pinned in the gate; agents that want the inner paragraph's ID kept target it directly with `update_block`, whose caller assertion does exactly that). Subtree removals land in A4's `removed` list.
- **Container-children constraints (v1)** — `update_block`/`split_block` fragments MUST NOT contain container children (a nested list/quote inside a fragment item is `PM006`); the target's existing container children are preserved and re-attach to the replacement block if it is still a `list_item`, and are `PM006` if the replacement changes kind away from `list_item` (nowhere defined to attach) or if the target's own content is non-contiguous (own-content lines straddling a nested container — replacement placement would be ambiguous); `merge_blocks` where any non-surviving member owns container children is `PM006`. RFC-005 may relax all three.

Mechanics (splicing, decoration, separator synthesis, the PM008 post-splice assertion, v1 structural limits) are RFC-003 D6; identity outcomes are decided here:

| Operation | Identity outcome |
|---|---|
| `insert_blocks(path, fragment, before/after)` | Every parsed block **mints** a new ID (nested items included). |
| `update_block(block_id, fragment)` | ID **kept** — the caller is asserting "same block, new content." Fragment arity exactly 1 (`PM004`); the fragment's parentless block **replaces the target's subtree wholesale**: its kind MAY differ from the old kind (the assertion is the caller's; the hash changes regardless), and for `list_item`→`list_item` the fragment's marker and task-checkbox state replace the target's decoration, restyled to the destination list (this is how an agent checks a checkbox: `update_block(item, '- [x] done')`). Fragment descendants follow **subtree carry**; the target's container children follow the **container-children constraints** (preserved and re-attached, or `PM006`). Targeting a nested block directly (e.g. the item's inner paragraph) is the simple path and keeps *that* ID by this same rule; the immediately enclosing item keeps its ID with a **recomputed hash** (RFC-002 D7: an item's content includes its direct non-container children), while higher ancestors — which see the change only through a container — keep both ID and hash. |
| `move_block(block_id, before/after)` | ID **kept**; content and hash unchanged; `ord` (and `heading_path`, if the move crosses section boundaries) recomputed. Moves never change identity — a move is *detectable* by unchanged hash, but it is *asserted* by this op, not inferred. Descendant items move with their parent, IDs kept. |
| `split_block(block_id, fragment)` | Fragment arity ≥ 2 (`PM004`); for an item target the fragment must parse as a single list of ≥ 2 items, re-marked to the destination list's style. The **first (document order) keeps** the ID (its descendants follow subtree carry); subsequent parentless blocks and all their descendants mint. Container-children constraints apply. Provenance recorded (A4). Rationale: the first fragment inherits the block's position, so citations to the old ID keep pointing at where the content begins — the least-surprising binding, and XyDiff's split convention. |
| `merge_blocks(block_ids[], fragment, keep => uuid DEFAULT NULL)` | `block_ids` must be ≥ 2 **contiguous siblings** in document order (`PM006` otherwise); fragment arity exactly 1 (`PM004`) — the caller supplies the merged text (pgmind never guesses joiners). The surviving ID is `keep` if given (must be in the set — `PM003`), else the **first in document order**; the others **retire** (rows deleted in Phase 2; tombstoned history from Phase 3), and retirees' descendant blocks retire with them — explicitly, listed in A4's `removed`, never as implicit cascade side effects; a non-surviving member owning container children is `PM006` (v1). Provenance recorded (A4). |
| `write(path, doc)` on a new path | Every block mints. Identical content elsewhere in the vault is irrelevant — no cross-note carry, ever. |
| `write(path, doc)` on an existing note | The **deterministic carry** (A3). |

"Copy" has no operation because copying is just writing content that happens to exist elsewhere: the copy mints a fresh ID and shares the hash — the gate pins this (two identical paragraphs: one hash, two IDs, both stable across a no-op rewrite).

### A3. Whole-document write: the deterministic carry

When `write()` replaces an existing note, old blocks and newly parsed blocks are matched in three passes. Everything here is exact — no similarity, no thresholds; that is Part B's territory and Phase 3's risk.

- **Pass 1 — `^id` claims.** A new block whose `block_ref_id` equals the `block_ref_id` of a not-yet-matched old block carries that block's ID. This is the serialized-identity escape hatch (plan §11) and is *user assertion carried in the text*, not parser-derived identity: the marker was written by whoever wants the binding, pgmind merely honors it deterministically. Collisions resolve by document order on both sides: among new claimants of one ref, the lowest-`ord` new block wins (the rest fall through to pass 2); among duplicate old holders (dirty imports exist), the lowest-`ord` old block is the claimable one, and remaining duplicates behave as unmarked. A claim carries across a kind change (same assertion strength as `update_block`).
- **Pass 2 — exact content match, section-first.** Remaining new blocks match remaining old blocks with equal `content_hash` (kind is inside the hash, RFC-002 D7). Equal-hash sets pair **by document order**: k-th unmatched new occurrence ↔ k-th unmatched old occurrence. This carries every untouched block and every *moved* block, positionally stably for duplicates.

  The pass runs in **two tiers**, each applying the k-th↔k-th rule to what the previous tier left unmatched:

  - **Tier 2a — same section.** Candidates must agree on `content_hash` *and* `heading_path`.
  - **Tier 2b — any section.** Candidates must agree on `content_hash` alone.

  *Amended 2026-08-05 (post-Phase-2 review).* The single-tier form was unsound because `content_hash` covers only `(kind, normalized_content)` — `heading_path` is deliberately **not** in the hash (it is a positional fact, see below). So two byte-identical paragraphs in different sections hashed equal and were interchangeable. Deleting one section then handed its paragraph's ID to the identical paragraph in a *surviving* section while deleting the survivor's own ID: an ID silently changing which content it denotes, which A1 forbids outright ("an ID is never reused"). Tier 2a makes same-section carry win; tier 2b preserves the behaviour that motivated the untiered rule — renaming a heading changes its descendants' `heading_path`, and those blocks must still carry. Tie-breaking within a tier is unchanged, so every previously-correct outcome is preserved; only cross-section theft is removed. Pinned by `section_delete_does_not_recycle_ids_across_sections` and `heading_rename_carries_section_blocks`.
- **Pass 3 — remainder.** Unmatched new blocks **mint**; unmatched old blocks are **removed** (rows deleted in Phase 2 — current-state storage per RFC-003 §4; from Phase 3, removal becomes a tombstoning revision op and Part B's stages 1-2 run *between* passes 2 and 3 to catch edited-in-place blocks).

Stated consequence, loudly: **in Phase 2, editing a paragraph and rewriting the whole note gave that paragraph a new ID** (its hash changed; no `^id` claimed it). This was deliberate — a deterministic core must not guess — and it is exactly why the block ops exist (an agent that means "update this block" should say so) and why `^id` markers exist (a human round-tripping through an external editor can pin identity in the text). The identity gate pinned it as a test that asserted minting, precisely so that Phase 3 would have to change it on purpose.

**Superseded 2026-08-06 for the `write()` path**: Part B now rebinds that paragraph, and the pinned test was consciously inverted (`edited_paragraph_rebinds_with_confidence`). Two things about A3 are unchanged and load-bearing. The passes above still run first and still decide everything they can decide — Part B only ever sees what they could not match, so no deterministic outcome was traded for a heuristic one. And the block ops never reach Part B at all: a caller who says "update this block" is obeyed, not second-guessed. The no-op case is stronger than ID preservation: byte-identical input short-circuits before the carry entirely (RFC-003 D6 step 2).

`heading_path`, `ord`, `tile_ord`, spans, **and `parent_block`** are **positional facts, recomputed on every write** for carried and minted blocks alike — position is never identity (Law 4), so a carried block whose section was renamed keeps its ID while its `heading_path` changes, and a block carried out of a removed item keeps its ID while its `parent_block` NULLs (RFC-003 D6's reconcile ordering makes this safe: re-parenting UPDATEs precede removal DELETEs).

### A4. Provenance until RFC-011

RFC-011 (Phase 3+) owns the real provenance model. Until it lands, `revision.meta` (jsonb) records identity events so nothing is silently lost:

```jsonc
{ "op": "write" | "insert" | "update" | "move" | "split" | "merge",
  "minted":   ["uuid", …],          // pass-3 / insert / split-tail mints
  "carried":  {"ref": n, "hash": n},// pass-1 / pass-2 counts (write op only)
  "removed":  ["uuid", …],          // pass-3 removals, merge retirees (incl. their
                                    // descendants), subtree-carry removals — the forensic trace
  "split":    {"from": "uuid", "into": ["uuid", …], "marker_to": "uuid"|null},   // split op only
  "merge":    {"into": "uuid", "from": ["uuid", …], "marker_to": "uuid"|null} }  // merge op only
                                    // marker_to: which surviving block carries the ^marker (A5)
```

Lists are capped at 200 entries, then truncated with `"truncated": true` plus counts — bulk imports must not bloat revision rows; the cap is honest about it. This meta is a stopgap contract: RFC-011 supersedes it and MUST define the migration of accumulated meta.

*Amended 2026-08-05 (post-Phase-2 review), clarifying three points the first implementation got wrong:*

- **`marker_to` is a `uuid` and it lives inside the `split`/`merge` object.** It names *which surviving block* carries the `^marker` (A5). The first implementation emitted the marker's **text label** and hoisted the key to the top level of `meta`, so the record could not answer the one question A5 says it exists to answer, and a consumer reading `meta->'split'->>'marker_to'` got NULL. Because identities are assigned by the carry, `marker_to` can only be resolved *after* passes 1-3 run — the op reports a block position and the commit step resolves it.
- **`marker_to` searches the surviving block's whole subtree.** RFC-002 D3 anchors a marker to a block's final content line, so for a `list_item` the marker rides on the item's *inner paragraph*. Scanning only the fragment roots reported `null` for markers that plainly survived.
- **`carried` is emitted for every op that runs a carry**, not only `write`. The five block ops run A3 scoped to a subtree (A2 "subtree carry"), and suppressing their counts would hide real identity events. The comment above is narrowed accordingly.

### A5. `block_ref_id` is a label, not identity

The `^id` marker resolves anchors (RFC-002 D8) and asserts carry claims (A3 pass 1) — it is never the block's identity. It is user-editable text: it can be duplicated, deleted, or moved to a different block, and each of those is honored as written (lowest-`ord` rules break ties). `pgmind.block.id` is the identity; `block_ref_id` is how text refers to it. Conflating them would hand identity minting to whoever types in an editor — the exact failure Law 4 exists to prevent.

The two therefore **diverge lawfully under split and merge**: on `split_block`, the ID goes to the *first* fragment (A2) while the `^marker` lands wherever the caller's fragment text puts it — typically the last fragment, since RFC-002 D3 anchors markers to a block's final content line; on `merge_blocks`, at most one marker survives, exactly as the caller's fragment writes it. Both facts are recorded in the op's A4 provenance (`split`/`merge` entries carry the marker's new holder when one exists) and pinned in the gate — an anchor (`#^x`) may thus resolve to a different block ID after a split, which is the marker doing its job as a *label*.

### A6. Typed errors (Phase 2 set)

SQLSTATE class **`PM`** (extended by RFC-005 with `PM01x` CAS errors):

| SQLSTATE | Name | Raised when |
|---|---|---|
| `PM001` | `pgmind_invalid_path` | note path fails grammar/normalization (also: malformed `pgmind.vault_id` GUC) |
| `PM002` | `pgmind_note_not_found` | path has no live note in the current vault |
| `PM003` | `pgmind_block_not_found` | block UUID absent, in another vault, or (`merge`) `keep` not in the set |
| `PM004` | `pgmind_fragment_arity` | fragment parses to the wrong number of addressable blocks for the op |
| `PM005` | `pgmind_invalid_anchor` | `before`/`after` both given, or anchor block not a valid v1 position (RFC-003 D6); *neither* given is not an error — `insert_blocks` appends after the last top-level block |
| `PM006` | `pgmind_container_constraint` | v1 structural limits: non-contiguous merge set; cross-container move; fragment carrying container children; kind change away from `list_item` while container children exist; non-contiguous own content; merge retiree owning container children (A2's container-children constraints) |
| `PM007` | `pgmind_section_not_found` | `read_section` heading path matches nothing |
| `PM008` | `pgmind_splice_restructures` | a splice's full-note re-parse alters blocks outside the fragment or dissolves the target — unclosed fences/HTML swallowing following tiles, backward absorption (RFC-003 D6); the op aborts rather than silently retiring neighbor IDs |

Every error's DETAIL carries the offending value (path, uuid, count); agents repair by re-reading, never by forcing (plan §7 error contract).

---

## Part B — Heuristic rebinding (REVISED 2026-08-06 from corpus measurement — proposed for acceptance)

*This part was a structured draft with two invented thresholds. The adversarial edit corpus now exists (`eval/corpora/pgmind/rebinding/`, 42 cases, committed), the deterministic baseline is published, and the drafted pipeline has been simulated over the corpus. **Everything below that differs from the draft differs because a measurement said so**, and the measurements are in `eval/published/rebinding-baseline-v1.json` and `rebinding-tuning-v1.json`. Nothing here is normative until the owner accepts it; no rebinding code exists yet, which is deliberate — the precedence law is accepted-RFC-before-code.*

### B1. What the deterministic engine actually does (measured, published)

| | expected bindings | carried | |
|---|---|---|---|
| **identical** — same content, same kind | 73 | 72 | pass 2's job |
| **marked** — content differs, same `^id` on both sides | 5 | 5 | pass 1's job |
| **inferred** — content differs, no marker | **22** | **0** | nothing deterministic can do it |

Aggregate recall reads 0.770 and precision 0.987 — but the aggregate is mostly bookkeeping, because most blocks in a realistic edit are untouched. Split three ways, the number that matters is **0 of 22**. That is not a defect: A3 says in as many words that editing a paragraph and rewriting the note mints a new ID. Part B exists to move exactly that number and nothing else.

The single missed *identical* binding and the one mis-binding are the same case — two byte-identical paragraphs where the first is edited. `k`-th ↔ `k`-th pairs the survivor with the wrong original. No content-based rule can do better; it is recorded rather than fixed.

### B2. Five findings the corpus forced

1. **Stage 1 as drafted breaks A2's first-keeps convention.** A split fragment is *similar to its parent*, so a plain similarity aligner binds the parent to whichever fragment scores highest — which is routinely not the first. Four of the five mis-bindings at the drafted τ=0.5 are this one bug.
2. **Reversing the stages does not fix it.** Running split/merge detection before the aligner — the obvious repair — measures *worse* on both axes (inferred recall 0.591, precision 0.918, six mis-bindings): it repairs one case and fires on the decoy. Recorded because it is the repair a reader will propose.
3. **The drafted containment rule can never fire on lists.** "≥ 2 new neighbours" reads as adjacency, and a list item is a `list_item` block *plus the paragraph inside it*, so two sibling items' same-kind fragments are never adjacent — by ord or in the residual. Split runs must be taken over **same-kind peers**, not neighbours.
4. **Bigrams alone are unusable at the short end.** A one-word fragment has no bigrams, so it shares nothing with anything: `split-list-item`, where a fragment is the single word "beta", is unsolvable by construction rather than by threshold. The feature set must be unigrams ∪ bigrams.
5. **Order-monotonicity within the residual is not monotonicity.** The deterministic passes have already bound blocks the residual cannot see, so a locally monotonic stage 1 still yields a globally crossing result — observed as (1→2) and (2→1) in the same note.

### B3. The revised pipeline (normative once accepted)

When a whole document arrives from *outside* the block ops — sync, re-import, bulk `write` — A3 passes 1-2 run first and are already deterministic. Between them and pass 3:

- **Stage 1 — alignment.** Unmatched old/new blocks aligned by similarity: **Dice over the unigram ∪ bigram multiset** of normalized content, same-kind only, score ≥ **τ = 0.5**; the alignment is a maximum-weight order-monotonic matching over the residual (finding 5's global constraint is **not** adopted — see B5). Carried ID, `confidence = score`.
- **Stage 1b — split detection, inside the aligner, not after it.** Before scoring an old block's candidates, test whether it was split: a run of **2-4 consecutive same-kind peers in the residual** qualifies when containment holds **in both directions** — the run covers the old block, *and every fragment in the run is itself made of it*, both at ≥ **τ_split = 0.6**. If a run qualifies, the old block's only candidate is the run's **first** fragment (A2's convention, now enforced rather than hoped for) and it scores as the run's coverage. Both directions are required: a one-way test calls a moved-and-edited paragraph a split and hands its ID to an unrelated block, which is what `split/split-decoy-lead-in` exists to catch.
- ~~**Stage 2 — merges.**~~ **Dropped 2026-08-06, measured.** The draft gave merges their own containment stage. Stage 1 already carries every merge case in the corpus — including the two where dominance points *away* from document order (`merge-three-last-dominant`, `merge-tiny-prefix`) — because a merged block is overwhelmingly similar to its dominant source: "dominant" and "similar" are the same property here. Adding the drafted stage on top changes nothing on any axis (0.818 / 0.950 / 0.969 / 2 either way). A stage that provably never fires is not a safeguard, it is unexecuted code that will rot; the merge convention (dominant source keeps) survives as A2 semantics, enforced by stage 1 selecting the highest-scoring old block.
- **Stage 2** (was stage 3) = A3 pass 3, with removals becoming tombstone revisions and every inferred binding written with `bind='rebind'`, its confidence stored in RFC-005's `block_revision.confidence`, and provenance per RFC-011 — inferred identity must be *visibly* inferred (blame, citations, and the embedding queue all read confidence).

**The budget (declared 2026-08-06 — the revision made this a precondition for code).** Stage 1's matching is O(n·m) in the residual sizes, on the write path, inside the transaction. It is capped at **`n · m ≤ 40 000` cells**; over the cap the whole heuristic is skipped and the write falls through to pass 3 unchanged — every unmatched block mints, exactly as it does today. The fallback is recorded in provenance (`"rebind": {"skipped": "budget", …}`) so a note that silently churned identity can be found later rather than guessed at. Two properties make this the right shape rather than a cop-out: the residual is *unmatched* blocks only, so ordinary edits are nowhere near it; and the case that does blow the cap — a large document rewritten wholesale — is the case where the right answer is to carry nothing anyway (`rewrite/rewrite-total`). Feature vectors are computed once per block, never per pair.

### B4. Thresholds, and one that is not a threshold

**τ = 0.5.** Measured, not guessed: recall on the inferred class is flat for every τ ≤ 0.5 and falls above it, so 0.5 is the highest threshold that reaches maximum recall. The draft's guess survives contact with the corpus — which is worth saying plainly, because it was a guess.

**τ_split = 0.6, and it does not discriminate.** Swept 0.3 → 0.9, every value gives an identical result. What decides a split is the *bidirectional containment test*, not where its threshold sits. It is retained as a named constant with a stated default rather than advertised as a tunable the corpus cannot constrain.

### B5. What the revision refuses, and what it costs

Extending order-monotonicity to the deterministic bindings (finding 5) buys precision **0.969 → 0.989** and costs inferred recall **0.818 → 0.636**. It is rejected: every binding it removes is a *move*. `knowledge.move` is first-class, section reordering is routine, and a rebinder that cannot follow a block across a document is refusing the case Part B is most needed for. The crossing pair it would have prevented is in the undecidable duplicate case, where the alternative binding is not better.

Recommended operating point, and what it costs against today's engine:

| | inferred recall | overall recall | precision | mis-bindings |
|---|---|---|---|---|
| deterministic only (today) | 0.000 | 0.770 | 0.987 | 1 |
| drafted pipeline, τ=0.5 | 0.682 | 0.920 | 0.939 | 5 |
| **revised, τ=0.5, τ_split=0.6** | **0.818** | **0.950** | **0.969** | **2** |
| revised + global monotonicity | 0.682 | 0.920 | 0.979 | 1 |

18 of 22 inferred bindings, for 1.8 points of precision. Both remaining mis-bindings are the undecidable duplicate case — the pipeline introduces **no new mis-binding of its own**.

Four inferred bindings stay unsolved, and their shapes are the honest statement of what this design cannot do: two are **short blocks** (`## Concurrency` → `## Concurrency and locking`; `Locks first.` → `Locks always.`) where two or three tokens carry all the signal; one is a **crossing match** (two blocks swapped *and* edited), which an order-monotonic aligner can never make both halves of; one is the **duplicate**. Nothing in this design will fix them, and a later attempt should say which of the four it is buying.

### B6. Open questions this closes, and what remains

*Closed:* τ and τ_split (B4). Whether stage 1 should consider `heading_path` locality — **no**, and not because it wouldn't help: A3 pass 2 is already section-first, so locality discriminates on the deterministic path *above* stage 1, and the corpus case that turns on it (`dup-boilerplate-section-removed`) is resolved before stage 1 ever sees it. Move-then-edit — resolved as a deliberate trade in B5.

*Closed at implementation:* the O(n·m) budget — declared in B3 as `n · m ≤ 40 000` with a recorded fallback to pass 3.

*Still open:* whether sync (RFC-006) may pass per-file hints; whether `confidence` should suppress re-embedding below some floor (RFC-009's question, not this one's); whether the cap should be a GUC rather than a constant — deliberately **not** one today, because a tunable nobody has a number for is a support burden, and the corpus gives no basis for a second value.

---

## 3. Alternatives considered

- **Parser-derived identity** (position, slugs, content anchors) — rejected: audit C1's central finding; no system on earth recovers identity from plain text deterministically, and pretending otherwise poisons every downstream feature (Law 4).
- **Content hash as identity** — rejected: copies collide (two identical paragraphs are two blocks), edits orphan (any change is a "new" block); hashes answer *changed?*, IDs answer *same?* (Law 5).
- **`^id` markers as primary identity** — rejected: optional, user-editable, duplicable text cannot be a primary key; as *claims* (A3 pass 1) they give exactly the deterministic escape hatch the plan wants without surrendering minting.
- **Heuristic matching in Phase 2** — rejected: thresholds without the adversarial corpus to tune and gate them would be vibes with a version number; Phase 2 stays deterministic, Phase 3 earns the heuristics against a published match-rate.
- **UUIDv4** — rejected: random PKs fragment a large append-heavy index; v7 is the same 128 bits with btree locality. **DB-side `uuidv7()`** — rejected: PG18-only; the extension mints identically on PG16/17/18.
- **Last-fragment-keeps / caller-picks on split** — rejected in favor of first-keeps: deterministic, position-preserving for existing citations, and consistent with Part B stage 2's convention so behavior doesn't flip between op and rebind paths.
- **Synthesized merge joiners** (auto-concatenate with `" "` or `"\n\n"`) — rejected: `"\n\n"` yields two blocks (not a merge) and any synthesized joiner is a formatting guess; the caller supplies the merged text.
- **Cross-note identity carry** — rejected: turns every write into a vault-wide match problem and makes note moves ambiguous with copies; note moves get first-class treatment (`knowledge.move`, Phase 3) instead.

## 4. Consequences

*Easier:* citations, blame, and embedding reuse all inherit one crisp invariant (A1); agents get loud, typed identity semantics per op; Phase 3 slots its heuristics into a pre-built pipeline seam (between A3 passes 2 and 3) without changing any accepted behavior.
*Harder:* Phase 2 whole-document rewrites churn IDs for edited blocks (mitigations: block ops, `^id`, idempotence short-circuit; resolved properly by Part B) — this is the sharpest user-visible edge of the phase and MUST be documented at the API; provenance-via-`meta` is a stopgap RFC-011 must migrate.
*Impossible until amended:* changing the carry pass order, the first-keeps convention, or ID opacity — all three are load-bearing for citations and Part B compatibility; an amendment must re-run the identity gate and address stored provenance.

## 5. Benchmark gate

**Part A (Phase 2 — shared with RFC-003 §5): `identity-semantics`** — a golden scenario suite executed against a live extension (pg_test-driven, results published by the harness), 100% required:

- per-op outcomes for all six operations of A2, including: update across kind change; item update with unchanged descendants (hash-carried IDs) and with an edited inner paragraph (inner mints — the pinned subtree-carry behavior); update targeting the inner paragraph directly (its ID kept, enclosing item's ID kept with recomputed hash, higher ancestors untouched); checkbox toggle via markerful item fragment; move across section boundaries (ID kept, `heading_path` changed); **move/insert separator synthesis** (move to last position, move of the last block earlier, insert after the last block — no adjacent-block merging); split first-keeps and **split-with-`^id`** (ID→first, marker→where written, provenance records both); merge with and without `keep`, merge-with-two-markers, merge retiree with descendants (`PM006`); **child carried while parent removed** (`- hello` rewritten to `hello`: paragraph ID carried, `parent_block` NULLed, item row removed); every A6 error case including `PM008` via unclosed fence (` ``` ` and ` ```rust `), unclosed type-1 HTML block, and backward absorption (setext underline / lazy continuation);
- carry cases for A3: untouched note (byte-identical short-circuit — IDs *and* `xmin` stable); pure reorder (pass 2 carries all, hashes unchanged); duplicate-content pairing (k-th ↔ k-th); `^id` claim beating hash match; `^id` collision (both sides); claim across kind change; edited-paragraph-mints (the documented Phase 2 behavior, pinned so Phase 3 must consciously change it); copy semantics (one hash, two IDs);
- provenance: `revision.meta` matches the A4 schema for each scenario, including the 200-entry truncation and the split/merge marker-holder records.

**Part B (Phase 3): `rebinding-edit-corpus`** — the adversarial edit corpus at `eval/corpora/pgmind/rebinding/` (42 cases, eight categories, committed and hand-authored) with the **published match-rate** per category. Delivered 2026-08-06: `eval/published/rebinding-baseline-v1.json` (the engine as it stands) and `rebinding-tuning-v1.json` (the threshold study behind B3-B5).

The suite deliberately **does not gate on a score.** The published number is the deliverable; asserting a floor on it would be claiming an acceptance the owner has not given, and the thresholds are supposed to come *from* this measurement. It fails on three things instead, each with a negative control in `gate-selftest`: a corpus that does not parse or whose ground truth is not 1:1; a stale case index (ground truth read against a document that no longer parses that way); and any regression in the **control** category, whose cases are deterministic by construction and whose failure is a Part A regression wearing a Part B costume.

**Amended 2026-08-06 when Part B shipped.** The gate acquires a fourth assertion, and it is *not* the precision floor this section originally proposed — that floor contradicted B5, which accepts trading 1.8 points of precision for 18 of 22 inferred bindings, and a ratio gate also falls whenever someone contributes a hard case, which quietly asks the corpus to stop growing. The assertion is categorical instead: **no mis-binding may land in a case not marked `ambiguous`.** Missing a new case costs recall and passes; carrying identity onto the *wrong* block fails until it is fixed or the case is marked ambiguous with a written reason. That is the invariant the project actually holds — churned identity is annoying, misplaced identity is corrosive — and it is the one a growing corpus can keep enforcing.

Measured at acceptance (`eval/published/rebinding-v2.json`): recall 0.951, precision 0.980, inferred recall 0.826 (19 of 23), 38 of 42 cases perfect, and the only two mis-bindings both inside `near-duplicate/dup-edit-first-of-two`, which is marked ambiguous because no content-based rule can resolve it.

## 6. Law compliance

- **Law 4:** identity is minted by write operations and carried only by explicit assertion (op semantics, `^id` claims) or exact equality — never inferred in Part A; Part B's inference is quarantined behind Phase 3 acceptance, confidence-scored, and provenance-marked.
- **Law 5:** every rule keeps ID ≠ hash: copies share hashes across distinct IDs; updates keep IDs across changed hashes; pass 2 uses hashes to *find* sameness but the ID remains the identity.
- **Law 2/1:** everything here is pure computation inside the transaction; no I/O, no models.
- **Law 7:** the carry is a per-note set operation over indexed columns (`block_note_hash`, `block_note_ref`); no vault-wide work ever.
- **Law 8:** every identity event lands in an append-only revision row (A4); Phase 2's hard removal of block rows is the current-state deferral RFC-003 declares in its title — this RFC adds no removal path RFC-003 hasn't declared, and the A4 `removed` lists (capped, with counts) are the interim forensic record until RFC-005's history engine makes removal recoverable.
No law is violated beyond RFC-003's declared Law-8 deferral, which this RFC inherits.
