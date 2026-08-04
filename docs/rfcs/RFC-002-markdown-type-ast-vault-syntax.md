# RFC-002: Markdown Type, AST & Vault Syntax

- **Status:** Draft — awaiting owner acceptance
- **Phase:** 1
- **Owner:** project author
- **Created:** 2026-08-05 · **Frozen:** —

## 1. Context

Phase 1 delivers the `markdown` boundary type: parse, validate, serialize, and structural access ([product plan](../PRODUCT-PLAN.md) §16). Everything above it inherits this RFC's decisions: the block taxonomy determines what can carry identity (RFC-004), be stored (RFC-003), be appended to (RFC-005), and be cited by `context()` (RFC-008). The content-hash normalization rules quietly determine rebinding quality — the audit's C1 analysis showed heuristic block matching is this project's #1 research problem, and the hash is its primary signal. Vault syntax (wiki-links, tags, block refs) is not CommonMark; Obsidian's dialect has no spec, so we define ours explicitly rather than chase bug-compatibility (RFC-001 D3).

Binding laws: Law 3 (markdown is a boundary, per-block rows are storage), Law 4 (parsing yields structure, hashes, positions — never identity), Law 2 (nothing here touches a network).

## 2. Decision

### D1. Parser configuration (normative)

comrak (pinned per RFC-001) with exactly these options: extensions `table`, `tasklist`, `strikethrough`, `autolink` **on**; `footnotes`, `superscript`, `description_lists`, smart punctuation **off**; `sourcepos` on. The spec anchor is **CommonMark 0.31.2 + those four GFM extensions**. Any option change is an amendment to this RFC.

### D2. Block taxonomy

A **block** is the addressable unit — the thing that can carry an ID, a hash, a citation, and a revision. The taxonomy:

| Kind | Addressable block? | Notes |
|---|---|---|
| `heading` | yes | levels 1-6; ATX and setext both accepted, preserved as written |
| `paragraph` | yes | |
| `list_item` | yes | **each item is a block** (Notion precedent); the list itself is a *container* carrying `ordered`, `start`, `tight` in attrs; nesting = nested containers |
| `code_block` | yes | fenced and indented; fence info string in attrs |
| `table` | yes | one block per table (row-level addressing would need a future RFC) |
| `thematic_break` | yes | |
| `html_block` | yes | opaque; never interpreted |
| `block_quote` | no — container | its children are blocks; quotes nest |
| `list` | no — container | see `list_item` |

Every block records: `kind`, document order `ord`, `heading_path` (the raw, trimmed text of enclosing headings, outermost first), source span, content hash, and kind-specific attrs.

### D3. Vault syntax (our deterministic pass over the comrak AST)

Recognized only in text positions — never inside code spans, code blocks, autolinks, or HTML:

- **Wiki-links:** `[[target]]`, `[[target|alias]]`, `[[target#Heading]]`, `[[target#^block-id]]`, alias combinable with anchors. Single-line only; `\[\[` escapes.
- **Transclusions:** `![[…]]` with the same target grammar.
- **Tags:** `#tag` where tag matches `[A-Za-z0-9_/-]+` containing at least one non-digit, preceded by start-of-text or whitespace. Stored as written; the tag index matches case-insensitively (documented, deterministic).
- **Block ID markers:** a block whose source ends with ` ^[A-Za-z0-9-]+` has that marker stripped into `attrs.block_ref_id` and re-emitted on serialization — the opt-in deterministic identity hook RFC-004 builds on.
- **Explicitly not special in v1:** Obsidian callouts (`> [!note]` parses as an ordinary blockquote), comments (`%%…%%`), inline queries. Adding any of these is an amendment.

Extraction of links/tags/refs is part of parsing output (feeding `pgmind.edge`/`pgmind.tag` in Phase 2) and MUST be incremental downstream (Law 7).

### D4. Frontmatter

A leading `---` … `---` YAML **mapping** becomes note-level `properties` (jsonb); it is not a block. Invalid YAML or a non-mapping is treated as ordinary CommonMark content (Obsidian-compatible, deterministic, import-friendly — never an error). Reserved keys: `tags` (string or list; merged into the tag index at note level), `pgmind-pin` (bool; consumed by RFC-008). YAML features beyond plain mappings/sequences/scalars (anchors, tags, multi-doc) → treated as invalid, i.e. content.

### D5. Source positions

Byte offsets `[start, end)` into the source, recorded per block at parse time. Positions are diagnostics and rebinding *inputs*, never identity (Law 4).

### D6. Round-trip guarantee (byte fidelity)

The `markdown` type preserves the original bytes. Each block's source slice includes its trailing blank-line trivia; frontmatter plus any leading trivia form the document *preamble*; serialization is exact concatenation: **parse ∘ serialize = identity for every input, byte for byte.** No normalization ever touches stored source. (This is what makes "any editor, any diff tool works unchanged" true, and it is property-tested in the gate.)

### D7. Content hash (the rebinding signal)

`content_hash = BLAKE3-256( kind_tag ‖ 0x00 ‖ normalized_content )` where normalization is deliberately minimal:

1. line endings CRLF/CR → LF;
2. Unicode NFC;
3. trailing newline runs stripped;
4. the block-ID marker (` ^id`), if present, stripped — adding an ID must not change content identity.

Nothing else. In particular trailing spaces are **kept** (two trailing spaces are a CommonMark hard break — semantic), and code-block interiors are untouched beyond line endings. Two blocks hash equal iff a reader would call them the same content of the same kind. Hashes are content equality for dedup, rebinding, and embedding reuse (Law 5) — bytes and hashes are allowed to differ.

### D8. Path grammar & link-target resolution

Note paths (enforced in storage by RFC-003; filename mapping is RFC-006's):

- UTF-8, **NFC-canonicalized on input** (macOS emits NFD); case-sensitive; `/`-separated segments; no leading/trailing `/`; segments non-empty, not `.`/`..`, no control characters, no `\`, no leading/trailing whitespace; ≤ 1024 bytes total; no `.md` suffix (that's a filename concern).
- Glob semantics for `notes()`: git-style — `*` within a segment, `**` across segments.

Wiki-link target resolution (deterministic, recomputed incrementally on note create/rename): (1) exact path match; (2) else, if the target has no `/`: unique last-segment (basename) match — multiple candidates ⇒ the edge stays dangling with `reason = ambiguous`; (3) else dangling (`dst_note` NULL, `dst_path` preserved — dangling links are first-class per the product plan §6). `#Heading` anchors resolve by exact trimmed-text match, first in document order; `#^id` by `block_ref_id`.

### D9. The `markdown` type surface (v1)

- Input: any UTF-8 text is structurally valid CommonMark, so validation = UTF-8 check + size limit — default 8 MiB per document via GUC `pgmind.max_document_bytes` (big documents belong split; capacity model, RFC-003).
- Storage: text-compatible varlena — the type is a boundary (Law 3); per-block decomposition into rows happens on write in Phase 2.
- Access functions (transient, over the type): blocks with kind/ord/heading_path/content/content_hash/sourcepos/attrs; extracted links, tags, properties, preamble.
- **No public HTML renderer in v1** — the brain product doesn't need one; CommonMark conformance is tested through an internal, test-only render path in the eval harness.

## 3. Alternatives considered

- **Whole list as one block** — rejected: kills the granularity that makes `append_to_section`, per-item citation, and per-item history useful; Notion's model (item = block) is the proven shape.
- **Normalized storage** (store cleaned-up markdown) — rejected: breaks the byte-fidelity promise (D6) that makes external editors and diff tools trustworthy; normalization belongs only inside the hash (D7).
- **Aggressive hash normalization** (strip all trailing whitespace, collapse runs) — rejected: trailing double-space is a hard break; over-normalization silently merges semantically different content, corrupting rebinding and dedup.
- **Slug/content-derived block anchors as identity** — rejected: identity is a write-path property (Law 4, audit C1); anchors here are resolution targets, never IDs.
- **Footnotes extension on** — deferred: not in the GFM core four; enabling later is a compatible amendment, disabling later would not be.
- **Obsidian bug-compatibility for vault syntax** — rejected in favor of an explicit spec (RFC-001 D3); we accept a documented, deterministic subset.

## 4. Consequences

*Easier:* everything downstream keys on a small, fixed taxonomy; byte fidelity makes the sync bridge's job tractable; the minimal hash normalization gives rebinding a high-precision signal.
*Harder:* table interiors are opaque in v1 (no per-row addressing/citation); our vault-syntax pass is ours to maintain against Obsidian drift; setext headings and indented code survive as-written, so serializers must resist the urge to "clean up."
*Impossible until amended:* changing comrak options, the taxonomy, or hash normalization — any of these changes every stored hash, so an amendment MUST ship with a migration note (rehash-on-upgrade policy, RFC-012's upgrade path).

## 5. Benchmark gate

Phase 1 exits when, in CI (`eval/` suites, thresholds fixed at acceptance):

1. **CommonMark conformance:** with the vault-syntax pass disabled, parse+render matches the expected HTML for **all 652 examples** of CommonMark spec 0.31.2 (via the internal test renderer); GFM-extension interactions are covered by comrak's own suites.
2. **Round-trip:** parse∘serialize is byte-identical on the spec corpus, a real-vault corpus (≥ 100 documents), and a property-based fuzz suite (≥ 10⁵ generated documents) — zero exceptions.
3. **Hash stability:** golden-vector suite for D7 (CRLF, NFC, trailing-newline, `^id`-marker, hard-break preservation cases) passes exactly.
4. **Extraction correctness:** golden corpus for wiki-links/transclusions/tags/block-refs/frontmatter (including code-span non-matches and escape cases) passes exactly.

## 6. Law compliance

- **Law 2:** parsing, hashing, and extraction are pure functions of the input text; no I/O of any kind.
- **Law 3:** the type stores source bytes and exposes structure; it is explicitly *not* the storage model (D9).
- **Law 4:** outputs are structure, hashes, positions, and extracted references — the word "identity" appears in this RFC only to state it is out of scope (RFC-004).
- **Law 7:** extraction outputs are defined per-block so downstream index maintenance can be diff-driven.
- **Law 9:** one new type, plain functions, one GUC — Postgres-idiomatic surface.
No law is violated.
