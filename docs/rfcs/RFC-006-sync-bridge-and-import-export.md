# RFC-006: Sync Bridge & Import/Export — a folder and a vault, kept honest

- **Status:** Draft — proposed for acceptance
- **Phase:** 4
- **Owner:** amin
- **Created:** 2026-08-09 · **Frozen:** —

## 1. Context

[Handbook §4 law 4](../PGMIND.md) promises that the vault is always exportable: migration in is
one command, migration out is one command, and "local Obsidian and server pgmind aren't rivals
but two views of the same vault." Nothing in Phases 0–3 delivers that. The extension is a
closed world reachable only through SQL.

That promise is also the project's honest answer to the competitor named in
[handbook §10](../PGMIND.md): **markdown files on the filesystem**. A tool that cannot hand the
files back is asking for a bet nobody sensible makes.

Three things arrive with this RFC, and only one of them is code the user sees:

1. **The mapping.** [RFC-002 D8](RFC-002-markdown-type-ast-vault-syntax.md) settled the path
   grammar and deliberately deferred the filename question: *"no `.md` suffix concern here (that
   is RFC-006's filename mapping)"* — `core/src/path.rs:7`. A pgmind path may legally contain
   `:` `*` `?` `"` `<` `>` `|`, is case-**sensitive**, and is NFC. Every one of those is a
   collision waiting on a real filesystem.
2. **The merge.** Two writers now exist that this project has never had at once: an agent
   writing through SQL and a human writing in an editor. [RFC-005](RFC-005-version-engine-concurrency-and-excision.md)
   settled what happens when two SQL writers collide. Nothing says what happens when the file
   and the row both changed.
3. **The deferral recorded at the Phase 3 exit.** `revision.source` has permitted `'sync'`
   since [RFC-003](RFC-003-vault-and-block-storage-layout.md) (`extension/src/schema.rs:77`) and
   nothing has ever been able to set it. `eval/published/phase-3-gates.json` records the
   concurrency gate's interleaved-sync clause as deferred here, with the reason: the only thing
   that could produce a `'sync'` write is the bridge.

The reader should assume nothing about the bridge except that it is a program outside the
database ([law 1](../PGMIND.md): there is no pgmind process that calls a model, and no pgmind
process inside Postgres at all beyond the extension).

## 2. Decision

### D1. The mapping is total, target-independent, and reversible — or the export refuses

A note at path `p` maps to the file `p + ".md"`, relative to the vault root, with `/` as the
directory separator. Import maps `f` to the path `f` with **exactly one** trailing `.md`
removed. A note at path `notes/readme.md` therefore exports to `notes/readme.md.md` and comes
back as `notes/readme.md`; the mapping is a bijection on its domain, not a heuristic.

Files that do not end in `.md` are **not notes**. They are ignored on import and never written
on export. There is no second extension.

**Percent-encoding, applied everywhere or nowhere.** These characters MUST be encoded as `%XX`
(uppercase hex, of the UTF-8 bytes) in a filename, in every direction, on every operating
system:

```
<  >  :  "  |  ?  *  %          and any byte < 0x20
```

`%` is in the set so that decoding is unambiguous. A path segment that is a Windows reserved
device name (`CON`, `PRN`, `AUX`, `NUL`, `COM0`–`COM9`, `LPT0`–`LPT9`, with or without an
extension, case-insensitively) has its first character encoded: `CON` → `%43ON`. A segment
ending in `.` or a space has that final character encoded.

The encoding is **not** conditional on the running platform. A vault exported on Linux and a
vault exported on Windows are byte-identical folders, because the whole point is that the same
folder can be synced from both. A conditional encoding would make the Mac and the Windows
checkout of one git repository permanently disagree.

**Case and normalization collisions refuse.** Two distinct pgmind paths may map to the same file
on a case-insensitive or normalization-insensitive filesystem (`Projects/Auth` vs
`projects/auth`; NFC `café` vs the NFD the filesystem hands back). Encoding cannot fix this —
the collision is in the filesystem's equality, not in the characters. Export therefore:

- computes the full mapping **before writing anything**;
- groups the result by `casefold(NFC(filename))`;
- if any group has more than one member, **writes nothing at all** and exits non-zero with
  every colliding group listed.

This is `excise`'s rule (refuse, don't half-erase) applied to the folder. `--on-collision=suffix`
appends `~1`, `~2`, … in pgmind-path byte order and records the assignment in the state file;
it is opt-in because the resulting names no longer describe themselves and only the state file
can invert them.

**Import normalizes.** Directory entries are NFC-normalized before becoming paths (macOS emits
NFD; `path_normalize` already does exactly this — `core/src/path.rs:15`). A file whose decoded
path fails `path_is_valid` is skipped and reported; it is never silently renamed into validity.

### D2. The state file is the merge base, and it is local

`.pgmind/state.json` in the vault root. `.pgmind/` is created with a `.gitignore` containing
`*` on first write, so the bridge excludes itself from the user's repository without touching
their `.gitignore`.

```json
{
  "format_version": 1,
  "vault_id": "00000000-0000-0000-0000-000000000000",
  "extension_version": "0.0.1",
  "synced_at": "2026-08-09T05:00:00Z",
  "notes": {
    "projects/auth": {
      "file": "projects/auth.md",
      "revision": "019fe4fc-cd4f-7a4f-8b3b-03ea258929af",
      "base_sha256": "9f2b…",
      "size": 412,
      "mtime_ns": 1786000000000000000
    }
  }
}
```

`base_sha256` is the SHA-256 of **the bytes last agreed by both sides** — what was written to
disk, or read from it, at the last successful sync of that note. It is the common ancestor of
D3's merge and the only field that must be right. `size` and `mtime_ns` are a fast path for
"unchanged"; a mismatch means *re-hash*, never *assume changed*, and the hash is always the
authority.

This is deliberately **not** pgmind's `content_hash`. That hash is per-block and normalizes
(RFC-002 D6); the merge base has to be the exact bytes, including the trailing newline the user's
editor did or did not leave.

The state file is per-clone and gitignored: `mtime_ns` is machine-local. A sync with no state
file is not an error — it is a first sync, and D3 handles it.

### D3. Three-way merge is per-block, and only when the changes are disjoint

For each note, three versions: **base** (`base_sha256`'s bytes, absent on first sync), **local**
(the file now), **remote** (`knowledge.read(path)` now).

| local vs base | remote vs base | Action |
|---|---|---|
| same | same | nothing |
| changed | same | push: `knowledge.write(path, local, expected_head => state.revision)` |
| same | changed | pull: write the file, update state |
| changed | changed | D3.1 |
| *(no base)* | note exists | D3.2 |

**D3.1 — both sides changed.** Parse all three with `knowledge.blocks(doc markdown)`, the
parse-without-storing function that already ships. Compute the set of blocks each side changed
against the base, keyed by position and content. If the two sets are **disjoint**, apply both:
the local changes go up as a single `write`, the remote changes come down in the same pass, and
the merged bytes become the new base. If they **overlap**, it is a conflict (D4).

This is not a new invention; it is `update_block`'s `expected_hash` rule (RFC-005 D5.11,
PM016 — "disjoint patches both land") applied across the bridge instead of across two SQL
sessions. Choosing the same rule twice is the point: one concurrency story, gated once, true in
both places.

The bridge MUST NOT attempt a line-level text merge. A line merge produces plausible markdown
that no one wrote, and pgmind's whole claim is that it never does that.

**D3.2 — no base.** A note exists on both sides with no recorded ancestor (first sync of a
folder into a non-empty vault, or a state file that was deleted). If the bytes are equal, adopt
them as the base and record it. If they differ, it is a conflict — there is no ancestor and
therefore no honest merge. `import` is the command for "the folder wins"; `export` is the
command for "the vault wins".

### D4. A conflict changes nothing, and never enters the vault

