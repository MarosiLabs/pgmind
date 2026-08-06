#!/usr/bin/env python3
"""pgmind evaluation harness.

Runs every registered gate suite and writes a machine-readable report to
eval/results/latest.json. Suite statuses:

  ok      - ran and met its thresholds
  fail    - ran and missed its thresholds (harness exits non-zero)
  pending - awaiting a phase deliverable

Phase 1 suites (RFC-002 §5) delegate to the pgmind-eval binary in core/
(pure Rust, no Postgres needed). Network use here is dev-time corpus
fetching only - never part of the product (Law 2).
"""

import glob
import json
import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
COMMONMARK_SPEC_URL = "https://spec.commonmark.org/0.31.2/spec.json"
FUZZ_COUNT = "100000"
STORAGE_FUZZ_COUNT = "10000"
CAPACITY_NOTES = 10_000


def results_dir() -> Path:
    """eval/results/ is gitignored, so no suite may assume it exists."""
    d = ROOT / "results"
    d.mkdir(parents=True, exist_ok=True)
    return d


def published_dir() -> Path:
    """eval/published/ holds the committed, RFC-mandated gate deliverables."""
    d = ROOT / "published"
    d.mkdir(parents=True, exist_ok=True)
    return d


def eval_bin():
    """Build (once) and return the pgmind-eval binary path."""
    subprocess.run(
        ["cargo", "build", "--release", "--features", "conformance", "--bin", "pgmind-eval"],
        cwd=REPO / "core",
        check=True,
        capture_output=True,
    )
    return REPO / "core" / "target" / "release" / "pgmind-eval"


def run_eval(*args):
    out = subprocess.run([str(eval_bin()), *args], check=True, capture_output=True, text=True)
    return json.loads(out.stdout)


def commonmark_corpus():
    corpus = ROOT / "corpora" / "commonmark" / "spec-0.31.2.json"
    if not corpus.exists():
        corpus.parent.mkdir(parents=True, exist_ok=True)
        print(f"  fetching {COMMONMARK_SPEC_URL} ...")
        urllib.request.urlretrieve(COMMONMARK_SPEC_URL, corpus)
    return corpus


def suite_commonmark_conformance():
    """RFC-002 gate 1: all 652 spec examples, pure CommonMark configuration."""
    return run_eval("conformance", str(commonmark_corpus()))


def suite_round_trip():
    """RFC-002 gate 2: byte-identical tiling on repo docs + 100k fuzz documents."""
    corpus = run_eval(
        "roundtrip",
        str(REPO / "docs"),
        str(REPO / "PGMIND.md"),
        str(REPO / "AUDIT.md"),
        str(REPO / "README.md"),
        str(REPO / "CONTRIBUTING.md"),
    )
    fuzz = run_eval("fuzz-roundtrip", FUZZ_COUNT)
    status = "ok" if corpus["status"] == "ok" and fuzz["status"] == "ok" else "fail"
    return {"status": status, "corpus": corpus, "fuzz": fuzz}


def suite_hash_stability():
    """RFC-002 gate 3: golden vectors for D7 normalization."""
    return run_eval("hash-goldens", str(ROOT / "corpora" / "pgmind" / "hash-goldens.json"))


def suite_vault_syntax_extraction():
    """RFC-002 gate 4: golden corpus for D3/D4 extraction."""
    return run_eval(
        "extraction-goldens", str(ROOT / "corpora" / "pgmind" / "extraction-goldens.json")
    )


def suite_parse_performance():
    """RFC-002 gate 5: pathological constructions parse within fixed bounds."""
    return run_eval("perf")


# ---------------------------------------------------------------------------
# Phase 2 infrastructure: a scratch Postgres cluster with the extension
# installed (RFC-003/004 suites run against real storage).
# ---------------------------------------------------------------------------


def find_pg_config() -> Path:
    env = os.environ.get("PGMIND_PG_CONFIG")
    if env:
        return Path(env)
    candidates = sorted(glob.glob(str(Path.home() / ".pgrx" / "*" / "pgrx-install" / "bin" / "pg_config")))
    if not candidates:
        raise RuntimeError("no pg_config found; set PGMIND_PG_CONFIG or run `make setup`")
    return Path(candidates[-1])


