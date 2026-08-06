//! Phase 2 gate tests: identity-semantics (RFC-004 §5), extraction lifecycle
//! (RFC-003 §5 suite 2), storage round-trip, and op error contracts. These run
//! inside a real Postgres via `cargo pgrx test` and are executed by the eval
//! harness as the `identity-semantics` / `extraction-correctness` suites.

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::Uuid;

    fn write(path: &str, md: &str) -> Uuid {
        Spi::get_one_with_args(
            "SELECT knowledge.write($1, $2::markdown)",
            &[path.into(), md.into()],
        )
        .expect("write failed")
        .expect("write returned NULL")
    }

    fn read(path: &str) -> String {
        Spi::get_one_with_args("SELECT knowledge.read($1)::text", &[path.into()])
            .expect("read failed")
            .expect("read returned NULL")
    }

    fn block_ids(path: &str) -> Vec<(String, Uuid)> {
        Spi::connect(|client| {
            client
                .select(
                    "SELECT content, block_id FROM knowledge.blocks($1) ORDER BY ord",
                    None,
                    &[path.into()],
                )
                .expect("blocks failed")
                .map(|row| {
                    (
                        row.get::<String>(1).unwrap().unwrap(),
                        row.get::<Uuid>(2).unwrap().unwrap(),
                    )
                })
                .collect()
        })
    }

    fn verify_clean(path: &str) {
        let violations: Option<i64> = Spi::get_one_with_args(
            "SELECT count(*) FROM pgmind.verify_note(
               (SELECT id FROM pgmind.note WHERE path = $1 AND tombstoned_at IS NULL))",
            &[path.into()],
        )
        .expect("verify failed");
        assert_eq!(
            violations,
            Some(0),
            "verify_note found violations for {path}"
        );
    }

    /// Run SQL, returning the SQLSTATE it fails with ('00000' if it succeeds).
    fn sqlstate_of(sql: &str) -> String {
        Spi::run(
            "CREATE OR REPLACE FUNCTION pg_temp.pgmind_catch(sql text) RETURNS text
             LANGUAGE plpgsql AS $f$
             DECLARE state text := '00000';
             BEGIN
               BEGIN
                 EXECUTE sql;
               EXCEPTION WHEN OTHERS THEN
                 GET STACKED DIAGNOSTICS state = RETURNED_SQLSTATE;
               END;
               RETURN state;
             END $f$;",
        )
        .expect("helper failed");
        Spi::get_one_with_args("SELECT pg_temp.pgmind_catch($1)", &[sql.into()])
            .expect("catch failed")
            .expect("catch NULL")
    }

    // ---------- storage round-trip & idempotence ----------

    #[pg_test]
    fn write_read_byte_faithful() {
        let md = "---\ntitle: X\n---\n\n# A\n\npara with [[link]] and #tag\n\n- item one\n- item two ^anchor\n\n> quoted\n";
        write("notes/roundtrip", md);
        assert_eq!(read("notes/roundtrip"), md);
        verify_clean("notes/roundtrip");
    }

    #[pg_test]
    fn idempotent_write_returns_head_no_new_revision() {
        let r1 = write("idem", "# T\n\nbody\n");
        let revs_before: i64 = Spi::get_one("SELECT count(*) FROM pgmind.revision")
            .unwrap()
            .unwrap();
        let r2 = write("idem", "# T\n\nbody\n");
        let revs_after: i64 = Spi::get_one("SELECT count(*) FROM pgmind.revision")
            .unwrap()
            .unwrap();
        assert_eq!(r1, r2, "idempotent write must return the same head");
        assert_eq!(revs_before, revs_after, "no new revision row");
        let ids1 = block_ids("idem");
        write("idem", "# T\n\nbody\n");
        assert_eq!(ids1, block_ids("idem"), "IDs stable across no-op writes");
    }

    // ---------- A3 carry ----------

    #[pg_test]
    fn edited_paragraph_mints_untouched_carry() {
        write("carry", "# H\n\nalpha\n\nbeta\n");
        let before = block_ids("carry");
        write("carry", "# H\n\nalpha CHANGED\n\nbeta\n");
        let after = block_ids("carry");
        // heading + beta carried, alpha minted
        assert_eq!(before[0].1, after[0].1, "heading carried");
        assert_eq!(before[2].1, after[2].1, "beta carried");
        assert_ne!(
            before[1].1, after[1].1,
            "edited paragraph mints (Phase 2 pinned behavior)"
        );
        verify_clean("carry");
    }

    #[pg_test]
    fn pure_reorder_carries_all() {
        write("reorder", "alpha\n\nbeta\n\ngamma\n");
        let before = block_ids("reorder");
        write("reorder", "gamma\n\nalpha\n\nbeta\n");
        let after = block_ids("reorder");
        for (content, id) in &before {
            let found = after.iter().find(|(c, _)| c == content).unwrap();
            assert_eq!(*id, found.1, "reordered block {content} keeps its ID");
        }
        verify_clean("reorder");
    }

    #[pg_test]
    fn duplicate_content_pairs_kth_to_kth() {
        write("dups", "same\n\nsame\n\nother\n");
        let before = block_ids("dups");
        assert_eq!(before[0].0, before[1].0);
        assert_ne!(
            before[0].1, before[1].1,
            "copies get distinct IDs, one hash"
        );
        // rewrite with one duplicate removed: first occurrence pairs first
        write("dups", "same\n\nother\n");
        let after = block_ids("dups");
        assert_eq!(
            before[0].1, after[0].1,
            "k-th ↔ k-th pairing keeps the first"
        );
        verify_clean("dups");
    }

    #[pg_test]
    fn ref_claim_beats_hash_and_collisions_resolve() {
        write("claims", "one ^a\n\ntwo\n");
        let before = block_ids("claims");
        // Move the ^a marker onto different content: the claim carries the ID
        // even though the hash changed.
        write("claims", "totally new ^a\n\ntwo\n");
        let after = block_ids("claims");
        assert_eq!(
            before[0].1, after[0].1,
            "^id claim carries across content change"
        );
        assert_eq!(before[1].1, after[1].1, "hash carries the rest");
        verify_clean("claims");
    }

    #[pg_test]
    fn kind_change_via_claim() {
        write("kindclaim", "plain para ^k\n");
        let before = block_ids("kindclaim");
        write("kindclaim", "# now a heading ^k\n");
        let after = block_ids("kindclaim");
        assert_eq!(before[0].1, after[0].1, "claim carries across kind change");
        verify_clean("kindclaim");
    }

    // ---------- extraction & resolution lifecycle ----------

    #[pg_test]
    fn resolution_lifecycle_missing_then_resolved() {
        write("src/a", "see [[target-note]]\n");
        let reason: Option<String> = Spi::get_one_with_args(
            "SELECT dangling_reason FROM knowledge.links($1)",
            &["src/a".into()],
        )
        .unwrap();
        assert_eq!(reason.as_deref(), Some("missing"));
        write("dir/target-note", "content\n");
        let resolved: Option<String> = Spi::get_one_with_args(
            "SELECT resolved_path FROM knowledge.links($1)",
            &["src/a".into()],
        )
        .unwrap();
        assert_eq!(
            resolved.as_deref(),
            Some("dir/target-note"),
            "basename resolution"
        );
        // A second note with the same basename demotes to ambiguous.
        write("other/target-note", "x\n");
        let reason: Option<String> = Spi::get_one_with_args(
            "SELECT dangling_reason FROM knowledge.links($1)",
            &["src/a".into()],
        )
        .unwrap();
        assert_eq!(
            reason.as_deref(),
            Some("ambiguous"),
            "demotion on collision"
        );
        // An exact-path note wins over both (promotion, frozen D8).
        write("target-note", "root\n");
        let resolved: Option<String> = Spi::get_one_with_args(
            "SELECT resolved_path FROM knowledge.links($1)",
            &["src/a".into()],
        )
        .unwrap();
        assert_eq!(
            resolved.as_deref(),
            Some("target-note"),
            "exact beats basename"
        );
        verify_clean("src/a");
    }

    #[pg_test]
    fn backlinks_tags_orphans() {
        write("hub", "# Hub\n\ncontent #core\n");
        write("spoke", "see [[hub]] #core ok\n");
        let bl: Option<String> = Spi::get_one_with_args(
            "SELECT src_path FROM knowledge.backlinks($1)",
            &["hub".into()],
        )
        .unwrap();
        assert_eq!(bl.as_deref(), Some("spoke"));
        let tagged: Option<i64> =
            Spi::get_one("SELECT count(*) FROM knowledge.tagged('CORE')").unwrap();
        assert_eq!(tagged, Some(2), "case-insensitive tag match");
        let orphans: Vec<String> = Spi::connect(|client| {
            client
                .select("SELECT path FROM knowledge.orphans()", None, &[])
                .unwrap()
                .map(|r| r.get::<String>(1).unwrap().unwrap())
                .collect()
        });
        assert!(
            orphans.contains(&"spoke".to_string()),
            "spoke has no incoming links"
        );
        assert!(!orphans.contains(&"hub".to_string()), "hub is linked");
    }

    #[pg_test]
    fn churn_discipline_one_paragraph_edit() {
        write("churn", "# H\n\nalpha\n\nbeta\n\ngamma\n");
        let xmins_before: Vec<(i32, String)> = Spi::connect(|client| {
            client
                .select(
                    "SELECT b.ord, b.ctid::text FROM pgmind.block b
                     JOIN pgmind.note n ON n.id = b.note_id
                     WHERE n.path = 'churn' ORDER BY b.ord",
                    None,
                    &[],
                )
                .unwrap()
                .map(|r| (r.get(1).unwrap().unwrap(), r.get(2).unwrap().unwrap()))
                .collect()
        });
        write("churn", "# H\n\nalpha\n\nbeta EDITED\n\ngamma\n");
        let xmins_after: Vec<(i32, String)> = Spi::connect(|client| {
            client
                .select(
                    "SELECT b.ord, b.ctid::text FROM pgmind.block b
                     JOIN pgmind.note n ON n.id = b.note_id
                     WHERE n.path = 'churn' ORDER BY b.ord",
                    None,
                    &[],
                )
                .unwrap()
                .map(|r| (r.get(1).unwrap().unwrap(), r.get(2).unwrap().unwrap()))
                .collect()
        });
        // ords 0 (heading), 1 (alpha), 3 (gamma) untouched; ord 2 replaced
        for ord in [0usize, 1, 3] {
            assert_eq!(
                xmins_before[ord].1, xmins_after[ord].1,
                "row at ord {ord} must be physically untouched (ctid stable)"
            );
        }
        assert_ne!(xmins_before[2].1, xmins_after[2].1);
    }

    // ---------- read_section ----------

    #[pg_test]
    fn read_section_first_match() {
        write(
            "sections",
            "# Top\n\nintro\n\n## Sub\n\nsub content\n\n## Sub2\n\nother\n",
        );
        let sec: String = Spi::get_one_with_args(
            "SELECT knowledge.read_section($1, ARRAY['Top','Sub'])::text",
            &["sections".into()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(sec, "## Sub\n\nsub content\n\n");
        assert_eq!(
            sqlstate_of("SELECT knowledge.read_section('sections', ARRAY['Nope'])"),
            "PM007"
        );
    }

    // ---------- block ops ----------

    #[pg_test]
    fn update_block_keeps_id_changes_hash() {
        write("ops/upd", "alpha\n\nbeta\n");
        let before = block_ids("ops/upd");
        let target = before[0].1;
        let (rev, ids): (Option<Uuid>, Option<Vec<Uuid>>) = Spi::get_two_with_args(
            "SELECT (r).revision, (r).block_ids
             FROM (SELECT knowledge.update_block($1, 'alpha NEW'::markdown) AS r) s",
            &[target.into()],
        )
        .unwrap();
        assert!(rev.is_some());
        assert_eq!(ids.unwrap(), vec![target], "op returns the targeted block");
        let after = block_ids("ops/upd");
        assert_eq!(after[0].1, target, "ID kept");
        assert_eq!(after[0].0, "alpha NEW");
        assert_eq!(after[1].1, before[1].1, "sibling untouched");
        assert_eq!(read("ops/upd"), "alpha NEW\n\nbeta\n");
        verify_clean("ops/upd");
    }

    #[pg_test]
    fn update_item_checkbox_toggle() {
        write("ops/task", "- [ ] todo one\n- [ ] todo two\n");
        let before = block_ids("ops/task");
        // items are blocks 0 and 2 (each item has an inner paragraph)
        let item = before[0].1;
        Spi::run_with_args(
            "SELECT knowledge.update_block($1, '- [x] todo one'::markdown)",
            &[item.into()],
        )
        .unwrap();
        assert_eq!(read("ops/task"), "- [x] todo one\n- [ ] todo two\n");
        let after = block_ids("ops/task");
        assert_eq!(after[0].1, item, "item ID kept across checkbox toggle");
        verify_clean("ops/task");
    }

    #[pg_test]
    fn update_inner_paragraph_directly() {
        write("ops/inner", "- hello world\n");
        let before = block_ids("ops/inner");
        let (item_id, para_id) = (before[0].1, before[1].1);
        let item_hash_before: Vec<u8> = Spi::get_one_with_args(
            "SELECT content_hash FROM knowledge.blocks($1) WHERE ord = 0",
            &["ops/inner".into()],
        )
        .unwrap()
        .unwrap();
        Spi::run_with_args(
            "SELECT knowledge.update_block($1, 'goodbye world'::markdown)",
            &[para_id.into()],
        )
        .unwrap();
        let after = block_ids("ops/inner");
        assert_eq!(after[0].1, item_id, "enclosing item ID kept");
        assert_eq!(after[1].1, para_id, "paragraph ID kept");
        let item_hash_after: Vec<u8> = Spi::get_one_with_args(
            "SELECT content_hash FROM knowledge.blocks($1) WHERE ord = 0",
            &["ops/inner".into()],
        )
        .unwrap()
        .unwrap();
        assert_ne!(item_hash_before, item_hash_after, "item hash recomputed");
        assert_eq!(read("ops/inner"), "- goodbye world\n");
        verify_clean("ops/inner");
    }

    #[pg_test]
    fn move_block_separator_synthesis() {
        write("ops/move", "alpha\n\nbeta\n\ngamma\n");
        let before = block_ids("ops/move");
        let gamma = before[2].1;
        let alpha = before[0].1;
        Spi::run_with_args(
            "SELECT knowledge.move_block($1, before => $2)",
            &[gamma.into(), alpha.into()],
        )
        .unwrap();
        assert_eq!(
            read("ops/move"),
            "gamma\n\nalpha\n\nbeta\n",
            "no paragraph merging"
        );
        let after = block_ids("ops/move");
        assert_eq!(after[0].1, gamma);
        assert_eq!(after[1].1, alpha);
        verify_clean("ops/move");
    }

    #[pg_test]
    fn move_last_block_earlier_and_back() {
        write("ops/move2", "alpha\n\nbeta\n");
        let before = block_ids("ops/move2");
        let beta = before[1].1;
        Spi::run_with_args(
            "SELECT knowledge.move_block($1, after => $2)",
            &[before[0].1.into(), beta.into()],
        )
        .unwrap();
        assert_eq!(read("ops/move2"), "beta\n\nalpha\n");
        verify_clean("ops/move2");
    }

    #[pg_test]
    fn insert_blocks_at_end_and_anchored() {
        write("ops/ins", "alpha\n");
        let rev_ids: Option<Vec<Uuid>> = Spi::get_one_with_args(
            "SELECT (knowledge.insert_blocks($1, 'beta\n\ngamma'::markdown)).block_ids",
            &["ops/ins".into()],
        )
        .unwrap();
        assert_eq!(rev_ids.map(|v| v.len()), Some(2), "two blocks minted");
        assert_eq!(read("ops/ins"), "alpha\n\nbeta\n\ngamma\n");
        let ids = block_ids("ops/ins");
        Spi::run_with_args(
            "SELECT knowledge.insert_blocks($1, 'zeta'::markdown, before => $2)",
            &["ops/ins".into(), ids[0].1.into()],
        )
        .unwrap();
        assert_eq!(read("ops/ins"), "zeta\n\nalpha\n\nbeta\n\ngamma\n");
        verify_clean("ops/ins");
    }

    #[pg_test]
    fn split_first_keeps_id() {
        write("ops/split", "one two\n\ntail\n");
        let before = block_ids("ops/split");
        let target = before[0].1;
        let ids: Option<Vec<Uuid>> = Spi::get_one_with_args(
            "SELECT (knowledge.split_block($1, 'one\n\ntwo'::markdown)).block_ids",
            &[target.into()],
        )
        .unwrap();
        let ids = ids.unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], target, "first fragment keeps the ID");
        assert_ne!(ids[1], target);
        assert_eq!(read("ops/split"), "one\n\ntwo\n\ntail\n");
        let after = block_ids("ops/split");
        assert_eq!(after[2].1, before[1].1, "tail carried");
        verify_clean("ops/split");
    }

    #[pg_test]
    fn merge_keeps_chosen_id() {
        write("ops/merge", "one\n\ntwo\n\ntail\n");
        let before = block_ids("ops/merge");
        let (a, b) = (before[0].1, before[1].1);
        let ids: Option<Vec<Uuid>> = Spi::get_one_with_args(
            "SELECT (knowledge.merge_blocks(ARRAY[$1, $2], 'one two'::markdown, keep => $2)).block_ids",
            &[a.into(), b.into()],
        )
        .unwrap();
        assert_eq!(ids.unwrap(), vec![b], "keep survives");
        assert_eq!(read("ops/merge"), "one two\n\ntail\n");
        let after = block_ids("ops/merge");
        assert_eq!(after[0].1, b);
        assert!(!after.iter().any(|(_, id)| *id == a), "retiree removed");
        verify_clean("ops/merge");
    }

    /// RFC-003 D6: the final tile keeps exactly the trailing trivia it had.
    /// A note whose last block has no trailing newline is legal, byte-faithful
    /// storage — `update_block`, `split_block` and `merge_blocks` used to
    /// invent one, and `verify_note` cannot see it (the recomputed parse agrees
    /// with the rewritten bytes; only the caller's original bytes disagree).
    #[pg_test]
    fn final_block_ops_keep_trailing_trivia() {
        let upd = |path: &str, id: Uuid, frag: &str| {
            Spi::run_with_args(
                "SELECT knowledge.update_block($1, $2::markdown)",
                &[id.into(), frag.into()],
            )
            .unwrap_or_else(|e| panic!("update_block on {path} failed: {e}"));
        };

        write("ops/eof", "alpha\n\nbeta");
        assert_eq!(read("ops/eof"), "alpha\n\nbeta", "write is byte-faithful");
        upd("ops/eof", block_ids("ops/eof")[1].1, "BETA");
        assert_eq!(read("ops/eof"), "alpha\n\nBETA", "update invented no byte");
        verify_clean("ops/eof");

        write("ops/eofsplit", "alpha\n\nbeta");
        Spi::run_with_args(
            "SELECT knowledge.split_block($1, 'b1\n\nb2'::markdown)",
            &[block_ids("ops/eofsplit")[1].1.into()],
        )
        .expect("split failed");
        assert_eq!(read("ops/eofsplit"), "alpha\n\nb1\n\nb2");
        verify_clean("ops/eofsplit");

        write("ops/eofmerge", "alpha\n\nbeta");
        let m = block_ids("ops/eofmerge");
        Spi::run_with_args(
            "SELECT knowledge.merge_blocks(ARRAY[$1, $2], 'MERGED'::markdown)",
            &[m[0].1.into(), m[1].1.into()],
        )
        .expect("merge failed");
        assert_eq!(read("ops/eofmerge"), "MERGED");
        verify_clean("ops/eofmerge");

        // The other direction of the same rule: a note that DOES end in a
        // newline still ends in exactly one afterwards.
        write("ops/eofkeep", "alpha\n\nbeta\n");
        upd("ops/eofkeep", block_ids("ops/eofkeep")[1].1, "BETA");
        assert_eq!(read("ops/eofkeep"), "alpha\n\nBETA\n");
        verify_clean("ops/eofkeep");

        // ...and a mid-note target still gets its mandatory terminator, or the
        // replacement would merge into the block after it.
        write("ops/eofmid", "alpha\n\nbeta\n\ngamma");
        upd("ops/eofmid", block_ids("ops/eofmid")[0].1, "ALPHA");
        assert_eq!(read("ops/eofmid"), "ALPHA\n\nbeta\n\ngamma");
        verify_clean("ops/eofmid");
    }

    /// The batched lanes flush every `LANE_CHUNK_ROWS` (2048) rows and pair
    /// each block with its content by ordinality within the chunk, so both the
    /// chunk boundary and the pairing need a note bigger than one chunk to be
    /// exercised at all. The leading paragraph makes the item/paragraph pairs
    /// start at an odd index, which puts a parent and its child on opposite
    /// sides of the boundary — the case that relies on the FK being checked at
    /// end-of-statement and on `doc.blocks` being pre-order.
    #[pg_test]
    fn lane_batching_survives_chunk_boundaries() {
        let mut md = String::from("lead\n\n");
        for i in 0..3000 {
            md.push_str(&format!("- item {i}\n"));
        }
        write("bulk/items", &md);
        assert_eq!(read("bulk/items"), md, "byte-faithful across chunks");
        let ids = block_ids("bulk/items");
        assert_eq!(ids.len(), 1 + 3000 * 2, "item + inner paragraph per line");
        assert!(
            ids.iter()
                .map(|(_, id)| id)
                .collect::<std::collections::HashSet<_>>()
                .len()
                == ids.len(),
            "no id collisions across chunks"
        );
        verify_clean("bulk/items");

        // Rewrite: exercises the batched UPDATE lane (ords shift by one for
        // every block after the edit, across all three chunks) and the carry.
        let edited = md.replace("- item 2999\n", "- item 2999 edited\n");
        write("bulk/items", &edited);
        assert_eq!(read("bulk/items"), edited);
        let after = block_ids("bulk/items");
        assert_eq!(after.len(), ids.len());
        assert_eq!(after[0].1, ids[0].1, "untouched lead paragraph carried");
        assert_eq!(after[4000].1, ids[4000].1, "untouched middle block carried");
        verify_clean("bulk/items");

        // A block whose content is far larger than any jsonb row travels as
        // text[], which has no per-element ceiling (the 256 MB jsonb string
        // limit itself is out of reach of a test that has to run in CI).
        let big = format!("intro\n\n```\n{}```\n", "x".repeat(2 * 1024 * 1024));
        write("bulk/big", &big);
        assert_eq!(read("bulk/big"), big, "multi-MB block round-trips");
        verify_clean("bulk/big");
    }

    #[pg_test]
    fn child_carried_while_parent_removed() {
        write("ops/delist", "- hello\n");
        let before = block_ids("ops/delist");
        let para = before[1].1;
        write("ops/delist", "hello\n");
        let after = block_ids("ops/delist");
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].1, para, "paragraph carried by hash, item removed");
        verify_clean("ops/delist");
    }

    // ---------- Phase 3 foundations (RFC-005 D2) ----------

    /// RFC-005 D2: every pgmind table must be registered for dump, and every
    /// table carrying `vault_id` must be inside the tenant boundary — both
    /// checked against `pg_catalog`, never against a list someone maintains.
    ///
    /// This is the shape of the review's fifth critical finding: with
    /// `pg_extension_config_dump` missing for `block_revision` alone, every
    /// assertion the Phase 2 dump-restore suite makes stays green while 100% of
    /// per-block history vanishes from the backup. A literal table list in a
    /// test cannot catch that, because the list and the omission are the same
    /// mistake.
    #[pg_test]
    fn every_pgmind_table_is_dumped_and_tenant_scoped() {
        let unregistered: Vec<String> = Spi::connect(|client| {
            client
                .select(
                    "SELECT c.relname::text
                       FROM pg_class c
                       JOIN pg_namespace n ON n.oid = c.relnamespace
                      WHERE n.nspname = 'pgmind' AND c.relkind = 'r'
                        AND c.oid <> 'pgmind.excision_replay'::regclass
                        AND NOT EXISTS (
                          SELECT 1 FROM pg_extension e
                           WHERE e.extname = 'pgmind' AND c.oid = ANY(e.extconfig))
                      ORDER BY 1",
                    None,
                    &[],
                )
                .expect("catalog query failed")
                .map(|row| row.get::<String>(1).unwrap().unwrap())
                .collect()
        });
        assert!(
            unregistered.is_empty(),
            "tables missing pg_extension_config_dump (their rows would not survive pg_dump): {unregistered:?}"
        );

        // excision_replay must be the one deliberate exception: it holds the
        // executable excision target, which for the literal and note forms IS
        // the identifying data the excision erased (D2 H4b).
        let replay_registered: Option<bool> = Spi::get_one(
            "SELECT EXISTS (SELECT 1 FROM pg_extension e
                             WHERE e.extname = 'pgmind'
                               AND 'pgmind.excision_replay'::regclass = ANY(e.extconfig))",
        )
        .unwrap();
        assert_eq!(
            replay_registered,
            Some(false),
            "excision_replay must NOT be dump-registered"
        );

        // Policy coverage is asserted in `tenant_scoping_and_grant_boundary`,
        // which is the one test that enables RLS: ALTER TABLE takes ACCESS
        // EXCLUSIVE, and two tests doing that concurrently in different table
        // orders deadlock (observed, not theorised).
    }

    /// Every `vault_id`-carrying table is inside the boundary the shipped
    /// helper establishes. Enumerated from `pg_catalog` on both sides.
    fn assert_every_vault_table_has_a_policy() {
        let unprotected: Vec<String> = Spi::connect(|client| {
            client
                .select(
                    "SELECT c.relname::text
                       FROM pg_class c
                       JOIN pg_namespace n ON n.oid = c.relnamespace
                      WHERE n.nspname = 'pgmind' AND c.relkind = 'r'
                        AND EXISTS (SELECT 1 FROM pg_attribute a
                                     WHERE a.attrelid = c.oid AND a.attname = 'vault_id'
                                       AND a.attnum > 0 AND NOT a.attisdropped)
                        AND NOT EXISTS (SELECT 1 FROM pg_policies p
                                         WHERE p.schemaname = 'pgmind'
                                           AND p.tablename = c.relname
                                           AND p.policyname = 'vault_isolation')
                      ORDER BY 1",
                    None,
                    &[],
                )
                .expect("policy query failed")
                .map(|row| row.get::<String>(1).unwrap().unwrap())
                .collect()
        });
        assert!(
            unprotected.is_empty(),
            "tables with vault_id but no vault_isolation policy (cross-tenant reads): {unprotected:?}"
        );
    }

    /// RFC-005 D3/D4: every revision carries a dense per-note seq and a verb.
    #[pg_test]
    fn revisions_carry_dense_seq_and_verb() {
        write("hist/seq", "alpha\n\nbeta\n");
        write("hist/seq", "alpha\n\nBETA\n");
        let id = block_ids("hist/seq")[0].1;
        Spi::run_with_args(
            "SELECT knowledge.update_block($1, 'ALPHA'::markdown)",
            &[id.into()],
        )
        .expect("update failed");

        let rows: Vec<(i64, String)> = Spi::connect(|client| {
            client
                .select(
                    "SELECT r.seq, r.verb FROM pgmind.revision r
                       JOIN pgmind.note n ON n.id = r.note_id
                      WHERE n.path = 'hist/seq' ORDER BY r.seq",
                    None,
                    &[],
                )
                .expect("revision query failed")
                .map(|row| {
                    (
                        row.get::<i64>(1).unwrap().unwrap(),
                        row.get::<String>(2).unwrap().unwrap(),
                    )
                })
                .collect()
        });
        assert_eq!(
            rows,
            vec![
                (0, "write".to_string()),
                (1, "write".to_string()),
                (2, "update_block".to_string()),
            ],
            "seq must be dense from 0 and verb must name the operation"
        );
    }

    /// RFC-005 D4: a structural insert costs ONE effect row, not one per block
    /// after it. This is the rule the whole storage model rests on — position
    /// (ord, spans) and heading_path ride the per-revision vectors, so only
    /// content-visible change produces a `block_revision` row.
    ///
    /// The design this replaced wrote a full history row per shifted block,
    /// which is 2x current state on patch traffic and ~65x on the structural
    /// traffic Phase 4's importer generates.
    #[pg_test]
    fn structural_edits_do_not_write_a_row_per_block() {
        let mut md = String::from("# Top\n\n");
        for i in 0..40 {
            md.push_str(&format!("para {i}\n\n"));
        }
        write("hist/churn", &md);
        let effects_after_create: i64 = effect_rows("hist/churn");
        assert_eq!(
            effects_after_create, 41,
            "creation mints every block: one existed=false row each"
        );

        // Insert at the very top: every following block's ord shifts, and the
        // heading_path of nothing changes. One content-visible change ⇒ 1 row.
        let first = block_ids("hist/churn")[1].1;
        Spi::run_with_args(
            "SELECT knowledge.insert_blocks('hist/churn', 'inserted'::markdown, before => $1)",
            &[first.into()],
        )
        .expect("insert failed");
        assert_eq!(
            effect_rows("hist/churn") - effects_after_create,
            1,
            "a structural insert writes exactly one effect row (the minted block)"
        );

        // Rename the heading: heading_path changes for all 40 blocks beneath
        // it, and that is positional too — only the heading's own content is
        // content-visible.
        let heading = block_ids("hist/churn")[0].1;
        let before = effect_rows("hist/churn");
        Spi::run_with_args(
            "SELECT knowledge.update_block($1, '# Renamed'::markdown)",
            &[heading.into()],
        )
        .expect("update failed");
        assert_eq!(
            effect_rows("hist/churn") - before,
            1,
            "a heading rename writes one effect row, not one per block in the section"
        );
        verify_clean("hist/churn");
    }

    /// RFC-005 D4: the pre-image is the bytes as they were, and it is captured
    /// before reconcile overwrites them.
    #[pg_test]
    fn history_records_the_pre_image_of_both_lanes() {
        write("hist/pre", "alpha\n\nbeta\n");
        let beta = block_ids("hist/pre")[1].1;
        Spi::run_with_args(
            "SELECT knowledge.update_block($1, 'BETA'::markdown)",
            &[beta.into()],
        )
        .expect("update failed");

        let prev: Option<String> = Spi::get_one_with_args(
            "SELECT prev_content FROM pgmind.block_revision WHERE block_id = $1 AND existed
              ORDER BY seq DESC LIMIT 1",
            &[beta.into()],
        )
        .unwrap();
        assert_eq!(
            prev.as_deref(),
            Some("beta"),
            "the effect row holds what the block said BEFORE the edit"
        );

        // The byte lane's script carries the old tile literally (X1: no history
        // row defines its bytes by reference to another).
        let payload: Option<Vec<Option<String>>> = Spi::get_one(
            "SELECT nr.tile_payload FROM pgmind.note_revision nr
               JOIN pgmind.note n ON n.id = nr.note_id
              WHERE n.path = 'hist/pre' ORDER BY nr.seq DESC LIMIT 1",
        )
        .unwrap();
        let payload = payload.expect("no note_revision row");
        assert!(
            payload.iter().flatten().any(|t| t.contains("beta")),
            "tile pre-image must hold the replaced bytes literally, got {payload:?}"
        );
    }

    /// RFC-005 D3: frames are written at cadence by the WRITE path. Without a
    /// cadence writer the only frame would be compaction's, at the floor, where
    /// reconstruction (which anchors at or above its target) cannot use it.
    #[pg_test]
    fn frames_are_written_at_the_configured_cadence() {
        Spi::run("SET pgmind.frame_every = 3").unwrap();
        for i in 0..7 {
            write("hist/frames", &format!("body {i}\n"));
        }
        let seqs: Vec<i64> = Spi::connect(|client| {
            client
                .select(
                    "SELECT f.seq FROM pgmind.note_frame f
                       JOIN pgmind.note n ON n.id = f.note_id
                      WHERE n.path = 'hist/frames' ORDER BY f.seq",
                    None,
                    &[],
                )
                .expect("frame query failed")
                .map(|row| row.get::<i64>(1).unwrap().unwrap())
                .collect()
        });
        assert_eq!(seqs, vec![0, 3, 6], "a frame every 3 revisions");
        Spi::run("RESET pgmind.frame_every").unwrap();
    }

    fn effect_rows(path: &str) -> i64 {
        Spi::get_one_with_args(
            "SELECT count(*) FROM pgmind.block_revision br
               JOIN pgmind.note n ON n.id = br.note_id WHERE n.path = $1",
            &[path.into()],
        )
        .unwrap()
        .unwrap()
    }

    // ---------- time travel (RFC-005 D3) ----------

    /// The property the whole version engine exists for: every revision of a
    /// note reconstructs to the exact bytes that were written, at every depth,
    /// through frames and across structural edits.
    #[pg_test]
    fn every_revision_reconstructs_byte_exactly() {
        Spi::run("SET pgmind.frame_every = 3").unwrap();
        let versions = [
            "# Doc\n\nalpha\n",
            "# Doc\n\nalpha\n\nbeta\n",
            "# Doc\n\nALPHA\n\nbeta\n",
            "# Doc\n\nbeta\n\nALPHA\n",
            "---\ntitle: T\n---\n\n# Doc\n\nbeta\n\nALPHA\n\n- item\n",
            "# Renamed\n\nbeta\n",
            "# Renamed\n\nbeta\n\ngamma [[link]] #tag\n",
        ];
        for v in versions {
            write("tt/doc", v);
        }
        for (seq, expected) in versions.iter().enumerate() {
            let got: String = Spi::get_one_with_args(
                "SELECT knowledge.read_as_of('tt/doc', $1::bigint)::text",
                &[(seq as i64).into()],
            )
            .unwrap()
            .unwrap();
            assert_eq!(&got, expected, "reconstruction at seq {seq}");
        }
        Spi::run("RESET pgmind.frame_every").unwrap();
    }

    /// Structure at a past revision comes from the stored vectors, never from
    /// re-parsing (X2) — so ids, order and heading_path must all come back.
    #[pg_test]
    fn blocks_as_of_returns_past_structure() {
        write("tt/blocks", "# A\n\none\n\n# B\n\ntwo\n");
        let before = block_ids("tt/blocks");
        write("tt/blocks", "# A\n\none\n");
        let rows: Vec<(i32, Uuid, String, Vec<String>)> = Spi::connect(|client| {
            client
                .select(
                    "SELECT ord, block_id, content, heading_path
                       FROM knowledge.blocks_as_of('tt/blocks', 0::bigint) ORDER BY ord",
                    None,
                    &[],
                )
                .unwrap()
                .map(|r| {
                    (
                        r.get::<i32>(1).unwrap().unwrap(),
                        r.get::<Uuid>(2).unwrap().unwrap(),
                        r.get::<String>(3).unwrap().unwrap(),
                        r.get::<Vec<String>>(4).unwrap().unwrap(),
                    )
                })
                .collect()
        });
        assert_eq!(rows.len(), 4, "the deleted section is back");
        assert_eq!(rows[3].2, "two");
        assert_eq!(rows[3].3, vec!["B".to_string()], "heading_path is restored");
        assert_eq!(rows[0].1, before[0].1, "identity is preserved across time");
    }

    /// PM011 and PM010 mean opposite things and are never interchanged.
    #[pg_test]
    fn history_errors_distinguish_missing_from_compacted() {
        write("tt/err", "one\n");
        write("tt/err", "two\n");
        assert_eq!(
            sqlstate_of("SELECT knowledge.read_as_of('tt/err', 99::bigint)"),
            "PM010",
            "a seq the note never had is a client bug"
        );
        // Simulate retention having moved the floor.
        Spi::run("UPDATE pgmind.note SET history_floor = 1 WHERE path = 'tt/err'").unwrap();
        assert_eq!(
            sqlstate_of("SELECT knowledge.read_as_of('tt/err', 0::bigint)"),
            "PM011",
            "below the floor is 'no longer reconstructable', not 'no such revision'"
        );
    }

    /// history() and diff() read the ledger without reconstructing anything
    /// they do not need.
    ///
    /// Note what "changed" means here: pgmind mints on edit, so a whole-document
    /// rewrite of a paragraph is remove + add, not a change. `changed` appears
    /// only where identity was ASSERTED — an `update_block`, or an `^id` claim.
    /// That is RFC-004 A1 showing through the diff, and it is the honest
    /// rendering: the engine will not claim two blocks are the same block
    /// because their text looks similar.
    #[pg_test]
    fn history_and_diff_report_what_changed() {
        write("tt/diff", "alpha\n\nbeta\n");
        let beta = block_ids("tt/diff")[1].1;
        Spi::run_with_args(
            "SELECT knowledge.update_block($1, 'BETA'::markdown)",
            &[beta.into()],
        )
        .expect("update failed");
        write("tt/diff", "alpha\n\nBETA\n\ngamma\n");

        let rows: Vec<(i64, String)> = Spi::connect(|client| {
            client
                .select(
                    "SELECT seq, verb FROM knowledge.history('tt/diff') ORDER BY seq",
                    None,
                    &[],
                )
                .unwrap()
                .map(|r| {
                    (
                        r.get::<i64>(1).unwrap().unwrap(),
                        r.get::<String>(2).unwrap().unwrap(),
                    )
                })
                .collect()
        });
        assert_eq!(
            rows,
            vec![
                (0, "write".to_string()),
                (1, "update_block".to_string()),
                (2, "write".to_string()),
            ]
        );

        let revs: Vec<Uuid> = Spi::connect(|client| {
            client
                .select(
                    "SELECT revision FROM knowledge.history('tt/diff') ORDER BY seq",
                    None,
                    &[],
                )
                .unwrap()
                .map(|r| r.get::<Uuid>(1).unwrap().unwrap())
                .collect()
        });

        // seq 0 -> 1: identity asserted, so this is a change, not a churn.
        let changes: Vec<(String, Option<String>, Option<String>)> = Spi::connect(|client| {
            client
                .select(
                    "SELECT change, before, after FROM knowledge.diff('tt/diff', $1, $2)",
                    None,
                    &[revs[0].into(), revs[1].into()],
                )
                .unwrap()
                .map(|r| {
                    (
                        r.get::<String>(1).unwrap().unwrap(),
                        r.get::<String>(2).unwrap(),
                        r.get::<String>(3).unwrap(),
                    )
                })
                .collect()
        });
        assert_eq!(
            changes,
            vec![(
                "changed".to_string(),
                Some("beta".to_string()),
                Some("BETA".to_string())
            )],
            "update_block keeps the id, so the diff reports one changed block"
        );

        // seq 1 -> 2: one block appended, nothing else touched.
        let added: Vec<String> = Spi::connect(|client| {
            client
                .select(
                    "SELECT change FROM knowledge.diff('tt/diff', $1, $2)",
                    None,
                    &[revs[1].into(), revs[2].into()],
                )
                .unwrap()
                .map(|r| r.get::<String>(1).unwrap().unwrap())
                .collect()
        });
        assert_eq!(added, vec!["added".to_string()]);
    }

    /// RFC-005 D8: verify_history must be able to FAIL. The accepted RFC's
    /// only reconstruction clause was `read_as_of(head) = read()`, which is a
    /// tautology — reconstruction at head applies no scripts by definition.
    #[pg_test]
    fn verify_history_catches_a_missing_pre_image() {
        Spi::run("SET pgmind.frame_every = 2").unwrap();
        for i in 0..6 {
            write("tt/verify", &format!("body {i}\n"));
        }
        assert_eq!(
            history_violations("tt/verify"),
            0,
            "healthy history verifies clean"
        );

        // Delete one revision's pre-image, exactly as a lost dump registration
        // or a half-written lane would.
        Spi::run(
            "DELETE FROM pgmind.note_revision nr USING pgmind.note n
              WHERE n.id = nr.note_id AND n.path = 'tt/verify' AND nr.seq = 3",
        )
        .unwrap();
        assert!(
            history_violations("tt/verify") > 0,
            "a missing note_revision row must be reported, not silently reconstructed around"
        );
        Spi::run("RESET pgmind.frame_every").unwrap();
    }

    fn history_violations(path: &str) -> i64 {
        Spi::get_one_with_args(
            "SELECT count(*) FROM pgmind.verify_history(
               (SELECT id FROM pgmind.note WHERE path = $1 AND tombstoned_at IS NULL))",
            &[path.into()],
        )
        .expect("verify_history failed")
        .unwrap()
    }

    // ---------- concurrency contract (RFC-005 D5) ----------

    /// CAS is checked BEFORE the byte-identical short-circuit. The short-circuit
    /// answers "did anything change"; CAS answers "did you see what you were
    /// changing" — reordering them lets a stale writer's no-op silently succeed.
    #[pg_test]
    fn cas_precedes_the_idempotence_short_circuit() {
        let head = write("cas/note", "alpha\n");
        write("cas/note", "beta\n");
        assert_eq!(
            sqlstate_of(&format!(
                "SELECT knowledge.write('cas/note', 'beta'::markdown, '{head}'::uuid)"
            )),
            "PM009",
            "byte-identical input with a stale head must still raise"
        );
        let current: Uuid =
            Spi::get_one("SELECT head_revision FROM pgmind.note WHERE path = 'cas/note'")
                .unwrap()
                .unwrap();
        assert_eq!(
            sqlstate_of(&format!(
                "SELECT knowledge.write('cas/note', 'gamma'::markdown, '{current}'::uuid)"
            )),
            "00000",
            "the current head is accepted"
        );
    }

    /// D5.6: a caller that asserted a head for a path with no live note is not
    /// creating — it is editing something that is gone, and a silent create
    /// would make its CAS unenforceable exactly when it matters.
    #[pg_test]
    fn cas_on_a_missing_note_raises_rather_than_creating() {
        let head = write("cas/other", "x\n");
        assert_eq!(
            sqlstate_of(&format!(
                "SELECT knowledge.write('cas/absent', 'y'::markdown, '{head}'::uuid)"
            )),
            "PM009"
        );
        let exists: i64 =
            Spi::get_one("SELECT count(*) FROM pgmind.note WHERE path = 'cas/absent'")
                .unwrap()
                .unwrap();
        assert_eq!(exists, 0, "no note was created");
    }

    /// CAS reaches the block ops too, with the same code and the same meaning.
    #[pg_test]
    fn block_ops_honour_expected_head() {
        let stale = write("cas/ops", "alpha\n\nbeta\n");
        write("cas/ops", "alpha\n\ngamma\n");
        let id = block_ids("cas/ops")[0].1;
        assert_eq!(
            sqlstate_of(&format!(
                "SELECT knowledge.update_block('{id}'::uuid, 'ALPHA'::markdown, '{stale}'::uuid)"
            )),
            "PM009"
        );
    }

    /// Two appends to the same section both survive, in order — the property
    /// that makes append an operation rather than a read-modify-write.
    #[pg_test]
    fn append_to_section_keeps_both_appends() {
        write("cas/log", "# Log\n\nfirst\n");
        for line in ["second", "third"] {
            Spi::run_with_args(
                "SELECT knowledge.append_to_section('cas/log', ARRAY['Log'], $1::markdown)",
                &[line.into()],
            )
            .expect("append failed");
        }
        let body = read("cas/log");
        assert!(
            body.contains("first") && body.contains("second") && body.contains("third"),
            "every append survives: {body:?}"
        );
        assert!(
            body.find("second").unwrap() < body.find("third").unwrap(),
            "appends land in order"
        );
        assert_eq!(
            sqlstate_of(
                "SELECT knowledge.append_to_section('cas/log', ARRAY['Nope'], 'x'::markdown)"
            ),
            "PM007"
        );
        verify_clean("cas/log");
    }

    // ---------- tenant isolation (RFC-003 D1 / §5 gate 4) ----------

    #[pg_test]
    fn tenant_scoping_and_grant_boundary() {
        let vault_b = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        write("public-note", "default vault\n");
        Spi::run(&format!("SET pgmind.vault_id = '{vault_b}'")).unwrap();
        write("secret/tenant-b", "tenant b secret\n");
        Spi::run("RESET pgmind.vault_id").unwrap();

        // Scoping: functions see only the current vault.
        let n: i64 = Spi::get_one("SELECT count(*) FROM knowledge.notes()")
            .unwrap()
            .unwrap();
        assert_eq!(n, 1, "default vault sees only its note");

        // RLS pattern (D1) + a non-superuser role. This calls the SHIPPED
        // helper rather than re-implementing its loop: the test used to
        // enumerate the same six table names the function did, so when Phase 3
        // added four more tables carrying vault_id, the test would have gone on
        // passing while history leaked across tenants.
        Spi::run("SELECT pgmind.enable_vault_rls()").unwrap();
        assert_every_vault_table_has_a_policy();
        Spi::run("CREATE ROLE pgmind_tenant_test").unwrap();
        Spi::run("GRANT USAGE ON SCHEMA pgmind, knowledge TO pgmind_tenant_test").unwrap();
        Spi::run("GRANT SELECT ON ALL TABLES IN SCHEMA pgmind TO pgmind_tenant_test").unwrap();
        Spi::run("SET ROLE pgmind_tenant_test").unwrap();

        let visible: i64 = Spi::get_one("SELECT count(*) FROM pgmind.note")
            .unwrap()
            .unwrap();
        assert_eq!(visible, 1, "RLS: direct reads scoped to current vault");
        // The GUC pattern is scoping, not a boundary: hostile SET switches vaults.
        Spi::run(&format!("SET pgmind.vault_id = '{vault_b}'")).unwrap();
        let hostile: i64 = Spi::get_one("SELECT count(*) FROM pgmind.note")
            .unwrap()
            .unwrap();
        assert_eq!(
            hostile, 1,
            "GUC pattern alone: SET reaches the other vault (documented)"
        );
        Spi::run("RESET pgmind.vault_id").unwrap();
        Spi::run("RESET ROLE").unwrap();

        // Grant-anchored boundary variant (D1): the grant bounds the GUC.
        Spi::run(
            "CREATE SCHEMA IF NOT EXISTS pgmind_app;
             CREATE TABLE pgmind_app.vault_grant (
               grantee name NOT NULL, vault_id uuid NOT NULL,
               PRIMARY KEY (grantee, vault_id));
             GRANT USAGE ON SCHEMA pgmind_app TO pgmind_tenant_test;
             GRANT SELECT ON pgmind_app.vault_grant TO pgmind_tenant_test;
             INSERT INTO pgmind_app.vault_grant
               VALUES ('pgmind_tenant_test', '00000000-0000-0000-0000-000000000000');",
        )
        .unwrap();
        for t in ["note", "revision", "tile", "block", "edge", "tag"] {
            Spi::run(&format!("DROP POLICY vault_isolation ON pgmind.{t}")).unwrap();
            Spi::run(&format!(
                "CREATE POLICY vault_isolation ON pgmind.{t}
                 USING (vault_id = current_setting('pgmind.vault_id')::uuid
                        AND vault_id IN (SELECT vault_id FROM pgmind_app.vault_grant
                                         WHERE grantee = current_user))"
            ))
            .unwrap();
        }
        Spi::run("SET ROLE pgmind_tenant_test").unwrap();
        let granted: i64 = Spi::get_one("SELECT count(*) FROM pgmind.note")
            .unwrap()
            .unwrap();
        assert_eq!(granted, 1, "granted vault visible");
        Spi::run(&format!("SET pgmind.vault_id = '{vault_b}'")).unwrap();
        let blocked: i64 = Spi::get_one("SELECT count(*) FROM pgmind.note")
            .unwrap()
            .unwrap();
        assert_eq!(blocked, 0, "hostile SET blocked by the grant boundary");
        Spi::run("RESET pgmind.vault_id").unwrap();
        Spi::run("RESET ROLE").unwrap();
    }

    // ---------- typed errors ----------

    #[pg_test]
    fn typed_error_sqlstates() {
        write("errs", "alpha\n\n- a\n- b\n");
        let ids = block_ids("errs");
        let (alpha, item_a) = (ids[0].1, ids[1].1);
        assert_eq!(sqlstate_of("SELECT knowledge.read('errs/nope')"), "PM002");
        assert_eq!(
            sqlstate_of("SELECT knowledge.write('bad//path', 'x'::markdown)"),
            "PM001"
        );
        assert_eq!(
            sqlstate_of(
                "SELECT knowledge.update_block('00000000-0000-0000-0000-000000000001'::uuid, 'x'::markdown)"
            ),
            "PM003"
        );
        assert_eq!(
            sqlstate_of(&format!(
                "SELECT knowledge.update_block('{alpha}'::uuid, 'one\n\ntwo'::markdown)"
            )),
            "PM004"
        );
        assert_eq!(
            sqlstate_of(&format!(
                "SELECT knowledge.split_block('{alpha}'::uuid, 'only-one'::markdown)"
            )),
            "PM004"
        );
        assert_eq!(
            sqlstate_of(&format!(
                "SELECT knowledge.move_block('{alpha}'::uuid, before => '{item_a}'::uuid)"
            )),
            "PM006",
            "cross-container move"
        );
        // Unclosed fence swallows the rest of the note → PM008.
        assert_eq!(
            sqlstate_of(&format!(
                "SELECT knowledge.update_block('{alpha}'::uuid, e'```\nnope'::markdown)"
            )),
            "PM008",
            "unclosed fence must be rejected"
        );
    }

    // ---------- A3 carry: identity must not migrate between sections ----------

    fn meta_of(revision: Uuid) -> serde_json::Value {
        let raw: String = Spi::get_one_with_args(
            "SELECT meta::text FROM pgmind.revision WHERE id = $1",
            &[revision.into()],
        )
        .expect("meta failed")
        .expect("meta NULL");
        serde_json::from_str(&raw).expect("meta is not json")
    }

    /// RFC-004 A1: "an ID is never reused". `content_hash` covers only (kind,
    /// normalized content), so an untiered pass 2 handed a deleted section's
    /// paragraph ID to an identical paragraph in a surviving section — and
    /// deleted the survivor's own ID.
    #[pg_test]
    fn section_delete_does_not_recycle_ids_across_sections() {
        let path = "carry/sections";
        write(path, "# Chapter A\n\nTODO\n\n# Chapter B\n\nTODO\n");
        let before = block_ids(path);
        let todo_b = before
            .iter()
            .filter(|(c, _)| c == "TODO")
            .nth(1)
            .expect("two TODO blocks")
            .1;

        write(path, "# Chapter B\n\nTODO\n");
        let after = block_ids(path);
        let surviving_todo = after
            .iter()
            .find(|(c, _)| c == "TODO")
            .expect("TODO survives")
            .1;
        assert_eq!(
            surviving_todo, todo_b,
            "Chapter B's paragraph must keep its OWN id, not inherit Chapter A's"
        );
        verify_clean(path);
    }

    /// The tier-1 fallback: renaming a heading changes its section's
    /// `heading_path`, and the section's blocks must still carry.
    #[pg_test]
    fn heading_rename_carries_section_blocks() {
        let path = "carry/rename";
        write(path, "# Old Name\n\nbody text\n");
        let before = block_ids(path);
        let body = before.iter().find(|(c, _)| c == "body text").unwrap().1;

        write(path, "# New Name\n\nbody text\n");
        let after = block_ids(path);
        assert_eq!(
            after.iter().find(|(c, _)| c == "body text").unwrap().1,
            body,
            "a heading rename must not mint its section's blocks"
        );
        verify_clean(path);
    }

    // ---------- A4 provenance ----------

    /// RFC-004 A4: `split` records `from`, `into` (the resulting block uuids)
    /// and `marker_to` (the surviving holder's uuid), all INSIDE the `split`
    /// object. `marker_to` used to be the marker's text label, hoisted to the
    /// top level, and `into` was absent entirely.
    #[pg_test]
    fn split_provenance_matches_a4_schema() {
        let path = "prov/split";
        write(path, "one two ^x\n\ntail\n");
        let target = block_ids(path)[0].1;
        let rev: Uuid = Spi::get_one_with_args(
            "SELECT (knowledge.split_block($1::uuid, e'one\n\ntwo ^x'::markdown)).revision",
            &[target.into()],
        )
        .expect("split failed")
        .expect("split NULL");

        let meta = meta_of(rev);
        let split = meta.get("split").expect("meta.split present");
        assert_eq!(
            split.get("from").unwrap().as_str().unwrap(),
            target.to_string()
        );
        let into = split
            .get("into")
            .expect("A4 requires split.into")
            .as_array()
            .unwrap();
        assert_eq!(into.len(), 2, "both split fragments recorded");
        assert_eq!(
            into[0].as_str().unwrap(),
            target.to_string(),
            "A2: the first fragment keeps the id"
        );
        // A5: the marker rode to the SECOND fragment, so marker_to must be that
        // block's uuid — not the label "x", and not null.
        let marker_to = split.get("marker_to").expect("A4 requires split.marker_to");
        assert_eq!(
            marker_to.as_str().unwrap(),
            into[1].as_str().unwrap(),
            "marker_to must name the surviving holder by uuid"
        );
        assert!(
            meta.get("marker_to").is_none(),
            "marker_to belongs inside the split object, not at the top level"
        );
        verify_clean(path);
    }

    /// RFC-004 §5: merge without `keep`, and the marker-holder record.
    #[pg_test]
    fn merge_without_keep_records_provenance() {
        let path = "prov/merge";
        write(path, "alpha ^m\n\nbeta\n\ngamma\n");
        let ids = block_ids(path);
        let (a, b) = (ids[0].1, ids[1].1);
        let rev: Uuid = Spi::get_one_with_args(
            "SELECT (knowledge.merge_blocks(ARRAY[$1,$2]::uuid[], 'alpha beta ^m'::markdown)).revision",
            &[a.into(), b.into()],
        )
        .expect("merge failed")
        .expect("merge NULL");

        let meta = meta_of(rev);
        let merge = meta.get("merge").expect("meta.merge present");
        assert_eq!(
            merge.get("into").unwrap().as_str().unwrap(),
            a.to_string(),
            "default keep is the first member"
        );
        assert_eq!(merge.get("from").unwrap().as_array().unwrap().len(), 2);
        assert_eq!(
            merge.get("marker_to").unwrap().as_str().unwrap(),
            a.to_string(),
            "the surviving holder, by uuid"
        );
        assert!(meta["removed"]
            .as_array()
            .unwrap()
            .contains(&serde_json::Value::String(b.to_string())));
        verify_clean(path);
    }

    /// RFC-004 §5 / PM006: a non-surviving merge member owning container
    /// children is rejected. The guard only inspected `idxs[1..]`, so an
    /// explicit `keep` that was not the first member let member 0's nested
    /// content be deleted silently.
    #[pg_test]
    fn merge_retiree_with_descendants_is_pm006_even_when_keep_is_not_first() {
        let path = "prov/merge-keep";
        write(path, "- a\n  - a1\n- b\n");
        let ids = block_ids(path);
        // Items only: a (with nested a1) and b.
        let item_a = ids[0].1;
        let item_b = ids
            .iter()
            .find(|(c, _)| c.starts_with('b'))
            .expect("item b")
            .1;
        assert_eq!(
            sqlstate_of(&format!(
                "SELECT knowledge.merge_blocks(ARRAY['{item_a}','{item_b}']::uuid[], \
                 '- ab'::markdown, keep => '{item_b}'::uuid)"
            )),
            "PM006",
            "member 0 is non-surviving here and owns container children"
        );
        // Nothing was destroyed.
        assert_eq!(read(path), "- a\n  - a1\n- b\n");
        verify_clean(path);
    }

    // ---------- splice discipline ----------

    /// RFC-003 D6: "bytes outside the spliced span are never reformatted."
    /// Separator synthesis used to blank-terminate EVERY tile, so an insert
    /// rewrote untouched neighbours — invisible to PM008 because D7 strips
    /// trailing newline runs before hashing.
    #[pg_test]
    fn insert_preserves_unseparated_neighbouring_tiles() {
        let path = "splice/tiles";
        // A paragraph interrupted by an ATX heading: two tiles, no blank line.
        write(path, "para\n# Heading\n");
        assert_eq!(read(path), "para\n# Heading\n");
        Spi::run_with_args(
            "SELECT knowledge.insert_blocks($1, 'x'::markdown)",
            &[path.into()],
        )
        .expect("insert failed");
        assert_eq!(
            read(path),
            "para\n# Heading\n\nx\n",
            "only the seam before the inserted tile may gain a blank line"
        );
        verify_clean(path);
    }

    /// RFC-003 D6 splice: an item's own marker lives between the enclosing
    /// container's decoration and the end of its own, so slicing
    /// (full - outer) bytes from the LINE start returned the container's
    /// prefix — `> - a` yielded "> " and the insert became a nested quote.
    #[pg_test]
    fn insert_item_level_into_a_quoted_list() {
        let path = "splice/quoted-list";
        write(path, "> - alpha\n> - beta\n");
        let ids = block_ids(path);
        let alpha = ids
            .iter()
            .find(|(c, _)| c.starts_with("alpha"))
            .expect("alpha item")
            .1;
        Spi::run_with_args(
            "SELECT knowledge.insert_blocks($1, '- gamma'::markdown, after => $2::uuid)",
            &[path.into(), alpha.into()],
        )
        .expect("item-level insert failed");
        let out = read(path);
        assert!(
            out.contains("> - gamma"),
            "inserted item must stay a quoted list item, got {out:?}"
        );
        assert!(
            !out.contains("> > "),
            "must not become a nested blockquote, got {out:?}"
        );
        verify_clean(path);
    }

    /// A block op must maintain lane 0. `reconcile` wrote tiles, blocks, edges
    /// and tags but never the note row, so a splice that moved the
    /// preamble/tile boundary left `knowledge.read()` returning bytes that
    /// were never written.
    #[pg_test]
    fn block_ops_keep_the_note_row_consistent() {
        let path = "splice/preamble";
        write(path, "---\ntitle: T\n---\n\nalpha\n");
        Spi::run_with_args(
            "SELECT knowledge.insert_blocks($1, 'beta'::markdown)",
            &[path.into()],
        )
        .expect("insert failed");
        assert_eq!(read(path), "---\ntitle: T\n---\n\nalpha\n\nbeta\n");
        // properties must still be derived from the frontmatter after the op.
        let title: Option<String> = Spi::get_one_with_args(
            "SELECT properties->>'title' FROM pgmind.note WHERE path = $1",
            &[path.into()],
        )
        .expect("properties failed");
        assert_eq!(title.as_deref(), Some("T"));
        verify_clean(path);
    }

    // ---------- read surface ----------

    /// RFC-002 D2: a section is delimited by a DOCUMENT heading. A heading
    /// inside a blockquote has no parent either, and matching it returned
    /// quoted text plus the unquoted paragraph that followed the quote.
    #[pg_test]
    fn read_section_ignores_headings_inside_blockquotes() {
        let path = "read/quoted-heading";
        write(path, "> # Q\n\npara\n\n# Real\n\nbody\n");
        assert_eq!(
            sqlstate_of(&format!(
                "SELECT knowledge.read_section('{path}', ARRAY['Q'])"
            )),
            "PM007",
            "a quoted heading is not a document section"
        );
        let real: String = Spi::get_one_with_args(
            "SELECT knowledge.read_section($1, ARRAY['Real'])::text",
            &[path.into()],
        )
        .expect("read_section failed")
        .expect("NULL");
        assert_eq!(real, "# Real\n\nbody\n");
    }

    /// RFC-003 D5: `write()` stores the NFC-trimmed path, so every reader must
    /// look the note up the same way — otherwise a caller cannot read back the
    /// note it just wrote with the identical string (macOS emits NFD).
    #[pg_test]
    fn paths_round_trip_through_normalization() {
        let nfd = "notes/cafe\u{0301}";
        let nfc = "notes/caf\u{00e9}";
        write(nfd, "body\n");
        assert_eq!(read(nfd), "body\n", "readable by the spelling write() took");
        assert_eq!(read(nfc), "body\n", "and by the normalized spelling");
        // Trailing whitespace is trimmed on write; reads must agree.
        write("notes/trimmed  ", "x\n");
        assert_eq!(read("notes/trimmed"), "x\n");
    }

    /// RFC-002 D8: `*` is a legal path character, and the matcher must not
    /// consume a pattern `*` against a literal one without leaving a backtrack
    /// point. Also pins that the literal-prefix pushdown does not drop matches.
    #[pg_test]
    fn notes_glob_matches_literal_stars_and_prefixes() {
        write("glob/a*bc", "x\n");
        write("glob/plain", "x\n");
        write("globber/other", "x\n");
        let hit: i64 = Spi::get_one("SELECT count(*) FROM knowledge.notes('glob/a*c')")
            .unwrap()
            .unwrap();
        assert_eq!(
            hit, 1,
            "'glob/a*c' must match the note literally named a*bc"
        );
        let prefixed: i64 = Spi::get_one("SELECT count(*) FROM knowledge.notes('glob/**')")
            .unwrap()
            .unwrap();
        assert_eq!(prefixed, 2, "prefix pushdown must not leak into 'globber/'");
        // A pathological pattern must return, not wedge the backend.
        let deep: i64 =
            Spi::get_one("SELECT count(*) FROM knowledge.notes(repeat('**/', 40) || 'zzz')")
                .unwrap()
                .unwrap();
        assert_eq!(deep, 0);
    }

    /// RFC-003 D1 / RFC-004 A6: a malformed `pgmind.vault_id` errors at first
    /// use. `+`-prefixed byte pairs used to parse as the all-zeros DEFAULT
    /// vault — silently writing a tenant's notes into someone else's vault.
    #[pg_test]
    fn malformed_vault_guc_raises_pm001() {
        for bad in [
            "+0+0+0+0+0+0+0+0+0+0+0+0+0+0+0+0",
            "0-0000000-00000000000000000000000-0",
            "not-a-uuid",
        ] {
            assert_eq!(
                sqlstate_of(&format!(
                    "SET pgmind.vault_id = '{bad}'; SELECT count(*) FROM knowledge.orphans()"
                )),
                "PM001",
                "GUC {bad:?} must raise PM001, not resolve to a vault"
            );
        }
        Spi::run("RESET pgmind.vault_id").unwrap();
    }

    /// RFC-003 §5 gate 2: extraction dedup and the full link-kind set through
    /// storage.
    #[pg_test]
    fn extraction_dedups_and_covers_all_link_kinds() {
        let path = "extract/kinds";
        write("extract/target", "t\n");
        write(
            path,
            "See [[extract/target]] and [[extract/target]] again.\n\n\
             Embed ![[extract/target]] plus [md](extract/target) and [[extract/target#^b1]].\n",
        );
        let kinds: Vec<(String, i64)> = Spi::connect(|client| {
            client
                .select(
                    "SELECT kind::text, count(*)::int8 FROM pgmind.edge
                     WHERE src_note = (SELECT id FROM pgmind.note WHERE path = $1)
                     GROUP BY kind::text ORDER BY 1",
                    None,
                    &[path.into()],
                )
                .expect("edge query failed")
                .map(|r| {
                    (
                        r.get::<String>(1).unwrap().unwrap(),
                        r.get::<i64>(2).unwrap().unwrap(),
                    )
                })
                .collect()
        });
        let by_kind: std::collections::HashMap<_, _> = kinds.into_iter().collect();
        assert_eq!(
            by_kind.get("wikilink").copied(),
            Some(1),
            "the duplicate [[extract/target]] in one block dedups to one edge"
        );
        assert_eq!(by_kind.get("transclusion").copied(), Some(1));
        assert_eq!(by_kind.get("mdlink").copied(), Some(1));
        assert_eq!(by_kind.get("blockref").copied(), Some(1));
        verify_clean(path);
    }

    /// RFC-003 D5: an unparseable target is `dangling_reason = 'invalid'`,
    /// distinct from 'missing'.
    #[pg_test]
    fn invalid_link_targets_are_reported_as_invalid() {
        let path = "extract/invalid";
        write(path, "bad [[a//b]] and missing [[nowhere]]\n");
        let reasons: Vec<(String, String)> = Spi::connect(|client| {
            client
                .select(
                    "SELECT dst_path, dangling_reason FROM pgmind.edge
                     WHERE src_note = (SELECT id FROM pgmind.note WHERE path = $1)
                     ORDER BY dst_path",
                    None,
                    &[path.into()],
                )
                .expect("edge query failed")
                .map(|r| {
                    (
                        r.get::<String>(1).unwrap().unwrap(),
                        r.get::<String>(2).unwrap().unwrap(),
                    )
                })
                .collect()
        });
        assert_eq!(
            reasons,
            vec![
                ("a//b".to_string(), "invalid".to_string()),
                ("nowhere".to_string(), "missing".to_string()),
            ]
        );
        verify_clean(path);
    }

    /// RFC-004 A2: move across a section boundary keeps the ID and recomputes
    /// `heading_path` (position is never identity — Law 4).
    #[pg_test]
    fn move_across_sections_keeps_id_changes_heading_path() {
        let path = "move/sections";
        write(path, "# A\n\nalpha\n\n# B\n\nbeta\n");
        let before = block_ids(path);
        let alpha = before.iter().find(|(c, _)| c == "alpha").unwrap().1;
        let beta = before.iter().find(|(c, _)| c == "beta").unwrap().1;
        Spi::run_with_args(
            "SELECT knowledge.move_block($1::uuid, after => $2::uuid)",
            &[alpha.into(), beta.into()],
        )
        .expect("move failed");

        let hp: Vec<String> = Spi::get_one_with_args(
            "SELECT heading_path FROM pgmind.block WHERE id = $1",
            &[alpha.into()],
        )
        .expect("heading_path failed")
        .expect("NULL");
        assert_eq!(hp, vec!["B".to_string()], "heading_path is recomputed");
        assert!(
            block_ids(path)
                .iter()
                .any(|(c, id)| c == "alpha" && *id == alpha),
            "the moved block keeps its id"
        );
        verify_clean(path);
    }
}