Default `--on-conflict=stop`: the note is left **untouched on both sides**, the conflict is
reported on stderr with both revisions named, the sync continues with the other notes, and the
process exits `2`. One unmergeable note does not strand a thousand mergeable ones; the exit
code still says the run was not clean.

`--on-conflict=ours` takes the vault, `theirs` takes the file. Both are whole-note choices, not
merges.

`--on-conflict=markers` writes `.pgmind/conflicts/<file>` containing the three versions with
git-style markers and leaves both sides untouched, refusing to sync that note again until the
file is removed. Conflict markers are **never** written into the note itself and never into the
vault tree: `<<<<<<<` inside a note would be parsed, stored, hashed, and pushed to the database
as content, which is precisely the corruption this rule exists to prevent. `.pgmind/` is outside
the note tree, so the artifact cannot be re-imported by construction.

### D5. Ignore rules: `.pgmindignore`, gitignore syntax

`.pgmindignore` in the vault root, gitignore semantics (`#` comments, blank lines, `!`
negation, leading `/` anchors to the root, `**` across segments, trailing `/` matches
directories). Gitignore is chosen over the `notes()` glob grammar (RFC-002 D8) because the user
of this file is a person with a `.gitignore` habit, not a caller of `notes()`.

Always ignored, not overridable: `.pgmind/`, `.git/`, anything not ending `.md`, and any path
component beginning with `.`. Ignoring a note that is already in the vault does **not** delete
it from the vault; it stops tracking it, and `sync` says so once.

### D6. Watch mode watches one side, and says so

`--watch` uses filesystem notification with a 300 ms debounce, coalescing events per file, and
falls back to polling at 2 s where notification is unavailable (network mounts, some
containers).

The database side is **polled**, at the same 2 s, comparing `head_revision` from
`knowledge.notes()`. There is no push notification from the vault, because there is no trigger
and no background worker in the extension ([law 1](../PGMIND.md), and RFC-003's deliberate
omission of triggers). A future RFC may add `LISTEN`/`NOTIFY`, which would require the extension
to emit on write — a change to the write path, not to the bridge. Until then the honest
statement is: **file changes are seen in milliseconds, vault changes in up to two seconds.**
That asymmetry is documented in the manual, not hidden behind the word "watch".

### D7. Freshness is `source` and `author`, not a new column

The bridge sets two session GUCs before every write:

- `pgmind.author` = `sync:<hostname>` by default, overridable with `--author`. RFC-011 D1's
  rules apply unchanged, including that it is a claim and not a verified fact.
- **`pgmind.source`** — new, and the only extension change this RFC requires. Userset, default
  `'api'`, accepting `'api'` and `'sync'` only. It populates `revision.source`, whose CHECK has
  permitted `'sync'` since RFC-003 and which nothing has ever been able to set.

`'rebind'` remains in the CHECK constraint and remains unsettable; removing it is RFC-012's
job, as recorded at the Phase 3 exit. `pgmind.source` MUST reject it, so the GUC cannot be used
to write a value the project has already decided to retire.

This gives freshness with **no schema change**: `knowledge.history()` already returns `source`,
`author` and `created_at`, so "when did the bridge last touch this note, and from where" is a
query that works today. A `last_synced_at` column would be a second copy of a fact the revision
log already holds, and RFC-011 D3 already rejected that shape once.

### D8. Import and export are one-way, and import is resumable

`pgmind import <dir>` — files win, no state required, writes one on completion.
`pgmind export <dir>` — vault wins, refuses per D1 rather than writing a partial folder.
`pgmind sync <dir>` — two-way, per D3.

Import writes **one transaction per note**, not one per run. A 10 000-note vault in a single
transaction holds locks for the whole import and rolls back everything on the last file's typo.
Because a byte-identical `write` is already a no-op that creates no revision
(`extension/src/write.rs:946` — the idempotence short-circuit), a partial import is resumed by
re-running it. `--atomic`
opts into the single transaction for people who want all-or-nothing more than they want
resumability.

Notes are processed in **pgmind-path byte order** in every direction, so two runs over the same
inputs produce the same revision order, the same file order, and the same diff.

