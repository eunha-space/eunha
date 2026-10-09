#!/bin/bash
# Compare eunha against a live Mastodon, rather than against a reading of it.
#
# Every other check in this repo compares eunha to what upstream's source says.
# This one asks upstream: the same request goes to both servers and the two
# responses are compared. Nothing here encodes what the answer should be, so a
# rule misread while writing a test cannot pass here too.
#
# The official image is used deliberately. Building Mastodon from source on
# macOS means libidn, OpenSSL headers for hiredis-client, libvips, and a `pg`
# gem that segfaults against Postgres 18 — all of it already solved in the image.
#
# Needs a container runtime (OrbStack, Docker Desktop, colima). Either point it
# at an eunha you are already running, or give it no arguments and it will bring
# up one of its own against a scratch database:
#
#   scripts/differential_test.sh                              # its own eunha
#   scripts/differential_test.sh http://localhost:3001 TOKEN  # one you run
#
# The first form exists so that CI and a developer run the same path. The eunha
# side needs a migrated database, two accounts and a token, and while that lived
# in nobody's script it lived in nobody's memory either — which is how this
# harness went weeks without being run at all.
#
# Set EUNHA_OTHER_ID to a second account on the eunha side to include the
# interaction verbs — follow, block, mute — which otherwise compare nothing.
# It is set for you when this script brings up its own eunha.
#
# For that form: EUNHA_PORT (3001), EUNHA_DB (eunha_differential),
# EUNHA_DATABASE_URL (a local socket to EUNHA_DB), EUNHA_REDIS_URL, and
# DIFFERENTIAL_WORK_DIR. `createdb`, `dropdb` and `psql` take their connection
# from the usual PG* variables, so a Postgres that wants a host and a password —
# CI's — is reached by setting those alongside EUNHA_DATABASE_URL.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE="$ROOT/scripts/mastodon-differential-compose.yml"
OWN_EUNHA=""
[ $# -eq 0 ] && OWN_EUNHA=1
if [ -z "$OWN_EUNHA" ]; then
  EUNHA_URL="${1:?usage: differential_test.sh [<eunha-url> <eunha-token>]}"
  EUNHA_TOKEN="${2:?usage: differential_test.sh [<eunha-url> <eunha-token>]}"
fi

command -v docker >/dev/null || { echo "!! no docker on PATH" >&2; exit 1; }

# ── An eunha of our own, when none was given ──────────────────────────────────
#
# Torn down at the end; the Mastodon side is left up, because bringing it back
# costs a minute and rerunning against it costs seconds.
EUNHA_PORT="${EUNHA_PORT:-3001}"
EUNHA_DB="${EUNHA_DB:-eunha_differential}"
EUNHA_S3_PORT="${EUNHA_S3_PORT:-9999}"
WORK="${DIFFERENTIAL_WORK_DIR:-/tmp/eunha-differential}"

cleanup() {
  [ -n "$OWN_EUNHA" ] || return 0
  if [ -n "${EUNHA_PID:-}" ]; then
    kill "$EUNHA_PID" 2>/dev/null || true
    for _ in $(seq 20); do
      kill -0 "$EUNHA_PID" 2>/dev/null || break
      sleep 0.2
    done
    kill -9 "$EUNHA_PID" 2>/dev/null || true
    wait "$EUNHA_PID" 2>/dev/null || true
  fi
  if [ -n "${S3_PID:-}" ]; then
    kill "$S3_PID" 2>/dev/null || true
    wait "$S3_PID" 2>/dev/null || true
  fi
  # The database is left behind on purpose — it is the evidence when a run
  # fails, and the next run drops it before doing anything else.
  return 0
}
trap cleanup EXIT

start_own_eunha() {
  command -v psql >/dev/null || { echo "!! no psql on PATH" >&2; exit 1; }
  [ -x "$ROOT/target/release/eunha" ] || {
    echo "!! build eunha first: cargo build --release --bin eunha" >&2; exit 1; }

  rm -rf "$WORK"; mkdir -p "$WORK"

  # `PGDATABASE` and friends carry the connection; a URL would have to be
  # reassembled for `createdb`, `psql` and eunha separately and they would drift.
  echo "==> Preparing $EUNHA_DB"
  dropdb --if-exists "$EUNHA_DB"
  createdb "$EUNHA_DB"
  local url
  url="${EUNHA_DATABASE_URL:-postgres:///$EUNHA_DB}"

  # Migration 001 creates the `eunha` schema, and the search path eunha connects
  # with puts sqlx's own ledger in it — so `public` stays a pure mirror of
  # Mastodon's schema, which is what the schema check depends on.
  #
  # From `$WORK`, because a checkout usually has a `config.toml` and a `.env`
  # naming somebody's real development database, and this is the one command
  # here that would write to whichever one it found.
  ( cd "$WORK" && DATABASE_URL="$url" "$ROOT/target/release/eunha" migrate )

  # Two accounts, because the interaction verbs need someone to follow and
  # block, and a token to act as the first of them. Three more, because a
  # notification group of one is a group on any server — telling two groupings
  # apart takes several accounts doing the same thing. No signing keys: nothing
  # here federates.
  psql -q -d "$EUNHA_DB" -v ON_ERROR_STOP=1 >/dev/null <<'SQL'
INSERT INTO accounts (id, username, domain, display_name, note, created_at, updated_at)
VALUES (1, 'differ', NULL, 'Differ', '', now(), now()),
       (2, 'other',  NULL, 'Other',  '', now(), now()),
       (3, 'fan1',   NULL, 'Fan One',   '', now(), now()),
       (4, 'fan2',   NULL, 'Fan Two',   '', now(), now()),
       (5, 'fan3',   NULL, 'Fan Three', '', now(), now()),
       (6, 'warden', NULL, 'Warden',    '', now(), now()),
       (7, 'troll',  NULL, 'Troll',     '', now(), now()),
       (8, 'mover',  NULL, 'Mover',     '', now(), now()),
       (9, 'moved_to', NULL, 'Moved To', '', now(), now());
-- Remote accounts nothing fetches: they exist so that a domain block has
-- someone to act on. `.example` is reserved, so whatever either server tries
-- to deliver to them goes nowhere.
INSERT INTO accounts (id, username, domain, display_name, note, uri, url, inbox_url,
                      shared_inbox_url, followers_url, protocol, actor_type, created_at, updated_at)
VALUES (10, 'faraway', 'blocked.example', '', '', 'https://blocked.example/users/faraway',
        'https://blocked.example/@faraway', 'https://blocked.example/users/faraway/inbox',
        'https://blocked.example/inbox', 'https://blocked.example/users/faraway/followers',
        1, 'Person', now(), now()),
       (11, 'distant', 'silenced.example', '', '', 'https://silenced.example/users/distant',
        'https://silenced.example/@distant', 'https://silenced.example/users/distant/inbox',
        'https://silenced.example/inbox', 'https://silenced.example/users/distant/followers',
        1, 'Person', now(), now());
UPDATE accounts SET moved_to_account_id = 9 WHERE id = 8;
INSERT INTO users (id, email, account_id, created_at, updated_at, confirmed_at, approved, encrypted_password, role_id)
VALUES (1, 'differ@localhost', 1, now(), now(), now(), true, 'x', NULL),
       (2, 'other@localhost',  2, now(), now(), now(), true, 'x', NULL),
       (3, 'fan1@localhost',   3, now(), now(), now(), true, 'x', NULL),
       (4, 'fan2@localhost',   4, now(), now(), now(), true, 'x', NULL),
       (5, 'fan3@localhost',   5, now(), now(), now(), true, 'x', NULL),
       (6, 'warden@localhost', 6, now(), now(), now(), true, 'x',
        (SELECT id FROM user_roles WHERE name = 'Owner')),
       (7, 'troll@localhost',  7, now(), now(), now(), true, 'x', NULL),
       (8, 'mover@localhost',  8, now(), now(), now(), true, 'x', NULL),
       (9, 'moved_to@localhost', 9, now(), now(), now(), true, 'x', NULL);
INSERT INTO oauth_applications (id, name, uid, secret, redirect_uri, scopes, created_at, updated_at)
VALUES (1, 'differential', 'u', 's', 'urn:ietf:wg:oauth:2.0:oob', 'read write follow push', now(), now()),
       (2, 'moderation', 'm', 's', 'urn:ietf:wg:oauth:2.0:oob',
        'read write follow push admin:read admin:write', now(), now());
INSERT INTO oauth_access_tokens (id, token, resource_owner_id, application_id, scopes, created_at)
VALUES (1, 'eunha-differential-token', 1, 1, 'read write follow push', now()),
       (2, 'eunha-other-token', 2, 1, 'read write follow push', now()),
       (3, 'eunha-fan1-token', 3, 1, 'read write follow push', now()),
       (4, 'eunha-fan2-token', 4, 1, 'read write follow push', now()),
       (5, 'eunha-fan3-token', 5, 1, 'read write follow push', now()),
       (6, 'eunha-mod-token', 6, 2, 'read write follow push admin:read admin:write', now()),
       (7, 'eunha-troll-token', 7, 1, 'read write follow push', now()),
       (8, 'eunha-mover-token', 8, 1, 'read write follow push', now()),
       (9, 'eunha-moved-to-token', 9, 1, 'read write follow push', now()),
       -- One per flow, as `differential_seed.rb` mints them, so that no token
       -- runs into `throttle_per_token_api`.
       (10, 'eunha-differ-flow-1', 1, 1, 'read write follow push', now()),
       (11, 'eunha-differ-flow-2', 1, 1, 'read write follow push', now()),
       (12, 'eunha-differ-flow-3', 1, 1, 'read write follow push', now()),
       (13, 'eunha-differ-flow-4', 1, 1, 'read write follow push', now()),
       (14, 'eunha-differ-flow-5', 1, 1, 'read write follow push', now()),
       (15, 'eunha-differ-flow-6', 1, 1, 'read write follow push', now());
-- What the two domain blocks remove: a remote follower of `differ`, with the
-- notification its follow produced, and a remote account `differ` follows.
-- No ids given: an explicit one leaves the sequence behind, and the first
-- follow made through the API then collides with it.
INSERT INTO follows (account_id, target_account_id, created_at, updated_at)
VALUES (1, 11, now(), now());
WITH follow AS (
  INSERT INTO follows (account_id, target_account_id, created_at, updated_at)
  VALUES (10, 1, now(), now()) RETURNING id
)
INSERT INTO notifications (activity_id, activity_type, account_id, from_account_id, type, created_at, updated_at)
SELECT id, 'Follow', 1, 10, 'follow', now(), now() FROM follow;
-- Ids 1 and 2, which the fixture below names, from a fresh sequence.
INSERT INTO rules (priority, text, created_at, updated_at)
VALUES (0, 'No spam', now(), now()),
       (1, 'Be kind', now(), now());
-- And past the ids given explicitly above, or the first user, app or token
-- made through the API collides with one of them — registering an app
-- answered 500 until this was here. Accounts take `timestamp_id`, no sequence.
SELECT setval('users_id_seq', (SELECT max(id) FROM users));
SELECT setval('oauth_applications_id_seq', (SELECT max(id) FROM oauth_applications));
SELECT setval('oauth_access_tokens_id_seq', (SELECT max(id) FROM oauth_access_tokens));
SQL

  local vapid vapid_priv vapid_pub
  vapid=$(openssl ecparam -genkey -name prime256v1 -noout 2>/dev/null)
  vapid_priv=$(printf '%s' "$vapid" | openssl pkcs8 -topk8 -nocrypt 2>/dev/null)
  # `tr -d '=\n'`, not just `=`: GNU base64 wraps at 76 columns, so on Linux
  # the key arrives split over two lines and the TOML below fails to parse.
  # macOS base64 does not wrap, which is why this only ever broke in CI.
  vapid_pub=$(printf '%s' "$vapid" | openssl ec -pubout -outform DER 2>/dev/null \
    | tail -c 65 | base64 | tr '+/' '-_' | tr -d '=\n')

  cat > "$WORK/config.toml" <<EOF
database_url = "$url"
redis_url = "${EUNHA_REDIS_URL:-redis://127.0.0.1:6379/14}"
bind_address = "127.0.0.1:$EUNHA_PORT"

[instance]
domain = "localhost:$EUNHA_PORT"
title = "eunha"
description = ""
short_description = ""
contact_email = "differ@localhost"
registrations_open = false
approval_required = false
vapid_private_key = """
$vapid_priv
"""
vapid_public_key = "$vapid_pub"
privacy_policy = ""
terms_of_service = ""

[media_storage]
bucket = "f"
region = "auto"
endpoint = "http://127.0.0.1:$EUNHA_S3_PORT"
access_key_id = "f"
secret_access_key = "f"
base_url = "http://127.0.0.1:$EUNHA_S3_PORT"

EOF

  # Somewhere for an upload to go, so the media endpoints answer as they
  # would and not with the error of a bucket nobody is listening for.
  python3 "$ROOT/scripts/differential_fake_s3.py" "$EUNHA_S3_PORT" &
  S3_PID=$!

  # The database is new, the Redis counts are not: the rate limits a previous
  # run spent — posting, following, registering apps, a token's five minutes —
  # are kept by account and token id, which every run reuses. Mastodon's seed
  # clears its own. CI's Redis is new every run, and a machine without
  # `redis-cli` only meets this after several runs close together.
  if command -v redis-cli >/dev/null; then
    local redis_url="${EUNHA_REDIS_URL:-redis://127.0.0.1:6379/14}"
    for pattern in 'rate_limit:*' 'cache:rack::attack:*'; do
      redis-cli -u "$redis_url" --scan --pattern "$pattern" | while read -r key; do
        redis-cli -u "$redis_url" del "$key" >/dev/null
      done
    done
  fi

  echo "==> Starting eunha on :$EUNHA_PORT"
  # `exec`, so that `$!` is eunha's own pid. Backgrounding the `cd && eunha`
  # list instead gives the pid of the shell wrapping it, and cleanup then kills
  # something that has already gone while eunha keeps the port.
  ( cd "$WORK"; exec "$ROOT/target/release/eunha" > "$WORK/eunha.log" 2>&1 ) &
  EUNHA_PID=$!
  for _ in $(seq 60); do
    curl -sf -m 3 -o /dev/null "http://127.0.0.1:$EUNHA_PORT/api/v1/instance" && break
    sleep 1
  done
  curl -sf -m 3 -o /dev/null "http://127.0.0.1:$EUNHA_PORT/api/v1/instance" || {
    echo "!! eunha did not come up:" >&2; tail -30 "$WORK/eunha.log" >&2; exit 1; }

  EUNHA_URL="http://127.0.0.1:$EUNHA_PORT"
  EUNHA_TOKEN="eunha-differential-token"
  EUNHA_OTHER_ID=2
  EUNHA_FANS="3:eunha-fan1-token,4:eunha-fan2-token,5:eunha-fan3-token"
  # What the flows need to know about this side, in the shape
  # `differential_seed.rb` reports Mastodon's.
  EUNHA_FIXTURE="$WORK/eunha-fixture.json"
  cat > "$EUNHA_FIXTURE" <<'JSON'
{"ids": {"differ": "1", "other": "2", "fan1": "3", "fan2": "4", "fan3": "5",
         "mod": "6", "troll": "7", "mover": "8", "moved_to": "9",
         "faraway": "10", "distant": "11"},
 "tokens": {"differ": "eunha-differential-token", "other": "eunha-other-token",
            "fan1": "eunha-fan1-token", "fan2": "eunha-fan2-token",
            "fan3": "eunha-fan3-token", "mod": "eunha-mod-token",
            "troll": "eunha-troll-token", "mover": "eunha-mover-token",
            "moved_to": "eunha-moved-to-token"},
 "flow_tokens": ["eunha-differ-flow-1", "eunha-differ-flow-2", "eunha-differ-flow-3",
                 "eunha-differ-flow-4", "eunha-differ-flow-5", "eunha-differ-flow-6"],
 "rules": ["1", "2"]}
JSON
}

[ -n "$OWN_EUNHA" ] && start_own_eunha

echo "==> Starting Mastodon $(grep -o 'mastodon:v[0-9.]*' "$COMPOSE" | head -1)"
docker compose -f "$COMPOSE" up -d

echo "==> Waiting for it to serve"
# Production mode forces SSL, so it answers 301 without this header — which is
# what a reverse proxy in front of it would send anyway.
until curl -sf -m 3 -o /dev/null -H "X-Forwarded-Proto: https" \
        http://localhost:3000/api/v1/instance; do
  sleep 5
done

# A worker, or nothing that Mastodon defers ever happens — and some of that is
# visible in the API. `unfavourite` and `unreblog` hand the removal to
# `UnfavouriteWorker` and `RemovalWorker` and force the flag false in their own
# response, so the undo itself looks right while the row survives; every later
# request then reads it and reports `favourited: true` on a status that was
# unfavourited. That is what nine of these findings were, blamed on eunha for
# weeks. Sidekiq registers itself in Redis, so ask Redis rather than trusting
# that a container was started.
echo "==> Waiting for a worker"
# Anything other than a number — Redis not up yet, the exec failing — counts as
# no worker. Left as an empty string it would go to `[ "" -gt 0 ]`, which is an
# error rather than a false, and the gate would wave through exactly the case it
# exists to catch.
workers() {
  local n
  n=$(docker compose -f "$COMPOSE" exec -T redis redis-cli scard processes 2>/dev/null | tr -d '\r')
  case "$n" in
    "" | *[!0-9]*) echo 0 ;;
    *) echo "$n" ;;
  esac
}
for _ in $(seq 60); do
  [ "$(workers)" -gt 0 ] && break
  sleep 2
