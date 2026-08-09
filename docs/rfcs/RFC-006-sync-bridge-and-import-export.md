# RFC-006: Sync Bridge & Import/Export — **withdrawn**

- **Status:** Withdrawn 2026-08-09 (never accepted; no implementation existed)
- **Phase:** 4 — cut with it
- **Owner:** amin
- **Created:** 2026-08-09 · **Withdrawn:** 2026-08-09

> The full draft — nine decisions covering the path↔filename mapping, a state file, per-block
> three-way merge, conflict strategies, ignore rules, watch mode, git interaction and freshness
> metadata — is in this file's history at `ec076e9`. It is not reproduced here, because a
> withdrawn RFC that still reads like a specification invites someone to build it.

## 1. Why it existed

[Handbook law 4](../PGMIND.md) promises the vault is always exportable, and v0.1 of the plan
answered that with a CLI: `pgmind import`, `pgmind export`, and `pgmind sync ./vault --watch`
keeping a real folder and the database in continuous two-way agreement. The last of those was
the headline — *"local Obsidian and server pgmind aren't rivals but two views of the same
vault."*

## 2. Why it was withdrawn

Three arguments, in increasing order of weight. The first two were already written down in this
repository, in documents that outrank this RFC, and had simply never been read against it.

**1. It served the user the handbook says to cede.** [Handbook §11](../PGMIND.md)'s risk table
answers *"why not just files + SQLite/git?"* with: *single-writer local use should stay on
files; pgmind starts winning at concurrent agents, server backends, multi-tenancy — lead with
those.* A continuous two-way bridge exists to serve exactly one human editing exactly one local
folder. The project was proposing to build the half of the market its own constitution declines
to compete for.

**2. It manufactured the risk it was cited as mitigating.** The same risk table listed *"sync
bridge minimizes full-document replaces"* as a mitigation for rebinding quality — the project's
self-declared #1 research problem. That was backwards. Every editor save is a whole-document
replace, so a continuous bridge is the largest possible *generator* of heuristic rebinding, and
it generates it from the least controlled source there is. Cutting the bridge removes a risk
driver; it does not create one.

**3. The complexity was entirely in the half that was cut.** Of nine decisions, four (state
file, three-way merge, conflict strategies, watch mode) and two of four benchmark suites
existed only for two-way sync. Byte-exact import and export — everything law 4 actually
requires — is two shell scripts and one gate.

## 3. What was kept, and the measurement that shaped it

The prompting question was whether a user could just write the shell loop themselves. Measured
against this repository's own cookbook recipe, over a vault of **eight notes whose paths are all
legal** per `core/src/path.rs`:

| | |
|---|---|
| Notes in | 8 |
| Files on disk | 7 |
| Notes surviving a round trip | **6** |
| Errors printed | **1** |

- `notes/it's mine` — the recipe interpolated the path into a single-quoted SQL literal, so
  `psql` raised a syntax error *after* the shell redirect had already created the file. A
  zero-byte file that reads as an empty note.
- `Projects/Auth` and `projects/auth` — one file on case-insensitive APFS. The survivor was
  *named* `Projects/Auth.md` and *contained* `# Lower`. Content served under the wrong note's
  name, with **no error at all**.

So "it's just a bash command" holds for a tidy ASCII vault and fails silently outside one. What
replaced the bridge is [`scripts/export-vault.sh`](../../scripts/export-vault.sh) and
[`scripts/import-vault.sh`](../../scripts/import-vault.sh), which fix both classes — psql
variable interpolation for quoting, and a pre-flight collision check that **writes nothing at
all** when two paths would land on one file. The `folder-round-trip` suite in `eval/harness.py`
gates them over a corpus of legal-but-hostile paths, with a negative control in
`suite_gate_selftest` proving the checker sees a single flipped byte.

The one decision worth carrying forward if this is ever revisited: the **refusal**. An export
that half-succeeds is worse than one that fails, because the user keeps the folder.

## 4. Consequences

`revision.source` still permits `'sync'` and now nothing can ever write it — the same state
`'rebind'` is in. RFC-012 retires both when there is an extension upgrade mechanism to carry the
schema change. The Phase 3 exit's deferred concurrency clause (interleaved sync and API writes)
is **withdrawn rather than deferred**; `eval/published/phase-3-gates.json` keeps its original
text with a supersedes note appended, because the record of what was true at the exit is not
rewritten.

Phase numbering is unchanged: Phase 5 follows Phase 3. Renumbering would falsify three frozen
RFCs and four published artifacts that name their phase, to buy tidiness.

**Reversing this** requires a new RFC that argues past §2's point 2 — that continuous two-way
sync will not degrade identity quality more than it delivers — with the rebinding corpus as
evidence rather than assertion. The `ec076e9` draft is a starting point for the mechanics, not
for the case.

## 5. Benchmark gate

Not applicable — withdrawn before acceptance, and no gate is owed for work that will not be
done. The obligation law 4 does create is discharged by `folder-round-trip`, which is a Phase 3
artifact now, not a Phase 4 one.

## 6. Law compliance

**Law 4 (no lock-in)** is the only law in play, and it is the reason this RFC could not simply
be deleted: withdrawing the CLI without leaving something behind would have left the
constitution promising a command that does not exist. The scripts and their gate are what make
the reworded law true.
