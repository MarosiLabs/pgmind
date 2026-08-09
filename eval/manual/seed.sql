-- The demo vault every example in the website manual is written against.
--
-- This file is the manual's ground truth. `eval/manual_gate.py` rebuilds a database
-- from it and re-runs every SQL block in website/docs/*.html against the result, so
-- an example that stops working fails the gate instead of rotting in place.
--
-- Seven notes, chosen so that each one earns its place in the manual:
--
--   projects/auth        the main example: frontmatter, headings, a bulleted
--                        Decisions section, a ## Log, wiki-links, tags, one ^id
--   rfc/auth-01          a link source, so backlinks('projects/auth') is non-empty
--   runbooks/rotate-keys the "join knowledge to your own tables" recipe
--   people/ada           the excision example (a path that is itself identifying)
--   daily/2026-08-06     append-to-section and agent-log examples
--   index                a hub linking to everything, so orphans() is meaningful
--   notes/scratch        ambiguous-basename and dangling-link demos
--
-- Invariant, asserted at the bottom of this file: knowledge.stats() reports
-- 7 notes, 48 blocks, 16 resolved edges, 2 dangling, 9 tags, 7 revisions, 1383 bytes.
-- Those numbers are quoted verbatim in the manual. If an edit here moves them,
-- either the edit is wrong or the pages that quote them must change with it.

CREATE EXTENSION IF NOT EXISTS pgmind;

SELECT knowledge.write('projects/auth', $md$---
title: Auth
tags: [architecture, active]
owner: platform
---

# Auth

The service that issues and rotates credentials. Owned by [[people/ada]].

## Decisions

- [[oauth2]] over server sessions, see [[rfc/auth-01]] #architecture
- Refresh tokens rotate every 24h #decision ^rotation
- Access tokens live 15 minutes, no exceptions

## Runbook

Key rotation is [[runbooks/rotate-keys|documented separately]].

## Log

Migrated the session store.
$md$);

SELECT knowledge.write('rfc/auth-01', $md$# RFC-AUTH-01: token rotation

Rotation policy for [[projects/auth]]. #architecture #decision

The operational steps live in the [rotation runbook](runbooks/rotate-keys.md).
$md$);

SELECT knowledge.write('runbooks/rotate-keys', $md$---
tags: runbook
---

# Rotate the signing keys

1. Announce the maintenance window
2. Generate the new key pair
3. Roll [[projects/auth]] to the new key id
4. Retire the old key once every token has expired

> Retiring the old key early invalidates live sessions.
$md$);

SELECT knowledge.write('people/ada', $md$# Ada Lovelace

Platform lead, reachable at ada@example.com.

Owns [[projects/auth]] and [[runbooks/rotate-keys]]. #person
$md$);

SELECT knowledge.write('daily/2026-08-06', $md$# 2026-08-06

## Log

Reviewed the rotation window with [[people/ada]]. #decision
$md$);

SELECT knowledge.write('notes/scratch', $md$# Scratch

A bare [[auth]] resolves by basename, because exactly one note ends in that segment.

A link to [[nowhere]] dangles, and stays a first-class row.
$md$);

SELECT knowledge.write('index', $md$# Index

- [[projects/auth]]
- [[rfc/auth-01]]
- [[runbooks/rotate-keys]]
- [[people/ada]]
- [[daily/2026-08-06]]
- [[notes/scratch]]
$md$);

-- The invariant. Fails loudly rather than seeding a vault the manual does not describe.
DO $seed$
DECLARE s record;
BEGIN
  SELECT * INTO s FROM knowledge.stats();
  IF (s.notes, s.blocks, s.edges_resolved, s.edges_dangling, s.tags, s.revisions, s.bytes)
     IS DISTINCT FROM (7::bigint, 48::bigint, 16::bigint, 2::bigint, 9::bigint, 7::bigint, 1383::bigint)
  THEN
    RAISE EXCEPTION 'seed vault does not match the manual: got % notes, % blocks, % resolved, % dangling, % tags, % revisions, % bytes',
      s.notes, s.blocks, s.edges_resolved, s.edges_dangling, s.tags, s.revisions, s.bytes;
  END IF;
END
$seed$;