class PgCluster:
    """A throwaway cluster (unix-socket only) with pgmind installed."""

    def __init__(self):
        self.pg_config = find_pg_config()
        self.bindir = Path(
            subprocess.run([str(self.pg_config), "--bindir"], check=True, capture_output=True, text=True).stdout.strip()
        )
        version = subprocess.run([str(self.pg_config), "--version"], check=True, capture_output=True, text=True).stdout
        self.major = re.search(r"PostgreSQL (\d+)", version).group(1)
        self.dir = Path(tempfile.mkdtemp(prefix="pgmind-eval-"))
        self.data = self.dir / "data"
        self.sock = self.dir

    def install_extension(self):
        subprocess.run(
            [
                "cargo", "pgrx", "install", "--release",
                "--no-default-features", "--features", f"pg{self.major}",
                "--pg-config", str(self.pg_config),
            ],
            cwd=REPO / "extension",
            check=True,
            capture_output=True,
        )

    def start(self):
        subprocess.run(
            [str(self.bindir / "initdb"), "-D", str(self.data), "-U", "pgmind", "--no-sync", "-A", "trust"],
            check=True, capture_output=True,
        )
        conf = self.data / "postgresql.conf"
        conf.write_text(
            conf.read_text()
            + f"\nlisten_addresses = ''\nunix_socket_directories = '{self.sock}'\n"
            + "fsync = off\nfull_page_writes = off\n"
        )
        subprocess.run(
            [str(self.bindir / "pg_ctl"), "-D", str(self.data), "-l", str(self.dir / "log"), "start"],
            check=True, capture_output=True,
        )

    def createdb(self, name: str):
        subprocess.run(
            [str(self.bindir / "createdb"), "-h", str(self.sock), "-U", "pgmind", name],
            check=True, capture_output=True,
        )
        self.psql(name, "CREATE EXTENSION pgmind;")

    def psql(self, db: str, sql: str = None, file: Path = None, tuples_only: bool = False) -> str:
        cmd = [str(self.bindir / "psql"), "-X", "-q", "-v", "ON_ERROR_STOP=1",
               "-h", str(self.sock), "-U", "pgmind", "-d", db]
        if tuples_only:
            cmd.append("-tA")
        if file is not None:
            cmd += ["-f", str(file)]
        else:
            cmd += ["-c", sql]
        out = subprocess.run(cmd, capture_output=True, text=True)
        if out.returncode != 0:
            raise RuntimeError(f"psql failed: {out.stderr[-2000:]}")
        return out.stdout

    def dump(self, db: str, target: Path):
        subprocess.run(
            [str(self.bindir / "pg_dump"), "-h", str(self.sock), "-U", "pgmind", "-f", str(target), db],
            check=True, capture_output=True,
        )

    def restore_plain(self, db: str, dump_file: Path):
        """Plain autocommit psql restore — deliberately no --single-transaction
        (RFC-003 §5 gate 6 forbids the crutch)."""
        self.psql(db, file=dump_file)

    def stop(self):
        subprocess.run(
            [str(self.bindir / "pg_ctl"), "-D", str(self.data), "stop", "-m", "immediate"],
            capture_output=True,
        )
        shutil.rmtree(self.dir, ignore_errors=True)


_CLUSTER: "PgCluster | None" = None


def cluster() -> PgCluster:
    global _CLUSTER
    if _CLUSTER is None:
        c = PgCluster()
        print(f"  installing extension (pg{c.major}) + starting scratch cluster ...")
        c.install_extension()
        c.start()
        _CLUSTER = c
    return _CLUSTER


def copy_literal(field: str) -> str:
    """Escape one field for COPY ... FROM STDIN in text format."""
    return (
        field.replace("\\", "\\\\")
        .replace("\t", "\\t")
        .replace("\n", "\\n")
        .replace("\r", "\\r")
    )


# The timed write pass, kept separate from staging so the throughput number
# measures knowledge.write and not the test scaffolding that fed it.
WRITE_PASS_SQL = "SELECT count(knowledge.write(path, src::markdown)) FROM public.rt_staging;"


