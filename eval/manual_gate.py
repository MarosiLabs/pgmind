"""The website manual's gate.

The manual (`website/docs/*.html`) claims that every SQL example in it was executed
against a seeded vault and its real output pasted. That claim was true when the pages
were written and had nothing keeping it true. This module is what keeps it true: it
rebuilds the vault from `eval/manual/seed.sql`, replays every SQL block in every page
in document order, and fails when a block's behaviour stops matching what the page
shows next to it.

Two gates live here:

  manual-sql        every SQL block runs; a block whose captured output shows an
                    ERROR must still raise, with the same PM code; every other
                    block must succeed.
  manual-inventory  every knowledge.*/pgmind.* identifier the manual names exists in
                    pg_proc, unless it sits inside a block the page has badged as not
                    yet implemented.

Blocks the SQL gate cannot run are never skipped silently — each one is reported with
its reason, and the counts appear in the published result.
"""

from __future__ import annotations

import html
import re
import subprocess
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DOCS = REPO / "website" / "docs"
SEED = REPO / "eval" / "manual" / "seed.sql"

# Page names are relative to website/docs, except "../index.html" — the landing page.
# It is here because it is the most-read page on the site and was, until it was added,
# the only one nothing checked: it shipped an invented `ERROR: pgmind: head moved`
# that no code path can produce, and a reader grepping their logs for it found nothing.
PAGES = ["../index.html", "index.html", "quickstart.html", "concepts.html",
         "internals.html", "sql.html", "cookbook.html"]

# A caption marks a block as unrunnable when it says so. These are the only two
# reasons a block may be excluded, and both are visible to the reader of the page.
CAP_ILLUSTRATIVE = "illustrative"
CAP_NOT_IMPLEMENTED = "not implemented"

SENTINEL = "===PGMIND-BLOCK"


class Block:
    def __init__(self, page: str, ordinal: int, cap: str, lang: str, code: str, out: str):
        self.page = page
        self.ordinal = ordinal
        self.cap = cap
        self.lang = lang
        self.code = code
        self.out = out

    @property
    def ref(self) -> str:
        return f"{self.page}#{self.ordinal}"

    @property
    def expects_error(self) -> bool:
        return "ERROR:" in self.out

    @property
    def expected_codes(self) -> set:
        """PM codes the captured output shows, if any."""
        return set(re.findall(r"\bPM0\d\d\b", self.out))

    @property
    def fresh_session(self) -> bool:
        """The caption promises the reader a session that has not loaded the library.

        `SHOW pgmind.frame_every` failing is a documented behaviour, not a defect, so
        the block gets its own connection rather than being skipped.
        """
        low = self.cap.lower()
        return "fresh session" in low or "new connection" in low

    def skip_reason(self) -> str | None:
        if self.lang == "unlabelled":
            return "unlabelled block (diagram, not runnable)"
        if self.lang != "sql":
            return f"{self.lang} block" + (f" — {self.cap}" if self.cap else "")
        low = self.cap.lower()
        if CAP_ILLUSTRATIVE in low:
            return "captioned illustrative"
        if CAP_NOT_IMPLEMENTED in low:
            return "captioned not implemented"
        # A block that deliberately holds a lock open across two sessions cannot be
        # replayed down one connection: session B would join session A's transaction
        # instead of contending with it. The concurrency behaviour these two blocks
        # show is gated for real in suite_concurrency(), not here.
        if re.search(r"\bsession [ab]\b", low):
            return "two-session lock demo (covered by suite_concurrency)"
        return None


def strip_tags(fragment: str) -> str:
    return html.unescape(re.sub(r"<[^>]+>", "", fragment))


def psql_cmd(cluster, db: str) -> list:
    """Base psql invocation for a cluster, honouring an optional port and role.

    The eval harness hands us a unix-socket cluster owned by `pgmind`; a developer
    driving this module against a `cargo pgrx run` cluster has a TCP port and their
    own role. Both go through here so the two paths cannot drift.
    """
    cmd = [str(cluster.bindir / "psql"), "-X", "-q",
           "-h", str(cluster.sock), "-U", getattr(cluster, "user", "pgmind"), "-d", db]
    port = getattr(cluster, "port", None)
    if port:
        cmd += ["-p", str(port)]
    return cmd


DIV = re.compile(r"<div\b|</div>")


def code_divs(src: str):
    """Yield the inner HTML of every <div class="code">, counting nested divs.

    The wrapper holds a nested <div class="cap">, so a non-greedy match to the first
    </div> would cut the block off at the caption.
    """
    for start in (m.end() for m in re.finditer(r'<div class="code">', src)):
        depth = 1
        pos = start
        for m in DIV.finditer(src, start):
            depth += 1 if m.group(0) == "<div" else -1
            if depth == 0:
                pos = m.start()
                break
        yield src[start:pos]


def page_path(page: str):
    """Resolve a PAGES entry. `../index.html` reaches the landing page."""
    return (DOCS / page).resolve()


