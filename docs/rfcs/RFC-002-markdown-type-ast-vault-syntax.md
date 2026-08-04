# RFC-002: Markdown Type, AST & Vault Syntax

- **Status:** Living (phase active) — accepted by owner 2026-08-05; freezes at Phase 1 exit (four gate suites green)
- **Phase:** 1
- **Owner:** project author
- **Created:** 2026-08-05 · **Revised:** 2026-08-05 (post-verification: 8 blocking findings resolved) · **Frozen:** —

## 1. Context

Phase 1 delivers the `markdown` boundary type: parse, validate, serialize, and structural access ([product plan](../PRODUCT-PLAN.md) §16). Everything above it inherits this RFC's decisions: the block taxonomy determines what can carry identity (RFC-004), be stored (RFC-003), be appended to (RFC-005), and be cited by `context()` (RFC-008). The content-hash normalization rules quietly determine rebinding quality — the audit's C1 analysis showed heuristic block matching is this project's #1 research problem, and the hash is its primary signal. Vault syntax (wiki-links, tags, block refs) is not CommonMark; Obsidian's dialect has no spec, so we define ours explicitly rather than chase bug-compatibility (RFC-001 D3).

Binding laws: Law 3 (markdown is a boundary, per-block rows are storage), Law 4 (parsing yields structure, hashes, positions — never identity), Law 2 (nothing here touches a network).

## 2. Decision

### D1. Parser configuration (normative)

comrak (pinned per RFC-001), stated against comrak's actual `Options` structure:

- `extension.table`, `extension.tasklist`, `extension.strikethrough`: **on** — exactly the three-extension GFM subset decided in handbook §7 / RFC-001 D4.
- `extension.autolink`, `extension.footnotes`, `extension.superscript`, `extension.description_lists`: **off**. Autolink was considered and deferred: enabling it would silently expand the accepted RFC-001 D4 anchor and break CommonMark spec-example conformance (bare URLs must render as plain text in several §6.5 examples); a future amendment may add it. (GFM defines five extensions; the fifth, tagfilter, is a render-time HTML sanitization filter — irrelevant while v1 has no public renderer, D9.)
- `parse.smart`: **off** (no smart punctuation — determinism and byte fidelity).
- AST source positions are always available in comrak; `render.sourcepos` matters only to the internal test renderer.

Any option change is an amendment to this RFC and MUST address stored-hash migration (§4).

### D2. Block taxonomy

A **block** is the addressable unit — the thing that can carry an ID, a hash, a citation, and a revision.

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

Every block records: `kind`, document order `ord`, `heading_path`, source span (D5), content hash (D7), and kind-specific attrs.

**Heading text (normative, used everywhere a heading is named):** the heading's concatenated plain inline text — formatting markers dropped, wiki-links contributing alias-else-target text, code spans contributing their literal text, the `^id` marker stripped — NFC-normalized and whitespace-trimmed. `## *Important* [[decisions|Decisions]]` has heading text `Important Decisions`. This single definition serves `heading_path` elements, `#Heading` anchor matching (D8), and `append_to_section` targeting (RFC-005) — matching what a reader of rendered text would type.

