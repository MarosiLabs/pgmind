# RFC-004: Block Identity & Rebinding Semantics

- **Status:** Living — **Part A (write-path identity) accepted 2026-08-05 for Phase 2; Part B (heuristic rebinding) remains draft until Phase 3 acceptance** (per plan §16: "accepted (living)" split). Amended same day after adversarial review: fragment arity (parentless counting), subtree carry, op return contract, container-children constraints, PM008, marker/ID divergence. Amended again after Phase-2 code review: A3 pass 2 is section-first (two tiers), A4 `marker_to` is a uuid inside the split/merge object and `carried` is emitted for every carrying op.
- **Phase:** 2-3
- **Owner:** project author
- **Created:** 2026-08-05 · **Accepted:** 2026-08-05 (Part A) · **Frozen:** —

## 1. Context

The audit's #1 finding (C1) is that stable block identity cannot come from a parser: every system that ships it — Notion, Yjs, ProseMirror — mints identity on the **write path**, and every attempt to recover identity from plain-text diffing is heuristic (GumTree/XyDiff literature). The handbook made this Law 4; the plan made it this RFC. Identity is what makes a block citable across edits (`path#^block @ revision`), what `blame` and `patch_block` address, and what keys the embedding hooks so users never re-embed unchanged content (Law 5).

This RFC has two parts with different maturity by design. **Part A** — what each write operation does to IDs, including the *deterministic* carry rules when a whole document is rewritten — is normative for Phase 2 and proposed for acceptance now. **Part B** — the heuristic rebinding pipeline for external whole-document replacement (sync, re-import) — is the project's #1 research problem; it stays a structured draft here and hardens against the adversarial edit corpus for Phase 3 acceptance. Splitting maturity this way keeps Phase 2 purely deterministic: nothing accepted today involves a threshold or a similarity score.

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

Stated consequence, loudly: **in Phase 2, editing a paragraph and rewriting the whole note gives that paragraph a new ID** (its hash changed; no `^id` claimed it). This is deliberate — a deterministic core must not guess — and it is exactly why the block ops exist (an agent that means "update this block" should say so) and why `^id` markers exist (a human round-tripping through an external editor can pin identity in the text). Phase 3's Part B narrows the gap heuristically, with confidence made visible. The no-op case is stronger than ID preservation: byte-identical input short-circuits before the carry entirely (RFC-003 D6 step 2).

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

## Part B — Heuristic rebinding (DRAFT — hardens for Phase 3, not proposed for acceptance now)

*Nothing in this part is normative yet. It is recorded so Part A's shapes (pass ordering, provenance fields, confidence plumbing) are demonstrably forward-compatible with it, and so the Phase 3 RFC work starts from structure instead of a blank page.*

When a whole document arrives from *outside* the block ops — sync, re-import, bulk `write` — passes 1-2 (A3) run first and are already deterministic. Between them and pass 3, Phase 3 inserts:

- **Stage 1 — modified-in-place.** Unmatched old/new blocks aligned by position and similarity: token-bigram Dice over normalized content, same-kind only, threshold **τ (draft: 0.5, tuned on the corpus before acceptance)**; alignment must be order-monotonic (no crossing matches). Carried ID, `confidence = score`.
- **Stage 2 — splits & merges.** Containment heuristics over stage-1 leftovers: one old block whose content largely contains ≥ 2 new neighbors ⇒ split (first fragment carries, A2's convention); the mirror image ⇒ merge (dominant source carries). Draft containment metric: bigram overlap ≥ τ_split against the concatenation.
- **Stage 3** = pass 3, with removals becoming tombstone revisions and every stage-1/2 binding written with `source='rebind'`, its confidence stored per RFC-005's `block_revision`, and provenance per RFC-011 — inferred identity must be *visibly* inferred (blame, citations, and the embedding queue all read confidence).

Open questions Phase 3 must close before Part B acceptance: τ and τ_split values from corpus tuning; whether stage 1 considers `heading_path` locality; move-then-edit (hash gone *and* position gone); interaction with `pgmind.max_document_bytes`-scale documents (O(n·m) alignment needs a budget); whether sync (RFC-006) may pass per-file hints. The **adversarial edit corpus** (splits, merges, move+edit, near-duplicates, full rewrites) is the Phase 3 gate: the match-rate is published, honest, and tracked — the number *is* the deliverable (plan §16).

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

**Part B (Phase 3, defined now, gated then):** the adversarial edit corpus in `eval/` with **published match-rate** per category (split/merge/move+edit/rewrite/near-duplicate) — publish the honest number; acceptance thresholds are set by the Phase 3 revision of this RFC after corpus tuning, not invented today.

## 6. Law compliance

- **Law 4:** identity is minted by write operations and carried only by explicit assertion (op semantics, `^id` claims) or exact equality — never inferred in Part A; Part B's inference is quarantined behind Phase 3 acceptance, confidence-scored, and provenance-marked.
- **Law 5:** every rule keeps ID ≠ hash: copies share hashes across distinct IDs; updates keep IDs across changed hashes; pass 2 uses hashes to *find* sameness but the ID remains the identity.
- **Law 2/1:** everything here is pure computation inside the transaction; no I/O, no models.
- **Law 7:** the carry is a per-note set operation over indexed columns (`block_note_hash`, `block_note_ref`); no vault-wide work ever.
- **Law 8:** every identity event lands in an append-only revision row (A4); Phase 2's hard removal of block rows is the current-state deferral RFC-003 declares in its title — this RFC adds no removal path RFC-003 hasn't declared, and the A4 `removed` lists (capped, with counts) are the interim forensic record until RFC-005's history engine makes removal recoverable.
No law is violated beyond RFC-003's declared Law-8 deferral, which this RFC inherits.
