#!/bin/sh
# Export a pgmind vault to a folder of markdown. Handbook law 4: you can leave.
#
#   ./scripts/export-vault.sh DATABASE DIRECTORY
#
# This is the reference implementation of "migration out", and it is gated:
# eval/harness.py's `folder-round-trip` suite runs it and its import twin over an
# adversarial path corpus and asserts the bytes survive.
#
# It refuses rather than half-exporting. A vault holding both `Projects/Auth` and
# `projects/auth` cannot become a folder on a case-insensitive filesystem without
# one silently overwriting the other, so nothing is written and the collision is
# named. The same applies to paths differing only by Unicode normal form.
#
# Requires: psql, perl (see EXACTNESS below).

set -eu

DB=${1:?usage: export-vault.sh DATABASE DIRECTORY}
DIR=${2:?usage: export-vault.sh DATABASE DIRECTORY}

command -v perl >/dev/null 2>&1 || {
    echo "export-vault: perl is required (exact trailing-newline handling)" >&2
    exit 1
}

psql_q() { psql -X -q -v ON_ERROR_STOP=1 -Aqt -d "$DB" "$@"; }

# --- Refuse before writing anything -----------------------------------------
#
# Two pgmind paths differing only by case, or only by Unicode normal form, are
# distinct notes but one file on APFS, NTFS and HFS+. Detected in SQL, where
# lower() and normalize() are locale- and Unicode-correct; the shell's tr is not.
collisions=$(psql_q <<'SQL'
SELECT string_agg(path, ' <-> ' ORDER BY path)
  FROM knowledge.notes()
 GROUP BY lower(normalize(path, NFC))
HAVING count(*) > 1;
SQL
)
if [ -n "$collisions" ]; then
    echo "export-vault: refusing to write — these paths become one file on a" >&2
    echo "case- or normalization-insensitive filesystem:" >&2
    echo "$collisions" | sed 's/^/  /' >&2
    exit 3
fi

# Not fatal: POSIX takes these happily, but the folder will not check out on
# Windows. Say so once rather than shipping a repo that is broken for half a team.
hostile=$(psql_q <<'SQL'
SELECT string_agg(path, ', ' ORDER BY path) FROM knowledge.notes()
 WHERE path ~ '[<>:"|?*]';
SQL
)
if [ -n "$hostile" ]; then
    echo "export-vault: warning — these paths contain characters Windows forbids in" >&2
    echo "  filenames; the folder will not check out there: $hostile" >&2
fi

# --- Write -------------------------------------------------------------------
mkdir -p "$DIR"
list=$(mktemp) && trap 'rm -f "$list" "$list.body"' EXIT
psql_q <<'SQL' > "$list"
SELECT path FROM knowledge.notes() ORDER BY path;
SQL

count=0
# A path cannot contain a newline: RFC-002 D8 forbids control characters in a
# segment (core/src/path.rs:43), so line-based iteration is safe by grammar.
# Redirecting the file in (rather than piping) keeps the loop in this shell, so
# `count` survives it and a failure can still abort the script.
while IFS= read -r p; do
    [ -n "$p" ] || continue
    mkdir -p "$DIR/$(dirname "$p")"

    # QUOTING: the path arrives as a psql variable and is interpolated with :'p',
    # which quotes and escapes it. Interpolating it into the SQL text is how a
    # note called "it's mine" becomes a syntax error and a zero-byte file.
    # `-c` does not expand variables, so the statement comes in on stdin.
    #
    # The status is checked before the bytes are used: piping psql straight into
    # perl would discard its exit code and write a truncated file as if it worked.
    psql -X -q -v ON_ERROR_STOP=1 -Aqt -d "$DB" -v p="$p" > "$list.body" <<'SQL'
SELECT knowledge.read(:'p');
SQL

    # EXACTNESS: psql appends a record separator to every row, so stdout is the
    # note plus one "\n" it may not have had. This strips exactly one trailing
    # newline — not all of them, which would corrupt a note ending in a blank line.
    perl -0777 -pe 's/\n\z//' < "$list.body" > "$DIR/$p.md"
    count=$((count + 1))
done < "$list"

echo "export-vault: wrote $count note(s) to $DIR"