def stage_vault_sql(docs: list[tuple[str, str]]) -> str:
    """SQL that stages (path, source) pairs via a single COPY. Scaffolding
    only — one statement, one transaction, no knowledge.write involved."""
    lines = [
        "DROP TABLE IF EXISTS public.rt_staging;",
        "CREATE TABLE public.rt_staging (path text PRIMARY KEY, src text);",
        "COPY public.rt_staging (path, src) FROM STDIN;",
    ]
    lines += [f"{copy_literal(p)}\t{copy_literal(s)}" for p, s in docs]
    lines.append("\\.")
    return "\n".join(lines) + "\n"


def load_vault_sql(docs: list[tuple[str, str]]) -> str:
    """Stage + write in one script, for suites that do not time the write."""
    return stage_vault_sql(docs) + WRITE_PASS_SQL + "\n"


def run_sql_file(c: PgCluster, db: str, name: str, sql_text: str):
    """Write a scratch .sql under eval/results/ (gitignored, so it may not
    exist yet), run it, and remove it even when psql fails."""
    path = results_dir() / name
    path.write_text(sql_text)
    try:
        c.psql(db, file=path)
    finally:
        path.unlink(missing_ok=True)


_REPO_DOCS: "list[tuple[str, str]] | None" = None


def repo_docs() -> list[tuple[str, str]]:
    global _REPO_DOCS
    if _REPO_DOCS is None:
        files = [REPO / "PGMIND.md", REPO / "AUDIT.md", REPO / "README.md", REPO / "CONTRIBUTING.md"]
        files += sorted((REPO / "docs").rglob("*.md"))
        docs = []
        for i, f in enumerate(files):
            text = f.read_text()
            if len(text.encode()) < 8 * 1024 * 1024:
                docs.append((f"repo/doc-{i}", text))
        _REPO_DOCS = docs
    return _REPO_DOCS


# ---------------------------------------------------------------------------
# Phase 2 suites (RFC-003 §5, RFC-004 §5)
# ---------------------------------------------------------------------------

# The pg_test binary is the executable form of the identity/extraction/tenant
# goldens; the harness classifies its tests into the RFC's suite names.
PGRX_TEST_SUITES = {
    "identity-semantics": [
        "idempotent_write_returns_head_no_new_revision",
        "edited_paragraph_mints_untouched_carry",
        "pure_reorder_carries_all",
        "duplicate_content_pairs_kth_to_kth",
        "ref_claim_beats_hash_and_collisions_resolve",
        "kind_change_via_claim",
        "update_block_keeps_id_changes_hash",
        "update_item_checkbox_toggle",
        "update_inner_paragraph_directly",
        "move_block_separator_synthesis",
        "move_last_block_earlier_and_back",
        "insert_blocks_at_end_and_anchored",
        "split_first_keeps_id",
        "merge_keeps_chosen_id",
        "child_carried_while_parent_removed",
        "typed_error_sqlstates",
        # A3: identity must not migrate between sections, and a heading rename
        # must still carry its section (RFC-004 A1/A3 pass 2 tiers).
        "section_delete_does_not_recycle_ids_across_sections",
        "heading_rename_carries_section_blocks",
        # A4 provenance schema, incl. split.into and the marker-holder uuid.
        "split_provenance_matches_a4_schema",
        "merge_without_keep_records_provenance",
        "merge_retiree_with_descendants_is_pm006_even_when_keep_is_not_first",
        "move_across_sections_keeps_id_changes_heading_path",
        # RFC-003 D6 splice discipline.
        "insert_preserves_unseparated_neighbouring_tiles",
        "final_block_ops_keep_trailing_trivia",
        "insert_item_level_into_a_quoted_list",
        "block_ops_keep_the_note_row_consistent",
    ],
    "extraction-correctness": [
        "write_read_byte_faithful",
        "resolution_lifecycle_missing_then_resolved",
        "backlinks_tags_orphans",
        "churn_discipline_one_paragraph_edit",
        "read_section_first_match",
        "read_section_ignores_headings_inside_blockquotes",
        "paths_round_trip_through_normalization",
        "notes_glob_matches_literal_stars_and_prefixes",
        "extraction_dedups_and_covers_all_link_kinds",
        "lane_batching_survives_chunk_boundaries",
        "revisions_carry_dense_seq_and_verb",
        # RFC-005 D3/D4: what history records, and what it refuses to record.
        "structural_edits_do_not_write_a_row_per_block",
        "history_records_the_pre_image_of_both_lanes",
        "frames_are_written_at_the_configured_cadence",
        # RFC-005 D3/D8: reconstruction, the error contract, and an invariant
        # checker that can actually fail.
        "every_revision_reconstructs_byte_exactly",
        "blocks_as_of_returns_past_structure",
        "history_errors_distinguish_missing_from_compacted",
        "history_and_diff_report_what_changed",
        "verify_history_catches_a_missing_pre_image",
        # RFC-005 D5: the concurrency contract's single-session half. The
        # interleaving half is the concurrency-isolation suite.
        "cas_precedes_the_idempotence_short_circuit",
        "cas_on_a_missing_note_raises_rather_than_creating",
        "block_ops_honour_expected_head",
        "append_to_section_keeps_both_appends",
        # RFC-005 D6: tombstones, reconstruction-based undelete, rename repair.
        "delete_then_undelete_restores_the_note",
        "move_note_repairs_edges_both_ways",
        "move_onto_an_occupied_path_raises_pm015",
        # RFC-005 D7/D8: erasure that reaches every surface and proves it, and
        # retention that keeps the ledger it compacted.
        "excision_erases_from_every_surface_and_proves_it",
        "excision_refuses_live_content_unless_told_to_remove_it",
        "excision_dry_run_is_the_default",
        "retention_compacts_history_but_keeps_the_ledger",
        "invalid_link_targets_are_reported_as_invalid",
    ],
    "tenant-isolation": [
        "tenant_scoping_and_grant_boundary",
        "malformed_vault_guc_raises_pm001",
        # RFC-005 D2: dump registration and policy coverage, both enumerated
        # from pg_catalog so a table added later is covered by construction.
        "every_pgmind_table_is_dumped_and_tenant_scoped",
    ],
}

