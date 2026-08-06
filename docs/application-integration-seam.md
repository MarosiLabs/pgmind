# The application-integration seam

*Design note, 2026-08-06. Written after trying to answer a plain question — "I have users, users have articles, the article body is markdown; how do I wire that up?" — and finding the answer took two wrapper functions, a trigger, and a warning.*

---

## 1. The verdict

pgmind is not badly designed. Almost every decision in RFC-002/003/005 has a written rationale, and most of them are right. The friction has a narrower cause:

> **pgmind has two intended consumers with two different notions of identity, and only one of them was ever designed.**

The vault model — identity is a path, the namespace is ambient session state — is coherent, deliberate, and correct for agents over MCP. The relational model — identity is a value you can put in a column, join on, and constrain — was named as a headline scenario and then never given an RFC. Applications land on the seam between them, and the boilerplate they write is exactly the shape of the missing design.

One decision inside that gap, **RFC-003 D1**, is load-bearing and I think genuinely wrong. The rest are consequences.

---

## 2. Both consumers are in scope, on the record

This is not a case of someone using the tool for something it never claimed to do.

RFC-000 D3, frozen:

> Primary consumer of the API: **agents via MCP; applications via SQL.**

The handbook's normative scenario list ([PGMIND.md §2](PGMIND.md), scenario 4):

> **SQL-joined knowledge.** Context assembly filtered by operational data in the same database (`WHERE customer_id = …`) — impossible for filesystem vaults and managed RAG APIs alike.

That is exactly the users-have-articles question, and it is sold as a *differentiator* — the thing filesystem vaults can't do. Section 6 below shows the public API returns a wrong answer for it.

Meanwhile every frozen RFC is vault-internal. RFC-002: markdown, AST, vault syntax. RFC-003: storage layout. RFC-004: block identity. RFC-005: versioning. There is no RFC that asks *what does an application's table look like when it points at a pgmind note?* The question was never on anyone's desk, so it has no answer, so every user derives their own.

---

## 3. Root cause: filesystem identity, relational expectations

Every friction point below is the same substitution. pgmind inherited the Obsidian data model faithfully — which was the point, per RFC-000 §1 — including its **identity semantics**:

| | Filesystem / vault | Relational |
|---|---|---|
| identity | a path | a surrogate key |
| namespace | ambient (cwd, "the open vault") | a column |
| attribution | the process user | a parameter |
| deletion | `unlink` | FK cascade |
| linkage | resolved by name at read time | a constraint enforced at write time |

The **left column is the extension's public API today**. The right column is what a table wants. Postgres is very good at the right column, and pgmind runs inside Postgres, which is what makes the mismatch feel gratuitous rather than merely unfamiliar.

None of this is an argument that the left column is wrong. For an agent holding one vault open over MCP, ambient namespace is the *correct* ergonomic — it's why nobody passes the working directory to every `cat`. The error is treating it as the only surface.

---

## 4. Symptom 1 — the vault is a GUC, not a parameter (the real bug)

RFC-003 D1, frozen:

> API functions operate in the **current vault**: GUC `pgmind.vault_id` (userset, uuid literal…). **Function signatures stay path-only**; multi-tenant callers `SET pgmind.vault_id` per session/transaction.

That sentence is the root cause. A GUC is ambient state; it has exactly one value per session at any instant. Therefore:

**It cannot vary per row.** Any query that spans two tenants is unexpressible in the public API. Not slow — unexpressible.

**It cannot be composed.** `knowledge.notes()` cannot appear in a join whose driving table selects the vault, because there is nowhere to put the vault.

**It forces a plpgsql wrapper around every call.** You cannot `SET` inside a plain SQL statement, so every application entry point becomes a function whose body is `set_config(...)` followed by the call you actually wanted. All the boilerplate in the users/articles example is this and only this.

The RFC's own **Alternatives considered** section lists ten rejected options for D2–D8 — monolithic source column, container rows, absolute spans, eager anchor resolution, nullable `vault_id`, vault registry, per-revision edges, fractional ordering, triggers, FTS-now — each with a reason. **An explicit `vault_id` parameter is not among them.** It wasn't weighed and rejected. It wasn't considered.

