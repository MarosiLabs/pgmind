#!/bin/sh
# Import a folder of markdown into a pgmind vault. Handbook law 4: you can arrive.
#
#   ./scripts/import-vault.sh DIRECTORY DATABASE
#
# The twin of export-vault.sh, gated by the same `folder-round-trip` suite.
#
# Every `*.md` file becomes the note at its path with exactly one `.md` removed, so
# `notes/readme.md.md` imports as `notes/readme.md` and exports back the same way.
# Files that are not `*.md` are not notes and are ignored. Paths the vault grammar
# rejects are reported and skipped, never silently renamed into validity.
#
# One transaction per note. A byte-identical write creates no revision
# (extension/src/write.rs:946), so a run interrupted halfway is resumed by running
# it again rather than by undoing anything.

set -eu

DIR=${1:?usage: import-vault.sh DIRECTORY DATABASE}
DB=${2:?usage: import-vault.sh DIRECTORY DATABASE}

cd "$DIR"
list=$(mktemp) && trap 'rm -f "$list"' EXIT

# Sorted so two runs over one folder produce the same revision order. A path
# cannot contain a newline (RFC-002 D8), so line-based iteration is safe.
find . -type f -name '*.md' | sed 's|^\./||' | LC_ALL=C sort > "$list"

imported=0
skipped=0
while IFS= read -r f; do
    [ -n "$f" ] || continue
    p=${f%.md}

    # macOS hands back NFD; the vault stores NFC. Normalize before validating so a
    # legitimate "café" is not reported as a bad path. `-c` does not expand psql
    # variables, so every parameterized statement comes in on stdin.
    valid=$(psql -X -q -v ON_ERROR_STOP=1 -Aqt -d "$DB" -v f="$p" <<'SQL'
SELECT pgmind.path_is_valid(pgmind.path_normalize(:'f'));
SQL
)
    if [ "$valid" != "t" ]; then
        echo "import-vault: skipping — not a valid note path: $p" >&2
        skipped=$((skipped + 1))
        continue
    fi

    # EXACTNESS: psql's backtick \set strips every trailing newline from the
    # command's output, which would silently drop the blank line a note ends on.
    # Appending a sentinel byte leaves nothing at the end to strip, and
    # left(:'doc', -1) removes it again inside the server.
    # psql runs the backtick through a shell, so the filename travels in an
    # environment variable rather than being pasted into the command text.
    PGMIND_FILE="$f" psql -X -q -v ON_ERROR_STOP=1 -t -d "$DB" -v f="$p" >/dev/null <<'SQL'
\set doc `cat "$PGMIND_FILE"; printf X`
SELECT knowledge.write(pgmind.path_normalize(:'f'), left(:'doc', -1)::markdown);
SQL
    imported=$((imported + 1))
done < "$list"

echo "import-vault: imported $imported note(s), skipped $skipped"
[ "$skipped" -eq 0 ]