_PGRX_TEST_RESULT: "dict | None" = None


def pgrx_test_results() -> dict:
    """Run `cargo pgrx test` once; return {test_name: passed} plus exit status.

    `cargo pgrx test` builds and installs its OWN debug, pg_test-enabled
    artifact over the --release build install_extension() placed: pgrx's
    framework::install_extension shells out to `cargo pgrx install --test`
    against the same pg_config, with PGRX_BUILD_PROFILE defaulting to debug.
    Every later suite does CREATE EXTENSION afterwards, so without the
    reinstall below the capacity and round-trip numbers describe a debug
    build. Restoring the release artifact here keeps that independent of
    SUITES ordering.
    """
    global _PGRX_TEST_RESULT
    if _PGRX_TEST_RESULT is None:
        c = cluster()
        out = subprocess.run(
            ["cargo", "pgrx", "test", f"pg{c.major}"],
            cwd=REPO / "extension", capture_output=True, text=True,
        )
        results = {}
        for line in out.stdout.splitlines():
            m = re.match(r"test \S*tests::pg_(\w+) \.\.\. (ok|FAILED)", line.strip())
            if m:
                results[m.group(1)] = m.group(2) == "ok"
        c.install_extension()  # undo the debug/pg_test install
        _PGRX_TEST_RESULT = {
            "tests": results,
            "raw_ok": out.returncode == 0,
            "stderr_tail": "" if out.returncode == 0 else out.stderr[-2000:],
        }
    return _PGRX_TEST_RESULT


def _listed_tests() -> set:
    return {t for names in PGRX_TEST_SUITES.values() for t in names}


def pg_test_suite(name: str):
    res = pgrx_test_results()
    wanted = PGRX_TEST_SUITES[name]
    missing = [t for t in wanted if t not in res["tests"]]
    failed = [t for t in wanted if not res["tests"].get(t, False)]
    # A pg_test outside every curated list is still evidence about this gate:
    # if the binary as a whole failed, no suite backed by it may report ok.
    unlisted_failed = sorted(
        t for t, passed in res["tests"].items() if not passed and t not in _listed_tests()
    )
    ok = not missing and not failed and res["raw_ok"]
    result = {
        "status": "ok" if ok else "fail",
        "total": len(wanted),
        "passed": len(wanted) - len(failed),
        "missing": missing,
        "failed": failed,
        "pgrx_test_exit_ok": res["raw_ok"],
    }
    if unlisted_failed:
        result["unlisted_failed"] = unlisted_failed
    if not ok and not missing and not failed:
        result["reason"] = "cargo pgrx test exited non-zero outside this suite's named tests"
        result["stderr_tail"] = res["stderr_tail"]
    return result