def parse_page(page: str) -> list:
    """Every <div class="code"> on the page, in document order."""
    src = page_path(page).read_text()
    blocks = []
    for ordinal, chunk in enumerate(code_divs(src), 1):
        cap_m = re.search(r'<div class="cap">(.*?)</div>', chunk, re.S)
        cap = strip_tags(cap_m.group(1)) if cap_m else ""
        # An unlabelled <code> is a diagram, not something to run — but it is still
        # counted and reported, so the gate can never quietly lose a block it failed
        # to recognise.
        code_m = re.search(r'<code(?: class="(\w+)")?>(.*?)</code>', chunk, re.S)
        if not code_m:
            continue
        out_m = re.search(r'<pre class="out"><code[^>]*>(.*?)</code></pre>', chunk, re.S)
        blocks.append(Block(page, ordinal, cap.strip(), code_m.group(1) or "unlabelled",
                            strip_tags(code_m.group(2)),
                            strip_tags(out_m.group(1)) if out_m else ""))
    return blocks


# ---------------------------------------------------------------------------
# manual-sql
# ---------------------------------------------------------------------------


def verdict(block: "Block", segment: str) -> str | None:
    """Did this block behave the way the page says it does? None means yes.

    Kept as a pure function of (what the page shows, what the server said) so the
    gate-selftest can prove it fails without needing a database to lie to it.
    """
    raised = bool(re.search(r"^(?:psql:[^:]*:\d+: )?ERROR:", segment, re.M))
    if block.expects_error and not raised:
        return f"{block.ref}: page shows an ERROR, the block succeeded"
    if not block.expects_error and raised:
        first = next((l for l in segment.splitlines() if "ERROR:" in l), segment[:200])
        return f"{block.ref}: unexpected {first.strip()}"
    if block.expects_error and raised:
        want = block.expected_codes
        got = set(re.findall(r"\bPM0\d\d\b", segment))
        if want and not (want & got):
            return (f"{block.ref}: page shows {sorted(want)}, "
                    f"raised {sorted(got) or 'no PM code'}")
    return None


def run_segment(cluster, db: str, blocks: list, path: Path, segments: dict):
    """One psql process over a run of blocks; record each block's stderr.

    `\\warn` puts the sentinel on stderr, the same stream the errors arrive on, so
    attribution survives without having to interleave two captured streams.
    """
    script = []
    for b in blocks:
        script.append(f"\\warn {SENTINEL} {b.ordinal} ===")
        # A reader on a fresh database runs this; the seeded replay has it already.
        script.append(b.code.replace("CREATE EXTENSION pgmind;",
                                     "CREATE EXTENSION IF NOT EXISTS pgmind;"))
    path.write_text("\n".join(script) + "\n")
    proc = subprocess.run(
        psql_cmd(cluster, db) + ["-v", "ON_ERROR_STOP=0", "-f", str(path)],
        capture_output=True, text=True,
    )
    current = None
    for line in proc.stderr.splitlines():
        if SENTINEL in line:
            current = int(line.split(SENTINEL)[1].split("===")[0].strip())
            segments[current] = []
        elif current is not None:
            segments[current].append(line)


def replay_page(cluster, db: str, blocks: list, tmpdir: Path) -> list:
    """Replay a page in document order, one connection unless a caption says otherwise.

    Session state (SET, BEGIN, LOAD) has to survive from block to block, so a run of
    blocks shares a psql process. A block captioned "fresh session" ends the run and
    gets a process of its own — otherwise the library would already be loaded and the
    unrecognized-parameter error the page documents could never reproduce.
    """
    runs = [[]]
    for b in blocks:
        if b.fresh_session:
            runs += [[b], []]
        else:
            runs[-1].append(b)

    segments = {}
    for i, run in enumerate(r for r in runs if r):
        run_segment(cluster, db, run, tmpdir / f"{db}.{i}.sql", segments)

    return [v for v in (verdict(b, "\n".join(segments.get(b.ordinal, [])))
                        for b in blocks) if v]


def suite_manual_sql(cluster, tmpdir: Path) -> dict:
    """Every SQL block in the manual, replayed against a freshly seeded vault."""
    ran = skipped = 0
    failures = []
    skips = []
    for page in PAGES:
        blocks = parse_page(page)
        runnable = []
        for b in blocks:
            reason = b.skip_reason()
            if reason:
                skipped += 1
                skips.append(f"{b.ref}: {reason}")
            else:
                runnable.append(b)
        if not runnable:
            continue
        db = "manual_" + page.replace(".html", "").replace("-", "_")
        cluster.psql("postgres", f"DROP DATABASE IF EXISTS {db};")
        cluster.createdb(db)
        cluster.psql(db, file=SEED)
        ran += len(runnable)
        failures += replay_page(cluster, db, runnable, tmpdir)
        cluster.psql("postgres", f"DROP DATABASE IF EXISTS {db};")

    return {
        "name": "manual-sql",
        "ok": not failures,
        "pages": len(PAGES),
        "blocks_run": ran,
        "blocks_skipped": skipped,
        # The one accommodation the replay makes to a page's text, recorded rather
        # than hidden: the seed has already created the extension.
        "create_extension_made_idempotent": True,
        "skips": skips,
        "failures": failures,
    }