Export writes **LF line endings and no trailing-whitespace changes**, byte-for-byte what
`knowledge.read()` returned. `git diff` after an export with no vault changes MUST be empty —
that, and `.pgmind/` self-ignoring, is the entirety of "git-friendly". The bridge never runs
git, never reads `.git/`, and never commits.

### D9. What the bridge is not

- It does not resolve links, repair edges, or touch identity. `knowledge.write` does all of
  that, and the bridge's only lever on identity is that re-importing a whole file is a
  whole-document replace, which runs the rebinder (RFC-004 Part B) with its published
  confidence. `^id` markers in the file are the deterministic escape hatch and survive the round
  trip because they are content.
- It does not filter, transform, or lint markdown. Bytes in, bytes out.
- It is not a backup. `pg_dump` is the backup; an exported folder has no history, no block IDs,
  and no excision log.

## 3. Alternatives considered

**Conditional (platform-aware) filename encoding.** Encode `:` only on Windows. Lost because the
two checkouts of one git repository would then disagree forever, which defeats the only reason
the bridge exists. The cost of encoding everywhere — an occasional `%3A` in a filename on Linux
— is visible and small; the cost of divergence is a corrupted shared vault.

**Line-level three-way merge (diff3/git merge-file).** Lost on the project's own terms. It
fabricates content, and pgmind's differentiator against "markdown files plus glue" is that it
does not. Block-level disjointness is weaker, refuses more often, and is explainable in one
sentence — and it reuses a rule that already ships and is already gated (RFC-005 D5.11).

**A `last_synced_at` column on `note`.** Lost to RFC-011 D3's precedent: a per-revision fact
belongs in the revision log, not duplicated into current state. `revision.source='sync'` was
already reserved for exactly this and had been waiting since RFC-003.

**Storing the merge base in the database** (a `sync_base` table) instead of a state file. Lost
because the base is per-*folder*, and one vault can be synced to many folders on many machines.
Putting it in the database would either collide between clones or require a client identity the
project does not have. It also makes `.pgmind/` deletable as a recovery step, which D3.2 relies
on.

**Percent-encoding vs. a private-use-area escape or Base32 segments.** Percent-encoding lost
nothing: it is the one escaping scheme every developer reads without a legend, and `%` is
already rare in note titles. PUA characters are invisible in editors; Base32 destroys
readability for the whole segment rather than one character.

**`.pgmindignore` with `notes()` glob syntax.** Rejected: the two files have different readers.
Nobody hand-writes a `notes()` glob; everybody has written a `.gitignore`.

## 4. Consequences

**Easy after this.** A vault is a folder; Obsidian, git, ripgrep and a text editor all work on
it. `revision.source` finally distinguishes an agent's SQL write from a human's editor save, so
"what did the humans change this week" becomes a query. The Phase 3 concurrency gate's deferred
interleaving clause becomes testable, because something can now produce a `'sync'` write.

**Hard after this.** The `%XX` encoding is now a compatibility surface: changing the escaped set
renames files in every synced vault. Adding a character to the set is a breaking change to
folders in the field, so the set is chosen to be the Windows-illegal set plus `%` and nothing
else, and it will not grow. `format_version` in the state file exists so that the *state* can
migrate; the *filenames* cannot, cheaply.

**Impossible without a new RFC.** Sub-second vault→file propagation needs the extension to emit
`LISTEN`/`NOTIFY` on write — a write-path change, hence RFC-005 territory, hence a new RFC. So
does any form of partial-note sync (syncing a section rather than a note), which would need a
file format that is not plain markdown.

**What reversing this costs.** Abandoning per-block merge for line merge would be an RFC that
has to argue against RFC-005 D5.11 as well, since the two share a rule. Abandoning the state
file means finding a new home for the merge base, and D3.2's "no base is not an error" behaviour
would have to be redesigned rather than adjusted.

## 5. Benchmark gate

Four suites in `eval/`, plus the quickstart. **No gate, no acceptance.**