done
if [ "$(workers)" -lt 1 ]; then
  echo "!! no Sidekiq worker registered; the comparison would blame eunha for" >&2
  echo "!! Mastodon's deferred work never running. Logs:" >&2
  docker compose -f "$COMPOSE" logs --tail=20 sidekiq >&2
  exit 1
fi

echo "==> Seeding"
# Accounts, tokens and the state each flow starts from, reset every run: this
# container outlives the run and eunha's database does not, so whatever a flow
# leaves here would otherwise be compared against a fresh eunha next time.
# The script prints one `FIXTURE {json}` line naming what it made.
SEED=$(docker compose -f "$COMPOSE" exec -T web bin/rails runner \
  "$(cat "$ROOT/scripts/differential_seed.rb")" | tr -d '\r')
MASTODON_FIXTURE_JSON=$(printf '%s\n' "$SEED" | sed -n 's/^FIXTURE //p' | tail -1)
[ -n "$MASTODON_FIXTURE_JSON" ] || {
  echo "!! Mastodon's seed reported no fixture; it said:" >&2
  printf '%s\n' "$SEED" >&2
  exit 1; }
mkdir -p "$WORK"
MASTODON_FIXTURE="$WORK/mastodon-fixture.json"
printf '%s' "$MASTODON_FIXTURE_JSON" > "$MASTODON_FIXTURE"
# `fixture tokens differ` is the fixture's `tokens.differ`.
fixture() {
  python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))[sys.argv[2]][sys.argv[3]])' \
    "$MASTODON_FIXTURE" "$1" "$2"
}
TOKEN=$(fixture tokens differ)
OTHER_ID=$(fixture ids other)
# `account_id:token` per fan, in the order they act, so the two servers' samples
# can be compared by who they name rather than by ids that cannot match.
MASTODON_FANS=$(for n in fan1 fan2 fan3; do
  printf '%s:%s,' "$(fixture ids "$n")" "$(fixture tokens "$n")"
done | sed 's/,$//')