**`heading_path`** is the heading text of enclosing headings, outermost first. It is **not guaranteed unique** (duplicate sibling headings are legal); consumers addressing sections by `heading_path` resolve to the first match in document order (mirroring D8's anchor rule), and RFC-005 MUST define append semantics under duplicates.

### D3. Vault syntax (our deterministic pass over the comrak AST)

**Precedence rule:** CommonMark inline parsing wins; vault syntax is recognized only in surviving text nodes. Consequently a link-reference definition elsewhere in the document (`[foo]: url`) makes `[[foo]]` parse as a bracketed reference link and **no wiki-link is recognized** — deterministic, and golden-cased in the gate. Vault syntax is never recognized inside code spans, code blocks, or HTML.

- **Wiki-links — closed grammar**, single line, inside `[[ … ]]`:
  `target` = characters up to the first unescaped `#`, `|`, or `]]`;
  `anchor` = after the first unescaped `#`, up to the first unescaped `|` or `]]` (later `#` characters are part of the anchor text);
  `alias` = everything after the first unescaped `|` (may itself contain `#`);
  escapes `\#`, `\|`, `\]` are valid in all three parts. Targets **and anchors** are whitespace-trimmed and NFC-normalized at extraction (anchors match against NFC heading text — D8). A target that violates the D8 path grammar still produces an edge — dangling, with `reason = 'invalid'`. An **empty target with no anchor** (`[[]]`, `[[ ]]`, `[[|alias]]`) is not a wiki-link: the bracket pair is consumed and no edge is produced (golden-cased).
- **Table-cell caveat (GFM interaction):** the table extension splits cells on unescaped `|` *before* inline parsing, so `[[a|b]]` unescaped inside a cell yields two cells and no link. Write `[[a\|b]]`: the `\|` survives cell splitting and then functions as an ordinary `|` in the wiki grammar — i.e. the alias separator (Obsidian-compatible). Cell text is scanned as GFM-reconstructed content, since cell source positions do not map back to raw bytes (empirical, comrak). Documented, deterministic, golden-cased.
- **Transclusions:** `![[…]]`, same grammar.
- **Markdown links** `[text](target)`: a target with a URL scheme (`https:`, `mailto:`, …) is external — no vault edge. A scheme-less target is a candidate vault path: an optional `#fragment` is split off first into the anchor (mirroring wiki-link anchors, NFC-normalized), then an optional `.md` suffix is stripped, then resolve per D8, producing an edge of kind `mdlink`. Pure-fragment links (`[t](#sec)`) are self-references — out of v1 scope, no edge. Autolinked bare URLs don't exist under D1.
- **Tags:** `#tag` where tag matches `[A-Za-z0-9_/-]+` containing at least one non-digit, recognized when preceded by the start of the block's inline content or by whitespace **in the original source** (checked via source positions across text-node boundaries — `**bold**#tag` is *not* a tag). No ATX collision exists: CommonMark requires a space/tab after the `#` run for a heading, so line-initial `#tag` is a paragraph. Stored as written; the tag index matches case-insensitively.
- **Block ID markers:** ` ^[A-Za-z0-9-]+` is recognized at the end of the block's **last content line** — after container-prefix stripping (D7), before trailing blank-line trivia; for setext headings, the content line is the one before the underline — for `heading`, `paragraph`, and `list_item` blocks. When the line is shared between nested eligible blocks (`- foo ^x`: paragraph inside item), the **innermost block carries `block_ref_id`**; enclosing items strip the marker from their hashes too (D7 step 4), so adding an ID never changes any block's identity. The marker is stripped into `attrs.block_ref_id` and re-emitted on serialization. **v1 limitation:** `code_block`, `table`, `html_block`, and `thematic_break` cannot carry a marker (their final line is content or fence/row syntax); the consequence — the two kinds where heuristic rebinding is weakest lack the deterministic `^id` escape hatch — is recorded for RFC-004, which may adopt Obsidian's following-line attachment rule as an amendment.
- **Explicitly not special in v1:** Obsidian callouts (`> [!note]` is an ordinary blockquote), comments (`%%…%%`), inline queries.

Extraction of links/tags/refs is part of parsing output (feeding `pgmind.edge`/`pgmind.tag` in Phase 2) and MUST be incremental downstream (Law 7).

### D4. Frontmatter

A leading `---` … `---` YAML **mapping** becomes note-level `properties` (jsonb); it is not a block. Invalid YAML or a non-mapping is treated as ordinary CommonMark content (Obsidian-compatible, deterministic, import-friendly — never an error). Reserved keys: `tags` (string or list; merged into the tag index at note level), `pgmind-pin` (bool; consumed by RFC-008). YAML features beyond plain mappings/sequences/scalars (anchors, tags, multi-doc) → treated as invalid, i.e. content.

### D5. Source positions

comrak emits 1-based line/column source positions (columns byte-counted). pgmind computes byte spans `[start, end)` from a **line-offset table** over the original bytes, deriving block spans from line boundaries; columns are advisory only (cmark-heritage column values are unreliable around tab expansion, so they must never determine a span). A nested block's span is a **sub-range of its enclosing container's span**. Spans are diagnostics and rebinding *inputs*, never identity (Law 4).

### D6. Round-trip guarantee (byte fidelity)

The `markdown` type preserves the original bytes. Serialization concatenates: the document *preamble* (frontmatter plus leading trivia), then the source spans of the **top-level document children only**, each span including its trailing blank-line trivia. Containers serialize as their single full span — covering their `>` markers and list indentation. Nested blocks' spans are addressable **views into their ancestor's span**, never independent serialization units (concatenating them would duplicate or mutilate bytes; a blockquote child's standalone slice contains interior `> ` prefixes and is not a faithful standalone reproduction — consumers that re-emit a nested block, like RFC-005's `patch_block`, work by re-serializing the containing top-level node). **parse ∘ serialize = identity for every input, byte for byte** — property-tested in the gate.

### D7. Content hash (the rebinding signal)

`content_hash = BLAKE3(kind_tag ‖ 0x00 ‖ normalized_content)` — BLAKE3's default 256-bit output, via the `blake3` crate, pinned in Cargo.toml.

**`normalized_content` (normative):** the block's *logical text* —

1. start from the block's source span;
2. strip enclosing container decoration exactly as CommonMark strips it: `>` prefixes on every line, list-item markers and continuation indentation (the item's marker style lives in attrs, not content; lazy-continuation lines, which have no prefix, are already logical text). A paragraph reads the same whether it sits inside a quote, a list, or at top level — and hashes the same;
3. for `list_item`: **exclude container children (nested lists, quotes) and their descendants** — the item's content is its direct non-container children (its paragraphs, code, tables). Nested items carry their own hashes; editing a leaf sub-item must not ripple "modified" through every ancestor item (that would blind RFC-004's exact-match stage and break Law-5 embedding reuse). The item's own marker — and, for task items, the checkbox — is decoration, stripped like container prefixes (step 2);
4. strip the block-ID marker (` ^id`) if present, together with surrounding trailing whitespace, searching content lines bottom-up (setext: the marker precedes the underline) — adding an ID must not change content identity, including for enclosing items sharing the marker line;
5. normalize line endings CRLF/CR → LF;
6. Unicode NFC — applied uniformly, **including code-block interiors** (a deliberate exception to "code is untouched": hash-only, never touches stored bytes; golden-cased);
7. strip trailing newline runs.

Nothing else. Trailing spaces are **kept** (two trailing spaces are a CommonMark hard break — semantic). Two blocks hash equal iff a reader would call them the same content of the same kind, regardless of container context or nesting depth. Hashes are content equality for dedup, rebinding, and embedding reuse (Law 5) — bytes and hashes are allowed to differ.

### D8. Path grammar & link-target resolution

Note paths (enforced in storage by RFC-003; filename mapping is RFC-006's):

- UTF-8, **NFC-canonicalized on input** (macOS emits NFD); case-sensitive; `/`-separated segments; no leading/trailing `/`; segments non-empty, not `.`/`..`, no control characters, no `\`, no leading/trailing whitespace; ≤ 1024 bytes total; no `.md` suffix (that's a filename concern).
- Glob semantics for `notes()`: git-style — `*` within a segment, `**` across segments.

Link-target resolution (deterministic, recomputed incrementally on note create/rename): the target is whitespace-trimmed and NFC-normalized first (so an NFD target typed on macOS exact-matches its NFC-stored note); then (1) exact path match; (2) else, if the target has no `/`: unique last-segment (basename) match — multiple candidates ⇒ dangling with `reason = 'ambiguous'`; (3) else dangling (`dst_note` NULL, `dst_path` preserved — dangling links are first-class per the product plan §6; grammar-violating targets carry `reason = 'invalid'`). `#Heading` anchors resolve by exact **heading-text** match (D2's definition — plain text, NFC, trimmed), first in document order; `#^id` by `block_ref_id`.

### D9. The `markdown` type surface (v1)

- Input: any UTF-8 text is structurally valid CommonMark, so validation = UTF-8 check + size limit — default 8 MiB per document via GUC `pgmind.max_document_bytes` (big documents belong split; capacity model, RFC-003).
- Storage: the source text held in a varlena (v1: pgrx's serialized representation; textual I/O is byte-faithful — the type is a boundary either way, Law 3, and the real storage model is RFC-003's per-block rows); per-block decomposition into rows happens on write in Phase 2.
- Access functions (transient, over the type): blocks with kind/ord/heading_path/content/content_hash/span/attrs — where `content` is the D7 *normalized logical content* (the hash input; raw bytes are recoverable via the span) — plus extracted links, tags, properties, preamble.
- **No public HTML renderer in v1** — this deliberately narrows the product plan's Phase 1 "renderer" deliverable to an *internal conformance renderer* used only by the eval harness (the brain product doesn't need HTML; the plan wording is amended alongside this RFC).

## 3. Alternatives considered

- **Whole list as one block** — rejected: kills the granularity that makes `append_to_section`, per-item citation, and per-item history useful; Notion's model (item = block) is the proven shape.
- **Quote as an addressable block** (the plan's §4 sketch listed "quote") — rejected in favor of quote-as-container: the quote's children remain individually addressable, citable, and patchable, which is strictly more useful than one opaque quote block; a whole quote consequently cannot carry its own ID or be a `patch_block` target (documented consequence; plan §4 amended alongside this RFC).
- **Autolink extension on** (this RFC's own first draft) — deferred: silently expanded the accepted RFC-001 D4 three-extension anchor and made the CommonMark conformance gate unachievable (§6.5 negative examples). Bare URLs stay plain text in v1.
- **Normalized storage** (store cleaned-up markdown) — rejected: breaks the byte-fidelity promise (D6) that makes external editors and diff tools trustworthy; normalization belongs only inside the hash (D7).
- **Aggressive hash normalization** (strip all trailing whitespace, collapse runs) — rejected: trailing double-space is a hard break; over-normalization silently merges semantically different content, corrupting rebinding and dedup.
- **Hashing raw source slices** (no container stripping, descendants included) — rejected: container context and nesting depth would leak into hashes, making identical content look different (quote vs top level) and every ancestor look modified on leaf edits — exactly the false signals rebinding cannot afford.
- **Slug/content-derived block anchors as identity** — rejected: identity is a write-path property (Law 4, audit C1); anchors here are resolution targets, never IDs.
- **Footnotes extension on** — deferred: outside the decided GFM subset; enabling later is a compatible amendment, disabling later would not be.
- **Obsidian bug-compatibility for vault syntax** — rejected in favor of an explicit spec (RFC-001 D3); we accept a documented, deterministic subset.

## 4. Consequences

*Easier:* everything downstream keys on a small, fixed taxonomy with one heading-text definition; byte fidelity makes the sync bridge's job tractable; container-invariant, descendant-exclusive hashes give rebinding a high-precision signal.
*Harder:* table interiors are opaque in v1 (no per-row addressing); `code_block`/`table` lack the `^id` escape hatch (recorded for RFC-004); our vault-syntax pass is ours to maintain against Obsidian drift; serializers must resist "cleaning up" setext headings and indented code.
*Impossible until amended:* changing comrak options, the taxonomy, heading-text definition, or hash normalization — any of these changes every stored hash or address, so an amendment MUST ship with a migration note (rehash-on-upgrade policy, RFC-012's upgrade path).

## 5. Benchmark gate

Phase 1 exits when these `eval/` suites pass in CI (suite IDs are normative; thresholds fixed at acceptance):

1. **`commonmark-conformance`** — in the *pure CommonMark configuration* (vault-syntax pass AND all D1 GFM extensions disabled), parse+render matches the expected HTML for **all 652 examples** of CommonMark spec 0.31.2 via the internal test renderer. Extension behavior is covered by comrak's own suites; extension *interaction* cases we depend on (table-cell pipes, tasklist items as list_items) are pinned in suite 4.
2. **`round-trip`** — parse∘serialize is byte-identical on the spec corpus, a real-vault corpus (≥ 100 documents, including blockquote/list nesting, tabs, setext headings, indented code), and a property-based fuzz suite (≥ 10⁵ generated documents) — zero exceptions.
3. **`hash-stability`** — golden vectors for D7, including: CRLF, NFC (including inside code blocks), trailing-newline runs, `^id`-marker stripping, hard-break preservation, quote-context invariance (`> foo` vs `foo`), lazy continuation, list-nesting depth invariance, and list_item descendant-exclusion (leaf edit leaves ancestor hashes unchanged).
4. **`vault-syntax-extraction`** — golden corpus for D3/D4: wiki-links (anchors, aliases, escapes, whitespace-trimming, NFD targets, empty-target non-links), the reference-definition collision case, table-cell `|` broken/escaped pairs, transclusions, tags (`**bold**#tag` non-match, line-initial `#tag` match), block-ID markers (per-kind including excluded kinds, container-caret non-markers, setext), `mdlink` internal/external/fragment split, frontmatter (valid, invalid-as-content, reserved keys). This suite seeds Phase 2's `extraction-correctness` corpus (plan §16).
5. **`parse-performance`** — pathological constructions (unclosed-wiki spam, tiny-paragraph floods, item floods, an 8 MiB single line) parse within fixed time bounds — the O(n²) regression guard.

## 6. Law compliance

- **Law 2:** parsing, hashing, and extraction are pure functions of the input text; no I/O of any kind.
- **Law 3:** the type stores source bytes and exposes structure; it is explicitly *not* the storage model (D9).
- **Law 4:** outputs are structure, hashes, positions, and extracted references — identity appears in this RFC only to state it is out of scope (RFC-004).
- **Law 7:** extraction outputs are defined per-block so downstream index maintenance can be diff-driven.
- **Law 9:** one new type, plain functions, one GUC — Postgres-idiomatic surface.
No law is violated.