Worth noting what D1 *does* get right: the trust-model paragraph is unusually honest ("a userset GUC… provides vault *scoping*… and is a tenant *boundary* only when a trusted layer owns the connection"), and `pgmind.enable_vault_rls()` was added after review precisely because prose wasn't enough. The security thinking is careful. The *composability* thinking never happened.

---

## 5. Symptom 2 — no note identity in the public API

A note's identity is `(vault_id, path)`. Both halves are awkward: `vault_id` is ambient (§4), and `path` is mutable text.

- `knowledge.write()` returns a **revision** uuid — a fact about an event, not a handle on the note.
- No public function returns a note id. `knowledge.notes()` returns `path`, `head_revision`, and metadata; `blocks()` returns block ids; `backlinks()` returns paths.
- The unique index is **partial** — `note_live_path ON pgmind.note (vault_id, path) WHERE tombstoned_at IS NULL` — so it cannot be a foreign-key target even for someone willing to key on text. (The partiality itself is correct; tombstoned notes must be able to share a path with a live one. It's a downstream cost, not an error.)

So an application's link to a note is a bare text column: no referential integrity, no cascade, no rename propagation. Delete an article row and the note survives unless you wrote a trigger. Rename a note and your table silently points at nothing.

The sharpest illustration is internal: `pgmind.verify_note(note_id uuid)` — the health-check function — **takes an argument the public API cannot produce.** You must query the storage schema directly to use the tool that checks the storage schema.

---

## 6. Symptom 3 — the advertised scenario returns a wrong answer

Not a limitation. A wrong result, silently, with no error. Verified on PostgreSQL 18.4 with pgmind 0.0.1:

```sql
CREATE TABLE app_user (handle text PRIMARY KEY,
                       vault_id uuid UNIQUE NOT NULL DEFAULT gen_random_uuid());
INSERT INTO app_user (handle) VALUES ('alice'),('bob');

-- alice writes a1 in her vault, bob writes b1 in his
SELECT set_config('pgmind.vault_id', (SELECT vault_id::text FROM app_user WHERE handle='alice'), false);
SELECT knowledge.write('a1', '# Alice one'::markdown);
SELECT set_config('pgmind.vault_id', (SELECT vault_id::text FROM app_user WHERE handle='bob'), false);
SELECT knowledge.write('b1', '# Bob one'::markdown);
```

Now ask the scenario-4 question — every user with their notes:

```sql
SELECT set_config('pgmind.vault_id', (SELECT vault_id::text FROM app_user WHERE handle='alice'), false);

SELECT u.handle, n.path FROM app_user u, LATERAL knowledge.notes() n ORDER BY u.handle;
```

```
 handle | path
--------+------
 alice  | a1
 bob    | a1     ← Bob is reported as owning Alice's note
```

Ground truth, reading the storage table directly:

```sql
SELECT u.handle, n.path FROM app_user u JOIN pgmind.note n ON n.vault_id = u.vault_id;
--  alice | a1
--  bob   | b1
```

The obvious rescue does not work either — `set_config` in a `LATERAL` returns the same wrong rows, because nothing constrains its evaluation relative to the set-returning function:

```sql
SELECT u.handle, n.path
FROM app_user u
CROSS JOIN LATERAL (SELECT set_config('pgmind.vault_id', u.vault_id::text, true)) s
CROSS JOIN LATERAL knowledge.notes() n;
--  alice | a1
--  bob   | a1     ← still wrong
```

There is no formulation of this query in the public API that is correct. The only correct answer today is to bypass `knowledge.*` and read `pgmind.note` — the internal schema, whose shape RFC-003 §4 reserves the right to change.

This is worse than an ergonomic complaint. A tenancy-scoped API whose failure mode is *quietly attributing one tenant's data to another* is the failure mode you least want. It fails open, in a query shape the handbook advertises.

---

## 7. Symptom 4 — authorship is process identity

`pgmind.revision.author text NOT NULL DEFAULT current_user`, and no write function takes an author.

This is the filesystem substitution again: the file's owner is whoever ran the process. It's right when one database role is one human. It's wrong for "server-side AI applications and agent systems" (RFC-000 D3), which is the case RFC-000 §1 opens by describing — those run a pooled application role, so `knowledge.history()` and `knowledge.blame()` report that role for every user in the system.

`SET LOCAL ROLE` works, and blame follows it correctly (verified). But it requires mapping every end user to a database role, which is a heavy structural commitment to buy back a column. The alternative is to keep your own attribution table, at which point pgmind's blame — a headline feature, scenario 5 — is decorative for your application.

---

## 8. What is *not* the problem

Being fair about the boundary of the complaint:

**Vault-wide basename link resolution** (RFC-002 D8) is correct. `[[handbook]]` resolving across the whole vault is what a wiki-link *means*. That per-user path prefixes in one shared vault don't isolate users — Bob creating `users/bob/handbook` makes Alice's `[[handbook]]` ambiguous — is real, and it's a trap I fell into, but D1 does say multi-tenant callers get a vault each. That's a documentation gap, not a design error.

**The two-lane storage model, tile-relative spans, deferred fractional ordering, no FK on `head_revision`** — all have rationale in RFC-003 §3 that holds up, several written after adversarial review found the naive version broken.

**The `markdown` type itself** is a good boundary type: it validates UTF-8 and enforces `pgmind.max_document_bytes` at write time, which `text` cannot. That it has no equality operator and no btree opclass is a *little* annoying (no `UNIQUE`, no `ORDER BY`, no index without a cast) and would cost one `CREATE OPERATOR CLASS` to fix, but it isn't structural.

---

## 9. The proof that D1 is the root cause

Here is the whole users/articles integration if `vault_id` were a parameter and `author` were a parameter:

```sql
CREATE TABLE app_user (
  id       uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  handle   text UNIQUE NOT NULL,
  vault_id uuid UNIQUE NOT NULL DEFAULT gen_random_uuid()
);

CREATE TABLE article (
  id        uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  author_id uuid NOT NULL REFERENCES app_user(id) ON DELETE CASCADE,
  slug      text NOT NULL,
  head      uuid NOT NULL,
  UNIQUE (author_id, slug)
);
```

Write — one statement, no wrapper:

```sql
INSERT INTO article (author_id, slug, head)
SELECT u.id, 'onboarding',
       knowledge.write('onboarding', $md$…$md$, vault_id => u.vault_id, author => u.handle)
FROM app_user u WHERE u.handle = 'alice';
```

Read every user's notes — the query that is wrong today:

```sql
SELECT u.handle, n.path, n.properties->>'title'
FROM app_user u, LATERAL knowledge.notes(vault_id => u.vault_id) n;
```

Two plpgsql wrapper functions, one `set_config` per entry point, and the tenancy footgun all disappear. **The boilerplate was the missing parameter, written out by hand at every call site.** That is what identifies D1 as the cause rather than a symptom.

---

## 10. What it would take

Ranked by cost. Nothing here changes a table shape, so nothing here is what RFC-003 §4 calls an expensive reversal — these are function signatures, and Postgres default arguments make all of them purely additive. Existing GUC-based callers keep working unchanged.

1. **Optional `vault_id uuid DEFAULT NULL` on every `knowledge.*` function**, falling back to the GUC when omitted. Fixes §4 and §6. The GUC stays the ergonomic default for MCP sessions; the parameter makes the functions composable. This is the one that matters.

2. **Expose note identity.** Add `note_id uuid` to `knowledge.notes()`, and a `knowledge.note_id(path text) → uuid`. Lets applications key on something stable across renames, and makes `pgmind.verify_note` reachable without touching the storage schema.

3. **Optional `author text DEFAULT NULL` on write functions**, defaulting to `current_user`. Makes blame and history usable under a pooled application role.

4. **Document the tenancy model in the user-facing docs**, not only in RFC-003 D1: one vault per tenant, and *why* path prefixes are not a substitute (§8).

5. *(Larger, needs its own RFC.)* A stable identity applications can actually foreign-key to, and a documented cascade story. This is the one that requires real design work rather than a signature change.

6. *(Small, optional.)* A btree opclass on `markdown` so it can be indexed and sorted without `::text`.

---

## 11. The one-line answer

> It's complicated because pgmind models a vault — path identity, ambient namespace, process attribution — and you are trying to model a relation. Both consumers are named in RFC-000 D3; only the vault got designed. The specific decision that turned that gap into per-call-site boilerplate is RFC-003 D1's "function signatures stay path-only", and the same decision makes the handbook's own scenario-4 query return the wrong tenant's data.

The timing is the good news: this is pre-0.1.0, the fix for the load-bearing item is an optional parameter, and there is no compatibility promise to break yet. It is a cheap fix now and an expensive apology later.