# A token that came back empty would compare a 401 against a 401 on every read
# and call it agreement, so say so here rather than a hundred lines later.
for name in TOKEN OTHER_ID MASTODON_FANS; do
  [ -n "${!name}" ] || {
    echo "!! Mastodon minted no $name; the seed said:" >&2
    printf '%s\n' "$SEED" >&2
    exit 1; }
done

echo "==> Comparing"
# `--opt=value`, not `--opt value`: Doorkeeper mints tokens with
# `SecureRandom.urlsafe_base64`, whose alphabet includes `-`, so about one run
# in sixty-four drew a token beginning with one and argparse read it as an
# option name — "expected one argument", on a token that was perfectly good.
# Everything after the `=` is the value however it starts.
#
# DIFFERENTIAL_ARGS passes more, such as `--verbose` to see each flow step's
# status codes, or `--flow=reports` to run one flow alone.
# shellcheck disable=SC2086
python3 "$ROOT/scripts/differential_test.py" \
  --eunha="$EUNHA_URL" --mastodon=http://localhost:3000 \
  --eunha-token="$EUNHA_TOKEN" --mastodon-token="$TOKEN" \
  --eunha-other-id="${EUNHA_OTHER_ID:-}" --mastodon-other-id="$OTHER_ID" \
  --eunha-fans="${EUNHA_FANS:-}" --mastodon-fans="$MASTODON_FANS" \
  --eunha-fixture="${EUNHA_FIXTURE:-}" --mastodon-fixture="$MASTODON_FIXTURE" \
  ${DIFFERENTIAL_ARGS:-}