def suite_identity_semantics():
    """RFC-004 §5 Part A: per-op identity outcomes + A3 carry, 100%."""
    return pg_test_suite("identity-semantics")


def suite_extraction_correctness():
    """RFC-003 §5 gate 2: extraction through storage + resolution lifecycle + churn."""
    return pg_test_suite("extraction-correctness")


def suite_tenant_isolation():
    """RFC-003 §5 gate 4: RLS scoping + grant-anchored boundary."""
    return pg_test_suite("tenant-isolation")


def suite_storage_round_trip():
    """RFC-003 §5 gate 3: write() → read() byte-identical through the tables,
    repo corpus + seeded fuzz sample; verify_note empty everywhere."""
    c = cluster()
    c.createdb("pgmind_rt")
    fuzz = json.loads(
        subprocess.run([str(eval_bin()), "emit-fuzz", STORAGE_FUZZ_COUNT],
                       check=True, capture_output=True, text=True).stdout
    )
    docs = repo_docs() + [(f"fuzz/{i}", d) for i, d in enumerate(fuzz) if d.strip()]
    run_sql_file(c, "pgmind_rt", "_rt_load.sql", load_vault_sql(docs))
    mismatches = int(c.psql(
        "pgmind_rt",
        "SELECT count(*) FROM public.rt_staging s WHERE knowledge.read(s.path)::text <> s.src;",
        tuples_only=True,
    ).strip())
    violations = int(c.psql(
        "pgmind_rt",
        "SELECT count(*) FROM pgmind.note n CROSS JOIN LATERAL pgmind.verify_note(n.id) v;",
        tuples_only=True,
    ).strip())
    ok = mismatches == 0 and violations == 0
    return {"status": "ok" if ok else "fail", "documents": len(docs),
            "read_mismatches": mismatches, "verify_violations": violations}


def synthetic_note(rng: random.Random, i: int, prefix: str = "cap",
                   n: int = CAPACITY_NOTES, extraction: bool = True) -> str:
    """23 blocks, 4 links and 4 tags per note (RFC-003 D8 records this shape).

    `extraction=False` keeps the block and byte shape and removes only the
    links and tags, which is what makes the D8 ablation an ablation: the
    difference between the two passes is extraction and nothing else.
    """
    tag = f"t{i % 20}"
    other = rng.randrange(n)
    parts = [f"# Note {i}\n\n"]
    for p in range(4):
        if extraction:
            parts.append(f"Paragraph {p} of note {i} links [[{prefix}/{(other + p) % n}]] and mentions #{tag}.\n\n")
        else:
            parts.append(f"Paragraph {p} of note {i} links to note {(other + p) % n} and mentions t{i % 20}.\n\n")
    parts.append("## Details\n\n")
    parts.append("".join(f"- point {j} of note {i}\n" for j in range(8)))
    parts.append("\n```\ncode sample\n```\n")
    return "".join(parts)


THROUGHPUT_NOTES = 500
THROUGHPUT_RUNS = 5
GROWTH_STEPS = 8


def timed_create_pass(c: PgCluster, db: str, docs: list[tuple[str, str]]) -> float:
    """Seconds for exactly one knowledge.write() pass over `docs`. Staging is
    loaded first, untimed, so the number describes write() and nothing else."""
    run_sql_file(c, db, "_bench_stage.sql", stage_vault_sql(docs))
    t0 = time.monotonic()
    c.psql(db, WRITE_PASS_SQL)
    return time.monotonic() - t0


def fresh_db(c: PgCluster, name: str):
    c.psql("postgres", f"DROP DATABASE IF EXISTS {name};")
    c.createdb(name)


