#!/usr/bin/env bash
# Rebuild the reference schema eunha is checked against.
#
# `mastodon/schema.rb` is upstream's own file; this executes it with the real
# ActiveRecord schema DSL against a scratch Postgres database and dumps the
# result to `mastodon/schema.sql`. That dump is what a Mastodon actually has —
# every default, index expression and constraint — rather than what a parser of
# ours believes the file means.
#
# Only needed when adopting a new Mastodon release. Requires mise (for Ruby) and
# a local Postgres.
#
# Usage: scripts/build_mastodon_schema.sh [schema.rb]

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCHEMA_RB="${1:-$ROOT/mastodon/schema.rb}"
OUT="$ROOT/mastodon/schema.sql"
DB="mastodon_schema_reference"
BUILD="$ROOT/scripts/mastodon-schema"

if [ ! -f "$SCHEMA_RB" ]; then
    echo "no such schema file: $SCHEMA_RB" >&2
    exit 1
fi

echo "==> Installing the Ruby toolchain (activerecord + pg, not Mastodon's bundle)"
( cd "$BUILD" && mise install && mise exec -- bundle install --quiet )

echo "==> Building a reference database from $(basename "$SCHEMA_RB")"
dropdb --if-exists "$DB"
createdb "$DB"
( cd "$BUILD" \
  && DATABASE_URL="postgres:///$DB" mise exec -- bundle exec ruby load_schema.rb "$SCHEMA_RB" )

echo "==> Dumping to $(basename "$OUT")"
# `--no-comments` and no ownership: what is being compared is structure, and the
# rest is noise that differs between machines.
# `\restrict` carries a random token per run, so strip it: the file should only
# change when the schema does, not every time it is regenerated. Dropping it
# also leaves plain SQL that anything can execute, not just psql.
pg_dump --schema-only --no-owner --no-privileges --no-comments -n public -d "$DB" \
    | grep -vE '^\\(un)?restrict ' > "$OUT"

echo "==> Recording the reference structure"
# `check` compares structure, not text, so the dump is for humans and this is
# what the tests read.
( cd "$ROOT" && SQLX_OFFLINE=true cargo run --quiet --bin eunha-schema -- \
    record-reference --database-url "postgres:///$DB" )

dropdb "$DB"

echo
echo "Wrote $OUT and mastodon/schema.json"
echo "Check a database against it with: mise run schema:check"
