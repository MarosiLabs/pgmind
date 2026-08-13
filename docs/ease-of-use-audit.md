# Easier than a folder of files

*Audit and change plan, 2026-08-09. Written against a stated product scenario — multi-tenant
agents, per-user and per-agent vaults, instructions that never change and memories that
several agents write at once — after the report that "using pgmind is so hard… the SQL
commands are not easy, for some cases we even need to create psql functions."*

*Status: proposal. Nothing here is accepted. It is written to be argued with and then
folded into [PRODUCT-PLAN.md](PRODUCT-PLAN.md) as slices, or rejected.*

---

## 0. Method, and one thing that moved underneath it

Five external research streams (agent-memory products, MCP tool design, document and CMS
APIs, PostgreSQL technique, PostgreSQL multi-tenancy — 155 sources), five internal audit
streams reading this tree, one design synthesis, and three adversarial reviews on separate
lenses (security, ergonomics, feasibility). Several claims below were executed against a
live PostgreSQL 18.4; those are marked **measured**. Claims I re-verified by reading the
source in this session are marked with a file reference. Everything else is reported as
what it is.

Two corrections the reviews forced, recorded because they bear on how much weight the rest
carries:

- **The repository moved mid-audit.** Commits `51bf5b2`, `ec076e9` and `d037731` landed
  while the audit ran. Phase 4 is cut, RFC-006 is withdrawn, and `scripts/import-vault.sh`
  and `scripts/export-vault.sh` shipped. Every sequencing argument in the first synthesis
  referred to a phase that no longer exists; §11 is written against the tree at `d037731`.
- **One root cause in the first synthesis was argued from fabricated evidence** — it
  indicted an import loop that interpolated paths into SQL and fudged a trailing newline
  with `|| E'\n'`. `scripts/import-vault.sh` does neither. The two real points underneath
  it survive and are stated in §4.7 at their actual size. A synthesis that invents evidence
  once is a synthesis whose other claims need checking, which is what the third review was
  for.

---

## 1. The verdict

pgmind's semantics are the strongest in the surveyed field. Compare-and-swap writes,
section-scoped appends that serialize, block-level patching with `expected_hash`, block IDs
that survive a whole-document rewrite with a confidence score attached — nothing else
surveyed has all four, and most have none. That is not the problem.