def write_cost_measurements(c: PgCluster) -> dict:
    """RFC-003 D8's published write cost. Two measurements, deliberately split.

    **Repeatability + attribution.** Every pass writes one corpus into a *fresh
    database*, so the only thing that differs between passes is the note shape:
    a full note, the same note with links and tags removed, and a one-block
    note. That gives a median with a spread (not one sample), and an
    attribution that is a real ablation rather than a difference between two
    differently-loaded databases. Every pass is a create — a rewrite takes the
    other `write_note` branch, so mixing them would measure neither.

    **Growth.** Successive passes into *one* database, recording the vault size
    each pass started from. This exists because the first version of this bench
    ran every pass into one growing database and reported the median: the
    passes were monotonically slowing (3.9 → 9.5 ms/note as the vault filled),
    so the "median" was really the cost at the median vault size, and the
    ablation was charging vault growth to whichever shape ran last. Write cost
    is a function of vault size; D8 publishes the curve instead of one number
    that silently depends on where on it the corpus happened to sit.

    Both run in throwaway databases, dropped afterwards, so the capacity corpus
    whose table sizes are the rest of this gate's deliverable is never polluted.
    """
    shapes = {
        "full": lambda rng, i, pre: synthetic_note(rng, i, prefix=pre, n=THROUGHPUT_NOTES),
        "no_extraction": lambda rng, i, pre: synthetic_note(
            rng, i, prefix=pre, n=THROUGHPUT_NOTES, extraction=False),
        "single_block": lambda rng, i, pre: f"Paragraph of note {i}.\n",
    }

    def corpus(shape, make, run):
        rng = random.Random(0xB0BB1E + run)
        pre = f"bench/{shape}/{run}"
        return [(f"{pre}/{i}", make(rng, i, pre)) for i in range(THROUGHPUT_NOTES)]

    passes: dict = {k: [] for k in shapes}
    for run in range(THROUGHPUT_RUNS):
        for shape, make in shapes.items():
            fresh_db(c, "pgmind_bench")
            elapsed = timed_create_pass(c, "pgmind_bench", corpus(shape, make, run))
            passes[shape].append(round(elapsed * 1000 / THROUGHPUT_NOTES, 3))

    fresh_db(c, "pgmind_bench")
    growth = []
    for step in range(GROWTH_STEPS):
        elapsed = timed_create_pass(c, "pgmind_bench", corpus("full", shapes["full"], 100 + step))
        growth.append({"notes_already_in_vault": step * THROUGHPUT_NOTES,
                       "ms_per_note": round(elapsed * 1000 / THROUGHPUT_NOTES, 3)})
    c.psql("postgres", "DROP DATABASE pgmind_bench;")

    def med(xs):
        return sorted(xs)[len(xs) // 2]  # odd run counts only

    full, no_ext, single = (med(passes[k]) for k in shapes)
    spread = round(max(passes["full"]) - min(passes["full"]), 3)
    return {
        "procedure": (
            f"{THROUGHPUT_RUNS} rounds x 3 shapes, each pass writing a "
            f"{THROUGHPUT_NOTES}-note corpus into an EMPTY database (every write a create, "
            "release build, single connection, staging loaded untimed); then "
            f"{GROWTH_STEPS} successive passes into one database for the growth curve"
        ),
        "notes_per_pass": THROUGHPUT_NOTES,
        "ms_per_note_by_pass_empty_vault": passes,
        "ms_per_note_median": full,
        "ms_per_note_range": [min(passes["full"]), max(passes["full"])],
        "notes_per_s_median": round(1000 / full, 1),
        "measured_at_vault_size": 0,
        "attribution_ms_per_note": {
            "extraction": round(full - no_ext, 3),
            "blocks_and_tiles_beyond_the_first": round(no_ext - single, 3),
            "per_note_fixed": round(single, 3),
            # An attribution term smaller than the run-to-run spread of the
            # full shape is not resolvable by this bench, and D8 may not quote
            # it as if it were. Recorded rather than hidden.
            "run_to_run_spread_of_full": spread,
        },
        "growth_ms_per_note_by_vault_size": growth,
    }


def suite_capacity_model():
    """RFC-003 §5 gate 5 / D8: measured bytes + throughput + latency, published
    honestly with extrapolation to the plan-§14 design target."""
    c = cluster()
    # Write cost first, in its own database: it is the number D8 quotes, and it
    # must not be measured against a cluster already holding the 10k corpus.
    write_cost = write_cost_measurements(c)
    c.createdb("pgmind_cap")
    rng = random.Random(0xC0FFEE)
    docs = [(f"cap/{i}", synthetic_note(rng, i)) for i in range(CAPACITY_NOTES)]
    # Staging is test scaffolding: load it first, untimed, so the published
    # throughput describes knowledge.write and nothing else.
    run_sql_file(c, "pgmind_cap", "_cap_load.sql", stage_vault_sql(docs))
    t0 = time.monotonic()
    c.psql("pgmind_cap", WRITE_PASS_SQL)
    elapsed = time.monotonic() - t0

    sizes = json.loads(c.psql("pgmind_cap", """
        SELECT json_object_agg(t.relname, json_build_object(
                 'total_bytes', pg_total_relation_size(t.oid),
                 'heap_bytes', pg_relation_size(t.oid),
                 'index_bytes', pg_indexes_size(t.oid)))
        FROM (SELECT c.oid, c.relname FROM pg_class c
              JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = 'pgmind' AND c.relkind = 'r') t;
    """, tuples_only=True).strip())
    counts = json.loads(c.psql("pgmind_cap", """
        SELECT row_to_json(s) FROM knowledge.stats() s;
    """, tuples_only=True).strip())
    latencies = json.loads(c.psql("pgmind_cap", f"""
        CREATE TEMP TABLE lat (fn text, ms double precision);
        DO $$
        DECLARE t0 timestamptz; i int;
        BEGIN
          FOR i IN 1..100 LOOP
            t0 := clock_timestamp();
            PERFORM knowledge.read('cap/' || (i * 97 % {CAPACITY_NOTES}));
            INSERT INTO lat VALUES ('read', extract(epoch FROM clock_timestamp() - t0) * 1000);
            t0 := clock_timestamp();
            PERFORM count(*) FROM knowledge.backlinks('cap/' || (i * 89 % {CAPACITY_NOTES}));
            INSERT INTO lat VALUES ('backlinks', extract(epoch FROM clock_timestamp() - t0) * 1000);
            t0 := clock_timestamp();
            PERFORM count(*) FROM knowledge.tagged('t' || (i % 20));
            INSERT INTO lat VALUES ('tagged', extract(epoch FROM clock_timestamp() - t0) * 1000);
          END LOOP;
        END $$;
        SELECT json_object_agg(fn, p95) FROM (
          SELECT fn, percentile_cont(0.95) WITHIN GROUP (ORDER BY ms) AS p95
          FROM lat GROUP BY fn) x;
    """, tuples_only=True).strip().splitlines()[-1])
    violations = int(c.psql(
        "pgmind_cap",
        "SELECT count(*) FROM pgmind.note n CROSS JOIN LATERAL pgmind.verify_note(n.id) v;",
        tuples_only=True,
    ).strip())

    blocks = counts["blocks"]
    total_bytes = sum(v["total_bytes"] for v in sizes.values())
    # Gate 5 publishes honest numbers rather than asserting a threshold, but a
    # degenerate corpus is a real failure, not an honest measurement.
    problems = []
    if counts["notes"] != CAPACITY_NOTES:
        problems.append(f"stored {counts['notes']} notes, expected {CAPACITY_NOTES}")
    if blocks <= 0:
        problems.append("no blocks stored")
    if elapsed <= 0:
        problems.append("write pass took no measurable time")
    if violations:
        problems.append(f"{violations} verify_note violations")
    report = {
        "status": "fail" if problems else "ok",
        "scale": {"notes": counts["notes"], "blocks": blocks,
                  "edges": counts["edges_resolved"] + counts["edges_dangling"],
                  "tags": counts["tags"]},
        "design_target_notes_per_s": 2000,
        "bytes": sizes,
        "latency_p95_ms": latencies,
        "verify_violations": violations,
        # The single 10k-note pass this database was built by. Kept for
        # transparency; it is one sample, and `write_cost` is what D8 quotes.
        "bulk_pass_notes_per_s_one_sample": round(counts["notes"] / elapsed, 1),
        "write_cost": write_cost,
    }
    if problems:
        report["reason"] = "; ".join(problems)
    else:
        report["write_throughput_notes_per_s"] = write_cost["notes_per_s_median"]
        report["bytes_per_block_all_in"] = round(total_bytes / blocks, 1)
        report["extrapolation_100k_notes_10m_blocks"] = {
            "assumption": "linear in blocks; revision-load behavior modeled only until Phase 3 measures it",
            "projected_total_gb": round(total_bytes / blocks * 10_000_000 / 1e9, 2),
        }
    # RFC-003 §5 gate 5 names this file as the published deliverable;
    # eval/results/ is gitignored and cannot serve that role.
    (published_dir() / "capacity-model-v1.json").write_text(json.dumps(report, indent=2) + "\n")
    (results_dir() / "capacity-model.json").write_text(json.dumps(report, indent=2) + "\n")
    return report


def suite_dump_restore():
    """RFC-003 §5 gate 6: pg_dump → plain autocommit psql restore → equal
    counts, advancing sequences, verify_note clean, post-restore write works."""
    c = cluster()
    c.createdb("pgmind_ref")
    run_sql_file(c, "pgmind_ref", "_ref_load.sql", load_vault_sql(repo_docs()))

    dump_file = c.dir / "ref.dump.sql"
    c.dump("pgmind_ref", dump_file)
    subprocess.run([str(c.bindir / "createdb"), "-h", str(c.sock), "-U", "pgmind", "pgmind_restored"],
                   check=True, capture_output=True)
    c.restore_plain("pgmind_restored", dump_file)

    count_sql = ("SELECT json_build_object("
                 "'note', (SELECT count(*) FROM pgmind.note),"
                 "'revision', (SELECT count(*) FROM pgmind.revision),"
                 "'tile', (SELECT count(*) FROM pgmind.tile),"
                 "'block', (SELECT count(*) FROM pgmind.block),"
                 "'edge', (SELECT count(*) FROM pgmind.edge),"
                 "'tag', (SELECT count(*) FROM pgmind.tag));")
    before = json.loads(c.psql("pgmind_ref", count_sql, tuples_only=True).strip())
    after = json.loads(c.psql("pgmind_restored", count_sql, tuples_only=True).strip())
    violations = int(c.psql(
        "pgmind_restored",
        "SELECT count(*) FROM pgmind.note n CROSS JOIN LATERAL pgmind.verify_note(n.id) v;",
        tuples_only=True,
    ).strip())
    # sequences must advance past restored PKs: a post-restore write succeeds
    post = c.psql(
        "pgmind_restored",
        "SELECT knowledge.write('post-restore/probe', ('after restore [[repo/doc-0]] #ok')::markdown);",
        tuples_only=True,
    ).strip()
    ok = before == after and violations == 0 and len(post) == 36
    return {"status": "ok" if ok else "fail", "counts_before": before,
            "counts_after": after, "verify_violations": violations,
            "post_restore_write": bool(len(post) == 36)}


SUITES = {
    "commonmark-conformance": suite_commonmark_conformance,
    "round-trip": suite_round_trip,
    "hash-stability": suite_hash_stability,
    "vault-syntax-extraction": suite_vault_syntax_extraction,
    "parse-performance": suite_parse_performance,
    # Phase 2 (RFC-003 §5 / RFC-004 §5)
    "identity-semantics": suite_identity_semantics,
    "extraction-correctness": suite_extraction_correctness,
    "storage-round-trip": suite_storage_round_trip,
    "tenant-isolation": suite_tenant_isolation,
    "capacity-model": suite_capacity_model,
    "dump-restore": suite_dump_restore,
    # Phase 3 (RFC-004/005): rebinding-edit-corpus, concurrency, storage-growth
    # Phase 4 (RFC-006):     sync-round-trip (incl. unicode/case collisions), torture
    # Phase 5 (RFC-007/008): context-determinism, quality-per-token
}


def main() -> int:
    report = {
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "suites": {},
    }
    failed = False
    try:
        for name, fn in SUITES.items():
            print(f"suite: {name}")
            try:
                result = fn()
            except Exception as exc:  # a crashed suite is a failed suite
                result = {"status": "fail", "error": repr(exc)}
            report["suites"][name] = result
            print(f"  -> {result['status']}" + (f" ({result.get('reason')})" if result.get("reason") else ""))
            failed |= result["status"] == "fail"
    finally:
        # KeyboardInterrupt/SystemExit must not leak a live postmaster and a
        # multi-GB scratch datadir.
        if _CLUSTER is not None:
            _CLUSTER.stop()

    out = results_dir() / "latest.json"
    out.write_text(json.dumps(report, indent=2) + "\n")
    print(f"report: {out}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