**5.1 `sync-round-trip` — zero tolerance.** A corpus at
`eval/corpora/pgmind/sync/` whose cases are adversarial by construction, not by accident:
every character in the encode set; a segment that is `CON` and one that is `con.md`; segments
ending in `.` and in a space; a 1024-byte path; `Projects/Auth` beside `projects/auth`; NFC and
NFD spellings of the same word; a note whose path already ends in `.md`; an empty note; a note
with no trailing newline; CRLF content; a 4-byte-emoji filename. For every case that is not a
declared collision: `import → export` MUST be byte-identical, and `export → import` MUST leave
`knowledge.read()` byte-identical. For every declared collision case: export MUST write nothing
and name the collision. **Threshold: 100%, and a partial write is a failure even if every byte
it wrote was correct.**

**5.2 `sync-merge` — publish the honest number.** A synthetic edit corpus in the shape of
`eval/corpora/pgmind/rebinding/` (the same file format, the same `make reindex-corpus`
discipline): paired file-side and vault-side edits, each labelled `disjoint` or `overlapping`
by hand. Two metrics, both published, neither thresholded on first release because the corpus
sets the operating point rather than the reverse:

- **merge rate** — disjoint pairs that merged without a conflict;
- **false-merge count** — pairs where a merge landed content neither side wrote, or dropped
  content one side wrote. **This one IS zero-tolerance.** A missed merge is an inconvenience the
  user resolves; a false merge is silent data loss, and D3's refusal to text-merge exists
  precisely to make it structurally impossible rather than statistically rare.

**5.3 `sync-torture` — zero tolerance.** Rename storms (100 notes renamed in one pass, including
swaps `a→b, b→a`); simultaneous edits on both sides of the same note and of different notes;
`SIGKILL` between the write-to-disk and the state-file update, then a resumed sync; a read-only
vault directory; a note deleted on one side and edited on the other; the state file truncated to
zero bytes mid-run. The invariant, checked after every scenario: **no note ends in a state where
the file and the vault differ without a conflict having been reported.** Silent divergence is
the only unrecoverable failure mode here, because the user stops looking.

**5.4 `sync-provenance`.** Every revision the bridge creates has `source='sync'`; no revision any
other path creates does. `pgmind.source` rejects `'rebind'` and every value outside the CHECK.
Zero tolerance — this is RFC-011's contract shape, and the same reasoning: provenance that is
sometimes right is not provenance.

**5.5 The quickstart.** The 5-minute quickstart in the manual runs in CI on a clean machine from
this phase forward, per the product plan's standing rule. It fails the build, not a report.

**Gate-selftest obligation.** Every checker above lands with a negative control in
`suite_gate_selftest`, per RFC-005 §5.0(b). A round-trip checker that cannot detect a corrupted
byte, or a torture checker that cannot detect divergence, does not count as shipped.

## 6. Law compliance

**Law 1 (no AI in the middle).** The bridge is a deterministic file-and-SQL program. It calls no
model, opens no socket other than to Postgres, and adds no background worker to the database.
The one extension change is a GUC.

**Law 2 (no synchronous network I/O in a transaction).** The bridge is a client. Filesystem I/O
happens outside the transactions it opens; each note's write is one short transaction.

**Law 4 (no lock-in).** This law is what the RFC exists to discharge. D1's refusal rather than
lossy export is the strict reading: an export that silently mangled a filename would satisfy the
letter of "exportable" and break the promise.

**Law 11 (`knowledge` is the public API; `pgmind` is admin).** The bridge uses only
`knowledge.*` for reading and writing notes. It reads `knowledge.notes()`, `knowledge.read()`
and `knowledge.history()`, and writes through `knowledge.write()`. It does not touch `pgmind.*`
tables. `pgmind.source` is a GUC, not a table, and mirrors `pgmind.author`'s precedent from
RFC-011 D1.

**RFC-002 D8 (path grammar) and RFC-003 D5 (storage enforcement)** are upstream of this RFC and
unchanged: the bridge normalizes and validates using `pgmind-core`, the same crate the extension
links, so the CLI and the server cannot disagree about what a path is.