**The container layer was never built.** A vault is not an object in this extension: it is
a `uuid` column with a default ([schema.rs:53](../extension/src/schema.rs#L53)) and a
session GUC ([lib.rs:78-85](../extension/src/lib.rs#L78-L85)). There is no `pgmind.vault`
table. A vault therefore has no name, no description, no owner, no creation event, no
existence test, and no way to be listed or dropped. It springs into being the first time
somebody writes a row stamped with a uuid they invented, and a typo produces a new empty
vault indistinguishable from an empty one.

Everything the scenario asks for starts one layer above where the API stops. So every
deployment builds that layer itself — a vault registry, a description convention, a search
index, a tenancy guard — and three of those four hand-built layers are actively undermined
by what is underneath them. That is the whole of "we even need to create psql functions."

The second half of the verdict is harder: **the mechanism that selects a vault is also the
only thing defending one.** `pgmind.vault_id` is `GucContext::Userset`, and the shipped
policy is `USING (vault_id = current_setting('pgmind.vault_id', true)::uuid)`
([schema.rs:343-345](../extension/src/schema.rs#L343-L345)). The predicate is the value the
caller controls. So the requirement "an agent must not reach another tenant's vault even if
it supplies a real id belonging to someone else" is not partially met — the supported,
documented way to switch tenants *is* the attack. The schema's own comment says this
plainly ("it SCOPES a session to a vault, it does not defend one"); the website manual says
the opposite.

---

## 2. The root cause, and why it is one cause

RFC-003 D1, frozen, in two sentences:

> There is no vault registry table in v1: a vault is a namespace value, nothing more.
>
> Function signatures stay path-only; multi-tenant callers `SET pgmind.vault_id` per
> session/transaction.

Every friction below is a consequence of one or the other.

**From "a namespace value, nothing more":** no name, no listing, no description, no
lifecycle, no FK target, no existence check, no per-vault settings, no drop. `mkdir` beats
it, and `ls` has no counterpart at all.

**From "signatures stay path-only":** the vault cannot vary per row, so a cross-vault query
is not slow but *unexpressible* — and the natural formulation is silently **wrong** rather
than an error. `SELECT u.handle, n.path FROM app_user u, LATERAL knowledge.notes() n`
attributes one tenant's notes to every tenant (reproduced on PG 18.4 here and previously in
[application-integration-seam.md §6](application-integration-seam.md)). It cannot be
composed into a join, so every application entry point becomes a plpgsql wrapper whose body
is `set_config()` followed by the call it actually wanted. And because `SET` is a utility
statement that takes no bind parameters, `SET LOCAL pgmind.vault_id = (SELECT …)` is a
syntax error — which is why [cookbook.html:2100](../website/docs/cookbook.html#L2100), the
only recipe showing an application selecting a tenant per request, cannot run as written:

```js
await client.query('SET LOCAL pgmind.vault_id = $1', [tenantId]);   // 42601
```

That D1's *Alternatives considered* lists ten rejected options and an explicit vault
parameter is not among them is the tell. It was not weighed and rejected. It was not
considered. D1's own text pre-authorizes the other half: *"Vault registry table —
deferred… a future RFC can add one compatibly (the column is the contract)."*

Five smaller causes sit around it, each independent of the tenancy question:

3. **The API produces identifiers it cannot consume.** `knowledge.notes()` returns no note
   id, and its `title` column is `n.basename` — the path's last segment
   ([read.rs:116-160](../extension/src/read.rs#L116-L160)), so `projects/auth` has "title"
   `auth` even when the frontmatter says otherwise. `pgmind.verify_note(note_id uuid)`
   takes an argument no public function returns.
4. **Nothing measures size.** Zero functions in either schema match `%token%`. The only
   number in the product is `knowledge.stats().bytes`, one vault-wide sum. Context
   budgeting has no unit.
5. **Discovery stops at exact tags and prefix globs.** No `tsvector`, no `pg_trgm`, no
   search function of any kind anywhere in the tree — verified at HEAD. Full-text and fuzzy
   search are entirely the caller's code.
6. **Two write-surface shapes are wrong at the two places an agent hits first** (§4.5).
7. **Bulk ingestion has no SQL primitive.** `knowledge.write` is one note per call.

---

## 3. The scenario, priced against today

The stated requirement is that each of these be *easier than a folder of markdown files*.

| # | Requirement | Today | Filesystem |
|---|---|---|---|
| 1 | Create a vault; it has a name **and** an id | **impossible** — no registry, no name, no create | `mkdir` |
| 2 | Add a document | 2 statements (`SET`, then `write`) — the `SET` cannot take a subquery | `>` |
| 3 | A guessed real id from another tenant must fail | **fails open** — one `SET` reaches any vault, RLS on or off | dir perms |
| 4 | List documents: path/id, name, description | 1-of-4 — path only; `title` is the filename; no id; no description | `ls` |
| 5 | Metadata, TOC, section/block/table content | TOC is a hand-written flat scan the caller indents; tables have no addressable identity | `head`, headings |
| 6 | Size of a block / section / document, in tokens | **impossible** — no token or byte accounting at any granularity | `du`, `wc` |
| 7 | Remove, edit, concurrently edit, append, add | strong semantics, two sharp shapes (§4.5, §4.6) | worse than pgmind |
| 8 | Tag search across a vault | works: `knowledge.tagged(tag)` | `grep` |
| 9 | Tag search within one document | no `path` argument; scans the vault's tags | `grep` |
| 10 | Fuzzy search, in a document and across a vault | **impossible** — nothing ships | `grep` |
| 11 | Migrate local .md files | `scripts/import-vault.sh` (gated, byte-exact) — but ~21 files/s, one psql fork per file | `cp` |

Four requirements have no answer at all. One is unsafe. Requirement 7 — the concurrent
memory append — is the one place pgmind beats the filesystem outright, and it is gated
behind the bug in §4.6.

The manual is honest evidence of the same gap. The cookbook is 73 recipes across 93 code
blocks; roughly a third reproduce something a filesystem gives free, 14 are the same
`knowledge.notes() CROSS JOIN LATERAL knowledge.blocks(n.path)` fan-out because there is no
vault-wide relation of anything, 4 require `CREATE FUNCTION`, and 11 reach below the public
API into a schema with no stability promise. A recipe everyone copies verbatim is a missing
feature, not documentation.

---

## 4. Defects found on the way

These are independent of everything proposed later. They are bugs in shipped code, they
have no dependency on the registry or the tenancy redesign, and several are silent-
corruption class.

> **§4.1, §4.2 and §4.6 are fixed** (2026-08-10). 85 extension tests and all 21 eval
> suites green, including the manual gate, which caught the two cookbook recipes that
> documented the behaviour these fixes changed. Each fix carries a regression test, and
> the excision test was negative-controlled — with the vault predicate removed it fails.
> §4.3, §4.4, §4.5 and §4.7 remain open.

### 4.1 `pgmind.excise()` erases across every vault — blocker · **fixed**

[`redact()`](../extension/src/excision.rs#L415-L452) builds its statements from
`text_columns()`, which enumerates every text-bearing column in schema `pgmind`, and issues
them with **no vault predicate**:

```sql
UPDATE pgmind.<table> SET <col> = replace(<col>, $1, $2) WHERE position($1 in <col>) > 0
```

That enumeration includes the live lanes — `tile.raw`, `block.content`, `note.path`,
`note.preamble`. Meanwhile `live_hits()` *is* scoped
([excision.rs:367-379](../extension/src/excision.rs#L367-L379)), so the PM012 refusal check
asks about the calling vault while the destruction is global. A literal that is dead in
tenant A but live in tenant B passes the refusal and is then rewritten in B's live content,
bypassing the write path, recording no revision, leaving B failing `verify_note`. A routine
erasure request in one tenant silently corrupts every other tenant.

The post-check compounds it: [`sweep()`](../extension/src/excision.rs#L57-L81) is equally
unscoped, so it finds no survivors and the transaction commits with a clean attestation.

Two aggravating factors:

- `sweep()` runs on `dry_run`, which **defaults to true**, and reports its count in a
  WARNING. That makes `excise` a cross-tenant existence oracle for arbitrary strings, at no
  cost and with no audit row.
- The only `REVOKE`s in the entire tree are `raise_error`
  ([schema.rs:44](../extension/src/schema.rs#L44)) and `enable_vault_rls`
  ([schema.rs:349](../extension/src/schema.rs#L349)). `excise`, `retain`,
  `verify_excision`, `verify_note` and `verify_history` are executable by `PUBLIC`.
  [PRODUCT-PLAN §7.1](PRODUCT-PLAN.md) states `excise` is "an admin surface, revoked from
  `PUBLIC` (Law 11, RFC-005 §6)"; RFC-005 D7 mandates it; the code does not do it, and
  [MANUAL-PLAN.md:186](MANUAL-PLAN.md) already records the divergence.

Scoping the sweep is not a pure win and should not be sold as one: it changes
`verify_excision` from "proven erased" to "proven erased from this vault," and gives up the
ability to notice the same literal surviving elsewhere. That is probably the right trade for
a multi-tenant product, but it is a change to the strongest safety claim in the README and
deserves to be argued, not slipped in.

### 4.2 A composite return type applies every write twice — blocker · **fixed**

`pgmind.op_result` is `(revision uuid, block_ids uuid[])`
([schema.rs:49](../extension/src/schema.rs#L49)), returned by six mutating functions. These
are `VOLATILE`, so Postgres does not common-subexpression them, and
`SELECT (knowledge.append_to_section(…)).*` expands to one evaluation **per output column**:
two columns, two commits, one row of correct-looking output. **Measured** on PG 18.4.

In a store whose premise is concurrent agent memory, a silently doubled append is the worst
available failure mode. It is warned about on six documentation pages, demonstrated running
in the cookbook (3 blocks became 5), and nine recipes are contorted into a `FROM`-clause
idiom to avoid it. A documented warning is not a safeguard — especially against an LLM,
which has read a million examples of `SELECT (f(x)).*`.

Returning a scalar `uuid` plus a separate `knowledge.blocks_changed(revision)` removes it by
construction. Useful and non-obvious: `RETURNS TABLE` functions are already immune — an SRF
expression is evaluated once by `ProjectSet` (**measured**) — so the fix is about the
scalar/composite distinction, not about arity.

### 4.3 The RLS policy does not survive a pooled connection

After `SET LOCAL pgmind.vault_id = …; COMMIT`, a custom GUC holds `''`, not NULL, and never
returns to NULL (**measured**). The shipped policy casts `current_setting(…, true)::uuid`
with no `NULLIF`, so every subsequent statement on that connection raises `22P02`. The
pooler-safe idiom poisons the connection it was meant to protect.

### 4.4 `excision_replay` sits outside the boundary by construction

`enable_vault_rls` enumerates tables carrying `vault_id` from `pg_catalog` — deliberately,
and the reasoning in the comment is good. But `pgmind.excision_replay` has no `vault_id`
column, so the enumeration silently excludes the one table holding erased identifying data.
The function reports nothing about what it could not cover.

### 4.5 `read_section` and `append_to_section` disagree about what a heading path is

`read_section` matches ancestors **plus the heading's own text** and takes the **first**
match ([read.rs:76-87](../extension/src/read.rs#L76-L87)). `append_to_section` matches
ancestors **only** and takes the **last** (`.rev().find()`,
[ops.rs:795-807](../extension/src/ops.rs#L795-L807)). Two functions, one argument name, two
different meanings and two different tie-breaks. Duplicate headings resolve silently.

### 4.6 You cannot append to an empty section — high · **fixed**

`in_section` is `r.heading_path == heading_path`
([ops.rs:796](../extension/src/ops.rs#L796)). A heading's own stored `heading_path` is its
*ancestor* chain, excluding itself. A section containing only its heading therefore matches
zero rows, falls to the final arm, and raises `PM007 "no section with that heading path"` —
about a section `read_section` resolves fine and `blocks()` plainly shows.

That is exactly the first write to every freshly created memory document: the scenario's
single most common operation. The fix is cheap — fall back to the heading block itself,
using the ancestors-plus-own-text comparison `read_section` already computes — and it must
apply only to the `SectionNotFound` arm, not the `InvalidAnchor` arm just above it
([ops.rs:812-822](../extension/src/ops.rs#L812-L822)), or an append lands silently at the
*top* of a section that ends in a blockquote.

**This is the highest value per line of code in the entire document.** It is a small,
local, well-understood fix to the one operation where pgmind decisively beats a file, and
it needs no registry, no tenancy work, and no new RFC.

### 4.7 Smaller, verified

- **No index for "blocks in document D tagged X"** — an explicit requirement. `pgmind.tag`
  has `(vault_id, lower(tag))`, `(note_id)`, and `(block_id)`
  ([schema.rs:258-260](../extension/src/schema.rs#L258-L260)), but no
  `(note_id, lower(tag))`. One index fixes it.
- **`pgmind.raise_error(code, message, detail)` has no `hint`** — so no pgmind error can
  carry a recovery instruction, and the PM number never appears in the DETAIL an agent
  reads.
- **Import throughput.** `scripts/import-vault.sh` is correct and gated, but forks one
  `psql` per file at roughly 21 files/s. RFC-003 D8 already publishes the relevant baseline
  (3.902 ms/note) and its normative finding that the 2k notes/s target "describes the bulk
  import path, which amortizes that fixed cost across many notes per call." That is the
  argument for a batch write primitive, and it is already frozen — it just needs citing. A
  `cat` failure still yields a silently empty note, which wants its own PM code.
- **`markdown` has no `length`, no equality, no ordering** — every size, dedupe or sort goes
  through `::text`.

### 4.8 The reference page documented signatures that no longer existed — **fixed 2026-08-11**

Row 2 added `vault text DEFAULT NULL` to 28 functions and updated **two** of the signatures
on `sql.html`. The other 27 documented a call shape the catalog did not have, and no
parameter table on the page mentioned `vault` at all — the parameter was, in effect,
undocumented on the API reference. Corrected against `cargo pgrx schema`, and the check is
now a diff of every documented signature against the real catalog rather than a reading.

Two things fell out of doing it, both worth keeping:

- **`knowledge.tags($md$…$md$)` silently changed meaning.** `tags` used to be overloaded on
  arity alone — `tags()` against `tags(markdown)` — which is why `sql.html` told readers a
  bare literal reached the parser without a cast. `tags(vault text DEFAULT NULL)` gave it a
  one-argument `text` form, and an uncast literal prefers `text`, so that documented call
  now resolves to the vault overload: `PM018` normally, and a *different answer* if the text
  happens to name a real vault. Every example on the site already carries `::markdown`, so
  nothing was broken — but the prose was actively telling people to write the broken form.
  `blocks` and `links` have had this shape all along; `tags` has simply joined them.
- **Two captured outputs in `cookbook.html` had already drifted.** `r-verify-history`
  published `clean_notes = 15` against a real 14, and `r-vault-scope` published `2` note
  rows for the second vault against a real 1. Nothing regressed to cause this and nothing
  caught it, because `manual_gate.verdict()` reads **stderr only** — it proves a block still
  runs and still raises what the page says it raises, and is structurally blind to every
  value the block prints. `MANUAL-PLAN` §7.1 says so; this is what that costs in practice,
  found by replaying the page twice and diffing the numbers by hand. Worth a gate that
  compares stdout for blocks whose output is deterministic. It is also the second
  independent reason to be careful with the `section_path` sweep (§11 item 1), where 16
  pasted outputs would have needed exactly this treatment.

---

## 5. What the field actually does

Condensed from the research streams; full detail in the run artifacts.

**Explicit containers with a name and an id are universal.** Notion databases, Confluence
spaces, Contentful spaces, S3 buckets, Chroma/Weaviate/Qdrant collections, OpenAI vector
stores — every one is created explicitly and carries both a display name and a stable id.
The two systems that create a namespace implicitly on first write (Weaviate's auto-schema,
and the "name a dataset and we make it" pattern) both document it as a data-loss hazard at
scale. pgmind is currently on the wrong side of a settled question.

**Compare-and-swap is table stakes and is always ergonomic.** GitHub's Contents API takes
the blob `sha`; HTTP takes `If-Match`; Notion and Google use revision ids. The ergonomic
lesson is uniform: the token you must send back is *returned by the read you just did*, in
the same object. pgmind's `expected_head` is the right mechanism reached by the wrong path —
`notes()` returns `head_revision`, but `blocks()` does not, so a block-level edit needs a
second query to find the guard.

**A description is a first-class listing field everywhere.** Drive's `files.list` has
`description`; S3 has user metadata; every CMS has it. Listings are expected to answer
"what is this?" without opening the document. pgmind's listing answers only "where is it?"

**The filesystem tool set is the bar, and it is six tools**: read, write, list, edit, glob,
grep. Agents are fluent in it because it is small, the names are unambiguous, and every tool
does one thing. The consistent finding across MCP practice is that surface *size* is itself
a failure mode — more tools means more wrong choices, more tokens spent on definitions, and
worse selection accuracy. Anything proposed here has to justify itself against six.

**Postgres has the whole search story in-tree.** Generated `tsvector` columns
(`GENERATED ALWAYS AS … STORED`) with `websearch_to_tsquery`, `ts_rank_cd` and `ts_headline`
for snippets; `pg_trgm` with `word_similarity` and a GiST/GIN index for typo tolerance.
Both are incumbents. `pg_trgm` is contrib and allowlisted on every managed platform worth
naming.

**Token counting has no honest network-free answer for the target model.** Every local
tokenizer is an OpenAI encoding (cl100k, o200k). Anthropic's is not public. §9 takes this
seriously rather than shipping a number labelled as something it is not.

---

## 6. The proposed surface

The design synthesis proposed 23 new names. Two reviews independently said cut it; the
ergonomics review's line is the fair one — *"a complaint about a 43-function API answered by
shipping a 58-function API."* What follows is the reduced set, grouped by what it unblocks.

### 6.1 The container

*A tenant is not a pgmind object — decided 2026-08-10.* Applications have tenants, or
users, or both, or neither, and they want vaults per tenant, per user, per agent, or any
combination. pgmind ships **one container, the vault**, and the application encodes its own
hierarchy in the vault's name. That removes a column, a uniqueness rule, a GUC and an
entire concept from the surface, and it is why `pgmind.vault` below has no `tenant`.

```sql
knowledge.create_vault(name text, description text DEFAULT NULL,
                       vault_id uuid DEFAULT NULL,       -- caller-supplied, else minted
                       if_not_exists boolean DEFAULT false)
  RETURNS TABLE (vault_id uuid, name text)

knowledge.vaults(glob text DEFAULT '**')
  RETURNS TABLE (vault_id uuid, name text, description text, created_at timestamptz)

knowledge.vault_id(name text) RETURNS uuid
```

backed by a registry that makes the typo an error instead of a new namespace:

```sql
CREATE TABLE pgmind.vault (
  id          uuid PRIMARY KEY,
  name        text NOT NULL UNIQUE CHECK (pgmind.path_is_valid(name)),
  description text,
  created_at  timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE pgmind.note ADD CONSTRAINT note_vault_fk
  FOREIGN KEY (vault_id) REFERENCES pgmind.vault(id);
```

Four columns, deliberately. `kind`, `read_only`, `search_config` and a `settings` jsonb were
all in the first draft and are all cut: none is needed to satisfy the scenario, each is
additive later, and a `read_only` flag the writer can flip is a suggestion rather than a
control (it needs a grant to mean anything, which is isolation work, which is deferred).

Vault names reuse the frozen RFC-002 D8 path grammar, so `acme/alice/agents/billing` is a
legal name and the existing `glob_match` already handles `acme/**` — one grammar in the
product, not two. That is what carries the application's hierarchy: a tenant is a name
prefix, and `knowledge.vaults('acme/**')` lists that tenant's vaults without pgmind knowing
what a tenant is. `ltree` was the obvious fit and is rejected: its label charset excludes
`/` and spaces, so every name would need an encoding step.

`vault_id` is caller-supplied, defaulting to a minted UUIDv7 — *decided 2026-08-10*. An
application that already has a tenant/user/agent table wants the vault keyed by an id it
chose, so its own row and the vault agree without a second lookup or a mapping table to
keep in sync. Supplying it is also what makes provisioning idempotent: re-running setup
with `if_not_exists => true` and the same id is a no-op rather than a second vault with the
same purpose. A collision on either id or name raises rather than adopting the existing
vault, since silently returning somebody else's vault is the failure this whole document is
about.

`pgmind.drop_vault` is **not** proposed. It would delete revisions, and
[excision.rs:268](../extension/src/excision.rs#L268) states normatively that
`pgmind.revision` rows are never deleted — bulk vault deletion is a Law 8 question that
wants its own argument, not a convenience function smuggled in beside a registry.

Note `vaults()` returns **metadata only**. The synthesis had it returning per-vault note,
block, token and byte counts; the feasibility review called it the worst item in the
proposal, correctly — those aggregates make the agent's *first* call O(all blocks in every
vault it can see). Counts go behind an explicit flag or get maintained incrementally.

Deliberately **not** proposed: a tenant registry. See §7 — this is the unresolved question,
not an oversight.

### 6.2 The parameter

`vault text DEFAULT NULL` appended to every `knowledge.*` function, resolved as
*argument → GUC → the tenant's default vault*. Purely additive under Postgres default
arguments; existing positional callers are untouched; the GUC survives as the ergonomic for
a single-vault MCP session.

This is the single highest-value item in the document and the cheapest. It deletes the
`set_config` wrapper from every application entry point, makes the functions composable in a
join, and turns the silently-wrong LATERAL query into a correct one.

One caveat the synthesis missed and a review caught: `vaults()` returns `vault_id uuid` while
`notes(vault => …)` takes `text`, so the natural join needs `v.vault_id::text` on every call
— in exactly the query shape the parameter exists to enable. Either make `vaults()` return
the name first, or accept the overload.

### 6.3 Identity, listing and structure

```sql
knowledge.notes(glob text DEFAULT '**', vault text DEFAULT NULL, tag text DEFAULT NULL, …)
  RETURNS TABLE (note_id uuid, path text, title text, description text, head_revision uuid, …)

knowledge.note_id(path text, vault text DEFAULT NULL) RETURNS uuid
knowledge.outline(path text, vault text DEFAULT NULL, max_depth int DEFAULT NULL)
  RETURNS TABLE (block_id uuid, level int, heading text, section_path text[], tokens int, …)
```

`title` becomes frontmatter → first H1 → basename, in that order, instead of silently
meaning basename. `description` is stored — from frontmatter, or set explicitly — so a
listing is a lookup and not an N+1. `outline()` is the best-designed item here: it collapses
the read-modify-write loop to two round trips by returning a `section_path` that round-trips
directly into `read_section` and `append_to_section`.

Which exposes the §4.5 disagreement as a design decision, not just a bug: **`blocks()` must
not grow a `section_path` column beside its existing `heading_path`.** Two `text[]` columns
on one row that both read as "the heading path" is a coin flip for every agent forever, and
the wrong one is the one already in every cached example. Rename `heading_path` to
`ancestor_path` or drop it. Keeping both is the worst of the three options.

### 6.4 Search

```sql
knowledge.search(q text, vault text DEFAULT NULL, path text DEFAULT NULL,
                 tags text[] DEFAULT NULL, limit_n int DEFAULT 20)
  RETURNS TABLE (path text, block_id uuid, section_path text[], excerpt text, rank real)

knowledge.tagged(tag text, vault text DEFAULT NULL, path text DEFAULT NULL, …)
```

A `STORED` generated `tsvector` on `pgmind.block` rather than the cookbook's materialized
view. Law 7 beats a REFRESH schedule, and it retires a shipped recipe that is actively
unsafe: the recipe's view carries no `vault_id`, so a REFRESH under the wrong GUC replaces
one tenant's search index with another's.

`tagged()` gaining a `path` argument, plus the `(note_id, lower(tag))` index from §4.7, is
what makes "tags within one document" a lookup instead of a vault scan.

**Fuzzy search is cut for now — decided 2026-08-10.** It was the one item here needing a
new external dependency, and the in-house alternative does not work: the synthesis proposed
reusing `rebind.rs`'s Dice similarity, which is over token unigrams and bigrams
([rebind.rs:46](../extension/src/rebind.rs#L46)) — **word**-level. Its own flagship example,
`search('excalation refunds', mode => 'fuzzy')`, scores Dice = 0, because `excalation` and
`escalation` share no token. Typo search is character-level and the algorithm does not
transfer. So the choice is `pg_trgm` or nothing, and that choice is deferred rather than
made hastily. Requirement 10 stays unmet and is listed as such; `mode` is not in the
proposed `search()` signature above, so adding it later is additive.

### 6.5 Write-surface shape

- Six mutating functions return scalar `uuid`; `knowledge.blocks_changed(revision)` returns
  what the composite used to carry (§4.2).
- `append_to_section` gains the empty-section anchor fix and `create_missing => true` (§4.6).
- `knowledge.write_many(paths text[], docs markdown[], …)` for bulk ingestion, capped at
  1000. Parallel arrays rather than `jsonb`, because D6's frozen batching amendment forbids
  routing unbounded columns through a single `jsonb` argument. **Shipped 2026-08-11.**

  *Correction, measured 2026-08-11.* This bullet said "expect 600–900 notes/s, not 2k —
  parse, hashing and the revision row do not amortize; only WAL and commit do." The first
  half is right and the second half is worth nothing. Three ways to write the same 500-note
  corpus, three runs each, release build, local socket:

  | | ms/note (median of 3) |
  |---|---|
  | 500 autocommit `write()` statements | 8.494 |
  | one `SELECT count(write(…)) FROM staging` — the D8 baseline | 8.450 |
  | one `write_many()` | 8.445 |

  All three are inside each other's run-to-run spread. There is no speedup, at any batch
  size, because there was nothing left to amortize: WAL and commit are already ~0.16 ms of a
  ~8.4 ms note on this hardware, so removing 499 of them moves 2%, under the noise floor.
  Repeating with `fsync=on` changed nothing (verified the knob engaged — `SHOW fsync`).

  The reason to ship it anyway is the one this document exists for: requirement 11 said
  migration must be trivial, and one call is trivial where 500 are not. The performance
  claim that belongs in the docs is **round trips**, not throughput — `N` × RTT becomes
  1 × RTT, which is worth real time against a remote database and nothing over a socket.
  That is what the reference page now says.
- `hint` on `pgmind.raise_error`, and the PM number in every DETAIL.
- `length(markdown)`, `octet_length(markdown)`, and a btree opclass.

**Cut, on review:** `upsert_section` — the synthesis called it "write's note-scope behaviour
applied at section scope, so RFC-004 governs unchanged," and that is false. Restricting the
rebinder's candidate set to one section changes the semantics: a block that moved out of the
section is unmatchable and re-mints, where whole-document rebinding would have carried its
ID. That is a new identity operation, and identity is this project's declared #1 research
problem. It needs its own corpus and its own published match rate, or it needs to not exist.
The concurrency need it served is met by `append_to_section` once §4.6 is fixed.

**Also cut:** `use_vault`/`set_vault` as a pair — confusable to the point of being a
correctness hazard (both read as "set the vault"; one selects, one mutates metadata), and
`use_vault(local => true)`, the documented default, is a **silent no-op in autocommit**
(measured) — every psql statement, every MCP tool call and most driver `query()` calls.

---

## 7. Isolation — decided: scope it to the MCP shape, defer the boundary

*Decision, 2026-08-10.* The full fail-closed design below was costed and **deferred to
after the first public release**. Two reasons, and the second is the real one.

The costing is bad. The proposed `PGC_BACKEND` tenant plus a RESTRICTIVE policy works —
measured on PG 18.4, a guessed real vault id from another tenant returns zero rows with no
error and no oracle, and `SET pgmind.tenant` after connect raises `55P02`. But it implies
one connection pool per `(tenant, scope)`, which at the stated topology — a few hundred
tenants, thousands of users, ~4 vaults each — is thousands of pools. The design rejects
role-per-vault on exactly that cardinality argument and then reimposes it in pool form.
That contradiction is unresolved, and resolving it is a research task, not an
implementation task.

The better reason is that **0.1.0's consumer does not need it.** The agent-facing product
is the MCP server, and an agent there does not speak SQL — it calls tools. The MCP server
owns the connection and sets `pgmind.vault_id` from its own configuration. In that shape
the Userset GUC stops being a weakness, because the party who could abuse `SET` has no way
to issue one, and RLS becomes real defence-in-depth against the failure that actually
happens: a bug in the server, a forgotten predicate, a crafted path in a tool argument.

So for 0.1.0 the tenancy story is a **documented deployment pattern**, not a new mechanism:

1. The MCP server (or application) sets `pgmind.vault_id` per connection, from credentials
   it derived — never from anything the agent produced.
2. `SELECT pgmind.enable_vault_rls(force => true)` once per database.
3. The application role is not a superuser, holds no `BYPASSRLS`, and does not own the
   extension.

> **The rule that makes this work, and the only one:** the vault must never be an
> agent-settable tool argument. If `read_note` takes a `vault` parameter the model can
> fill in, every guarantee here is void. Vault selection is server-side configuration and
> must not appear in the tool schema.

State the boundary honestly in the docs, because it is narrower than the current manual
implies. **Defends against:** a buggy server, a forgotten filter, a hostile tool argument,
an agent that has learned another tenant's vault uuid. **Does not defend against:**
anything that can execute raw SQL on that connection, a superuser, or `BYPASSRLS`. The
website manual currently asserts a boundary the code does not provide; that text has to
change either way, and this is the change.

Three things still need doing now, none of which is the deferred redesign:

- **§4.1 is fixed** — excision no longer leaves its vault, and the admin surface is
  revoked from `PUBLIC`. That was the one hole this deployment pattern could not have
  covered, because it was reachable by anyone who could call a function.
- **§4.3 (the `''::uuid` pooled-connection poisoning) must be fixed**, since the pattern
  above is a pooled-connection pattern. The policy needs `NULLIF(current_setting(…), '')`.
  It is a one-line change to `enable_vault_rls` and it is now on the critical path.
- **`enable_vault_rls` should report what it could not cover** rather than silently
  skipping it (§4.4) — `pgmind.excision_replay` has no `vault_id` and falls outside the
  enumeration.

The full design — registry-anchored resolution, `PGC_BACKEND` tenant, RESTRICTIVE
policies, per-user scope — is preserved in the run artifacts and should become its own RFC
after 0.1.0, when the pool-cardinality question can be answered with a real deployment
rather than an estimate. **What it still could not stop, whenever it lands:** a superuser
or `BYPASSRLS` connection, and a tenant who controls their own connection string.
## 8. Size and tokens — what is actually deliverable

A BPE tokenizer is a pure function over bytes. It invokes no model (Law 1) and opens no
socket (Law 2), and the handbook already marks it `[DECIDED]`. So this is a schedule
question, not a decision — with one honesty problem the synthesis argued past.

**cl100k and o200k are OpenAI encodings.** This product's stated primary consumer is agents
over MCP, which in this project's own context means Claude. Anthropic's tokenizer is not
public and is not cl100k-compatible. A `tokens` column stamped on ten million blocks is
therefore a count for a model family the product does not target — typically within 10–20%
on English prose, materially worse on code blocks, tables and CJK. That is exactly the
argument the synthesis deployed *against* the `chars/4` estimate, and it never turned it on
itself.

What survives, and should ship:

- `pgmind.tokens(t text, model text)` **`IMMUTABLE` with the encoding explicit**. The
  synthesis declared it `IMMUTABLE` while resolving a NULL model through a GUC — a function
  whose result depends on a GUC is not immutable, and marked so it can be constant-folded
  into a plan or an expression index whose contents then silently disagree with it. Two
  functions: immutable with the model named, stable without.
- Never a fabricated number. `NULL` when no encoding is compiled in, not `chars/4`. A wrong
  number an agent budgets against is worse than no number — that instinct is right and rare.
- The encoding name travels with every count, inseparably, in the column name or a `NOT
  NULL` sibling. A number an agent budgets against must not silently change meaning after a
  `SET`.
- Do **not** store a note-level `source_tokens` recomputed on every write. It is a second
  full pass over bytes for a number derivable from the blocks the write path already has.
- Store the per-block count only after the capacity suite publishes the write-cost delta
  against RFC-003 D8's 3.902 ms/note baseline.

One more consequence nobody caught until the third review: `tsvector` is not in
`excision.rs`'s type enumeration, so `sweep()` cannot see a generated tsvector column and
`verify_excision` would attest erasure without looking at it. In practice the STORED column
recomputes on the redacting UPDATE and self-heals — but the RFC-005 D7 amendment exists
precisely because a filtered enumeration once produced a positive attestation over an
unopened lane. Adding a content-derived column of a type the enumeration cannot see
reintroduces that defect with the gate still green.

---

## 9. Sequencing

Written against `d037731`. Phase 4 is cut, so **Phase 5 is next**, and RFC-007 already owns
— by name, in three normative documents — the vault parameter, per-session tenant selection,
the search half, and the MCP surface. RFC-003 D1's own closing sentence assigns it: *"RFC-007
owns per-session tenant/role selection for the MCP surface and builds on this model."*

So the honest structure is **not** a new RFC in an invented phase. It is two documents:

- **A storage amendment RFC** — the registry table and FK. A genuine RFC-003 amendment.
  (Excision vault-scoping, the REVOKEs and the `op_result` arity change are **already
  done** — see §4 — and want an amendment recorded after the fact, not before.)
- **RFC-007, with its scope expanded** to the vault parameter and the documented MCP
  tenancy pattern (§7) it was already assigned.

Ordered by what unblocks the most for the least risk:

| | Work | Why here | Effort |
|---|---|---|---|
| 0 | ~~**The three verified bugs**~~ — excise scoping + REVOKEs (§4.1), `op_result` arity (§4.2), empty-section append (§4.6) | **Done 2026-08-10.** None needed a registry, a tenant GUC, a tokenizer or an index. Two were silent-corruption class. | ✅ |
| 1 | ~~**The pooled-connection RLS fix** (§4.3) + `enable_vault_rls` reporting uncovered tables (§4.4)~~ | **Done 2026-08-10.** `NULLIF` in the policy; the function now returns one row per table with `covered`, naming `excision_replay`. | ✅ |
| 2 | ~~**The vault parameter**~~ — `vault text DEFAULT NULL` on all 28 vault-scoped functions, accepting a name or a uuid | **Done 2026-08-10.** No table change. The scenario-4 join returns the right rows per row of the driving table; gated by `the_vault_is_a_parameter_so_a_join_can_vary_it_per_row`. | ✅ |
| 2b | **Identifiers** — `note_id` on `notes()`, real `title`, stored `description`, `section_path` | Split out of row 2: these need two new `note` columns and parser work, where the parameter needed neither. | M |
| 3 | ~~**Registry + `create_vault`/`vaults`/`vault_id`**~~ | **Done 2026-08-10.** Four columns, no `tenant`, caller-supplied ids, FK from `note.vault_id`, and `PM018` when a vault is not registered. | ✅ |
| 4 | ~~**`write_many`**~~ | **Done 2026-08-11.** Requirement 11's answer, capped at 1000, `PM019` when a batch is malformed. Not a throughput win — see the correction in §6.5. | ✅ |
| 5 | ~~**Search**~~ | **Done 2026-08-13.** `knowledge.search()` over a bounded expression GIN index, `tagged(path)`, the missing tag index; ratified in [RFC-007](rfcs/RFC-007-query-api-and-mcp-surface.md) D4 and gated by `search-quality`. The **tokenizer is not row 5's** — token budgeting is RFC-008 (*Deterministic Context Assembly & Token Budgeting*), which is where §8 belongs. | ✅ |
| 6 | **MCP surface**, carrying the §7 deployment pattern as its documented tenancy story | Phase 5 ⇒ **0.1.0 ships here.** Designed in [RFC-007](rfcs/RFC-007-query-api-and-mcp-surface.md) D6–D9 — eight tools — and **awaiting review before implementation**. | M |
| 7 | **Isolation — the real boundary** | *Deferred past 0.1.0* by decision. Needs the pool-cardinality question answered against a real deployment. | L |

The one sequencing argument to reject: the synthesis put the vault parameter *behind* the
isolation rebuild, reasoning that shipping the selector before the boundary means shipping a
knowingly unsafe selector. That is wrong. The parameter is exactly as safe as the GUC it
defaults to, which is what every deployment already uses. It is a strict ergonomic
improvement at identical security. Do not hold it hostage to an L-effort redesign with an
open question in it.

---

## 10. Alternatives weighed and rejected

- **Keep the Userset GUC and rely on `enable_vault_rls` + grants, as D1 documents.** The
  policy compares each row to the value the caller controls. The project's own test asserts
  the leak works.
- **Schema-per-vault or database-per-vault.** Tens of thousands of vaults at a few hundred
  tenants; catalog and plan-cache costs make this break well below the target.
- **Role-per-vault.** The only pattern immune to a hostile `SET`, and kept — as the optional
  role-anchored tier at role-per-*tenant* granularity. Per-vault is rejected on cardinality:
  a credential lifecycle per user per agent.
- **Implicit vault creation on first write.** Cheapest onboarding available, and a
  data-loss-shaped bug at multi-tenant scale. It is what pgmind does today by accident.
- **`ltree` for vault names.** Excellent operators, wrong charset.
- **Parsing `tag:` / `path:` prefixes out of the query string.** Measurably good for agents;
  rejected on Law 9 — silently reinterpreting a literal `tag:` in someone's text is hidden
  behaviour.
- **A `knowledge.write_retry(path, doc, tries)` helper.** RFC-005 D5.8 commits to never
  retrying internally, and `pg_sleep` inside a server transaction holds a snapshot. Retry
  belongs in the client; what the server owes is an error carrying enough to retry on, which
  is the `hint` work in §4.7.
- **Making `expected_head` mandatory.** Safer in isolation; makes the trivial case harder,
  and the first write to a new note has no head to supply.
- **`chars/4` when no BPE is compiled in.** What the whole field does, wrong by 2× on
  exactly the content a vault holds.
- **Keeping `pgmind.enable_vault_rls` as an alias for a renamed `enable_isolation`.**
  PRODUCT-PLAN §7.1 has rejected two-names-for-one-thing twice on the record.

---

## 11. Decided, and still open

**Decided 2026-08-10.**

- **Isolation is deferred past 0.1.0** (§7). For the first release the tenancy story is the
  documented MCP deployment pattern — the server sets `pgmind.vault_id` from credentials,
  RLS enforces it, and the vault is never an agent-settable tool argument. The boundary
  that survives raw SQL becomes its own RFC afterwards.
- **`create_vault` takes a caller-supplied `vault_id`** (§6.1), so an application can key a
  vault by an id it already has and make provisioning idempotent.
- **The three bugs are fixed** (§4.1, §4.2, §4.6), with regression tests and a negative
  control on the excision one.

**Also decided 2026-08-10.**

- **A tenant is not a pgmind object** (§6.1). Applications own tenancy; pgmind ships the
  vault and the application puts its hierarchy in the vault name. This deletes a column, a
  uniqueness rule, a GUC and a concept, and it answers the question the deferred isolation
  RFC would otherwise have had to reopen.
- **`verify_excision` is now vault-scoped**, and the claim it supports has been narrowed in
  the README to match: "proven erased from this vault" rather than "proven erased."
- **Fuzzy search is cut** (§6.4) — no `pg_trgm` dependency for now. Requirement 10 of §3
  stays unmet, deliberately and visibly.
- **`upsert_section` stays cut** (§6.5).

**Still open.**

1. **The `heading_path` rename / `section_path`** (§6.3) — **attempted 2026-08-10, reverted,
   and it needs a real decision rather than a rename.**

   The friction is genuine: a block's `heading_path` is its *ancestor* chain, so for a
   HEADING it excludes the heading itself. A caller listing a table of contents out of
   `blocks()` therefore cannot feed an entry straight back into `read_section` or
   `append_to_section` — it must know to append `attrs->>'text'` first, and gets PM007 if it
   does not. That is requirement 5 of §3, and it is worth fixing.

   I implemented the clean fix — `blocks()` returning a single `section_path` that means
   "the section this block is in", round-tripping for every kind — and backed it out for two
   reasons found in the doing:

   - **It cannot be made consistent.** `blocks_as_of()` reconstructs structure from the
     stored history vectors, which carry the ancestor chain and not the heading's own text.
     Deriving that text needs the parser, and RFC-005 invariant **X2** states that
     reconstruction never invokes the parser — that is what makes history survive an RFC-002
     amendment. So `blocks_as_of()` *cannot* return a round-tripping `section_path` at all.
     Renaming only `blocks()` leaves two sibling functions permanently disagreeing about
     both the name and the meaning of the same concept, for a frozen reason.
   - **The doc cost is 16 blocks across all five pages**, and the captured *values* change
     too, not just the column header — which the manual gate cannot check (MANUAL-PLAN §7.1:
     pasted output is not compared). Sixteen hand-recaptured outputs is exactly where a
     wrong paste slips in.

   So this is not "rename a column during the doc sweep". It is a design question: either
   `blocks()` and `blocks_as_of()` both expose the ancestor chain and pgmind ships a
   separate way to address a section, or history starts storing heading text and X2 is
   revisited. **Decide it before the sweep, not during.**
2. **`pg_trgm`** — whether fuzzy search is worth one contrib dependency. Reopen when the
   FTS half has shipped and you can see what it does not cover.
3. **Isolation** (§7) — the real boundary, and the pool-cardinality question underneath it,
   after 0.1.0.
4. **`knowledge.vaults()` enumerates every tenant, and RLS cannot stop it** — found 2026-08-13 while
   designing the MCP surface, and **measured**, not inferred.

   `pgmind.vault` is the one table with no `vault_id` column, and `enable_vault_rls` builds its
   table list by scanning `pg_catalog` for `attname = 'vault_id'` — so the registry is
   *structurally* outside the boundary the rest of the schema sits inside. On PG 18.4, with
   `enable_vault_rls(force => true)` and a non-owner role scoped to one vault: `pgmind.note`
   returns **0** rows for the other vaults, and `knowledge.vaults()` returns **all of their
   names**. No note content leaks. The names do — and vault names are exactly where §6.1 tells
   applications to put their tenant and user hierarchy, so `globex/bob/secrets` discloses a
   customer and a user.

   The function already reports this (`enable_vault_rls` lists `vault` as `covered = false`) and
   the reference page now says so explicitly. It is **not** reachable by an agent under the §7
   deployment pattern, because an agent does not issue SQL and RFC-007 D6.5 forbids exposing this
   function as a tool. It *is* reachable by anything that can run SQL on that connection — which
   §7 already lists under "does not defend against".

   The fix is small and is a decision, not a cleanup: a policy on `pgmind.vault` keyed on `id`
   rather than `vault_id`, which would make `vaults()` return only the current vault under RLS,
   and would take listing-to-choose away from operators using `force => true`. Under X1 nothing
   *should* be listing to choose — the server is told its vault by credentials — so the cost is
   probably zero. Not shipped unilaterally because it changes what a shipped function returns.

5. **RFC-003 D8's published numbers no longer match its published artifact** — found while
   benchmarking `write_many`, and not fixed here because RFC-003 is frozen and the numbers
   are yours to rule on.

   D8's own post-batching amendment says the write-cost table is "quoted from"
   `eval/published/capacity-model-v1.json` and that "a number this section cannot regenerate
   is not a published number." The RFC prose says **3.902 ms/note**; the committed artifact
   has said **5.873 ms/note (170.3 notes/s)** since `c873354`, when `make eval` regenerated
   it. So the deliverable was regenerated and the section quoting it was not — the exact
   defect that amendment was written to close, reopened by the mechanism it prescribed.

   Two readings, and they need different fixes. If the artifact is right, the prose is stale
   and someone should regenerate the table with it. If the prose is right and the artifact
   just records a slower laptop, then D8 is publishing a hardware-dependent number as if it
   were a property of pgmind, and it needs to say which machine — which is the same problem
   in a different place. Either way the repo currently states two write costs and calls one
   of them a quotation of the other.

The critical path to 0.1.0 is now §9 rows 2b, 4, 5 and 6.