# ---------------------------------------------------------------------------
# manual-inventory
# ---------------------------------------------------------------------------

IDENT = re.compile(r"\b(knowledge|pgmind)\.([a-z_][a-z0-9_]*)\b")

# Names that are not functions: schema-qualified tables, types, GUCs and columns the
# manual legitimately mentions. Each must exist as *something* — checked below.
def catalog(cluster, db: str) -> dict:
    def rows(sql):
        return [r for r in cluster.psql(db, sql, tuples_only=True).splitlines() if r]
    return {
        "functions": set(rows(
            "SELECT n.nspname||'.'||p.proname FROM pg_proc p "
            "JOIN pg_namespace n ON n.oid=p.pronamespace "
            "WHERE n.nspname IN ('knowledge','pgmind');")),
        "relations": set(rows(
            "SELECT n.nspname||'.'||c.relname FROM pg_class c "
            "JOIN pg_namespace n ON n.oid=c.relnamespace "
            "WHERE n.nspname IN ('knowledge','pgmind');")),
        "types": set(rows(
            "SELECT n.nspname||'.'||t.typname FROM pg_type t "
            "JOIN pg_namespace n ON n.oid=t.typnamespace "
            "WHERE n.nspname IN ('knowledge','pgmind');")),
        # The GUCs do not exist until the library loads — the manual documents this,
        # and the gate has to honour it or it would report every GUC as invented.
        "gucs": set(rows(
            "LOAD 'pgmind'; SELECT name FROM pg_settings WHERE name LIKE 'pgmind.%';")),
    }


# A region may name something that does not exist only if it also tells the reader so.
# These are the page's own visible ways of saying it — a badge, a caption, or plain
# words — so the gate reads the same signal the reader does.
DISCLAIMERS = re.compile(
    r'badge next|does not exist|not implemented|illustrative|not yet|'
    r'reserved|Phase [4-9]|shipped, renamed|folded in',
    re.I)

# Block-level starts. A table row is one region, so the badge in the second cell
# covers the function named in the first.
REGION = re.compile(r"(?=<(?:tr|p|li|pre|h[1-6]|dt|dd|summary|figcaption)\b)")


def scan_page(src: str, everything: set):
    """(names that resolve to nothing, names visibly declared absent, names checked).

    Split out from the suite so the gate-selftest can hand it a fabricated page and
    prove it reports an invented name -- and stays quiet about a disclaimed one.
    """
    # Clone URLs are not schema qualifications: .../MarosiLabs/pgmind.git
    src = re.sub(r"https?://\S+", "", src)
    # The pages use non-breaking spaces to keep "Phase 5" and friends from wrapping;
    # a disclaimer must not become invisible to the gate over typography.
    src = src.replace("\u00a0", " ")
    missing, declared_absent, checked = [], set(), 0
    for region in REGION.split(src):
        excused = DISCLAIMERS.search(region)
        for m in IDENT.finditer(strip_tags(region)):
            name = f"{m.group(1)}.{m.group(2)}"
            checked += 1
            if name in everything:
                continue
            (declared_absent.add(name) if excused else missing.append(name))
    return missing, declared_absent, checked


def suite_manual_inventory(cluster, db: str) -> dict:
    """No invented API: every name resolves, or is visibly declared absent.

    The manual's own rule is that nothing unimplemented appears except in a clearly
    badged block. This checks exactly that rule, region by region: a `knowledge.*` or
    `pgmind.*` name must either resolve in a live catalog, or sit in a region that
    tells the reader it does not.

    Only this direction is gated. The reverse — a name still badged absent after it
    ships — is not, because at row granularity it cannot tell a missing function from
    a missing *overload* of a function that exists, and a gate that cries wolf gets
    ignored. §2.2 of the manual plan is the human check for that direction.
    """
    known = catalog(cluster, db)
    everything = known["functions"] | known["relations"] | known["types"] | known["gucs"]
    missing, declared_absent = [], set()
    checked = 0
    for page in PAGES:
        m, d, n = scan_page(page_path(page).read_text(), everything)
        missing += [f"{page}: {name}" for name in m]
        declared_absent |= d
        checked += n
    # The other direction: a function that ships without a reference entry is just as
    # much a documentation defect as one that is documented without shipping, and it
    # is the direction nobody notices — the pages stay true, they just stop being
    # complete. Overloads share one entry, so this compares names, not signatures.
    entries = set(re.findall(r'id="fn-([a-z0-9-]+)"', (DOCS / "sql.html").read_text()))
    undocumented = sorted(
        name for name in known["functions"]
        if name.replace(".", "-").replace("_", "-") not in entries
        and not name.endswith(".raise_error"))  # internal, REVOKEd from PUBLIC

    return {
        "name": "manual-inventory",
        "ok": not missing and not undocumented,
        "identifiers_checked": checked,
        "functions_in_catalog": len(known["functions"]),
        "undocumented_functions": undocumented,
        "catalog_size": {k: len(v) for k, v in known.items()},
        "declared_absent": sorted(declared_absent),
        "unknown": sorted(set(missing)),
    }
