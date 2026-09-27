#!/usr/bin/env bash
# A local post goes viral: thousands of remote actors, none of whom the
# instance has seen before, like, boost and reply to it within minutes.
#
# Every remote server is simulated by eunha-fedisim, which is also the only
# proxy eunha is given: nothing it does reaches a real server. See
# docs/design/benchmarking.md, “Spikes”.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)

RESULTS=${EUNHA_SPIKE_RESULTS:-$ROOT/benchmark-results/spike-$(date +%Y%m%d-%H%M%S)}
# An existing database to spike — a disposable clone of a real instance. Left
# unset, a fresh one is created with the benchmark's ten seeded users.
DATABASE=${EUNHA_SPIKE_DATABASE:-}
DOMAIN=${EUNHA_SPIKE_DOMAIN:-spike.test}
ACCOUNT=${EUNHA_SPIKE_ACCOUNT:-user1}
# Appended to the generated config.toml; a clone needs its instance's
# [active_record_encryption] table to read its own signing keys.
CONFIG_APPEND=${EUNHA_SPIKE_CONFIG_APPEND:-}
POOL=${EUNHA_SPIKE_POOL:-5}
SERVERS=${EUNHA_SPIKE_SERVERS:-50}
ACTORS=${EUNHA_SPIKE_ACTORS_PER_SERVER:-200}
PEAK_RPS=${EUNHA_SPIKE_PEAK_RPS:-100}
RISE=${EUNHA_SPIKE_RISE_SECONDS:-30}
DURATION=${EUNHA_SPIKE_DURATION_SECONDS:-180}
MIX=${EUNHA_SPIKE_MIX:-'{"like": 6, "announce": 3, "reply": 1}'}
BASELINE=${EUNHA_SPIKE_BASELINE_SECONDS:-20}
# How long to wait after the last activity for the inbox queue to drain.
RECOVERY=${EUNHA_SPIKE_RECOVERY_SECONDS:-600}
# How long to leave the instance idle once drained, to see what memory it keeps.
COOLDOWN=${EUNHA_SPIKE_COOLDOWN_SECONDS:-60}
FLAME_SECONDS=${EUNHA_SPIKE_FLAME_SECONDS:-10}
EUNHA_PORT=${EUNHA_SPIKE_PORT:-18900}
SIM_PORT=${EUNHA_SPIKE_SIM_PORT:-18990}

EUNHA="$ROOT/target/release/eunha"
FEDISIM="$ROOT/target/release/eunha-fedisim"
for bin in "$EUNHA" "$FEDISIM"; do
  [ -x "$bin" ] || { echo "missing $bin: cargo build --release --bin eunha --bin eunha-fedisim" >&2; exit 1; }
done
for tool in inferno-collapse-sample inferno-flamegraph rustfilt; do
  command -v "$tool" >/dev/null || { echo "missing $tool: cargo install --locked inferno rustfilt" >&2; exit 1; }
done

# eunha and the simulator — the only two processes here that make outbound
# connections of their own accord — run sandboxed with the network denied
# except loopback and PostgreSQL's socket. That nothing leaves the machine is
# then enforced by the kernel, not only by the proxy settings; DNS is denied
# too, since nothing here needs a name resolved. The driver itself addresses
# only 127.0.0.1 and local sockets.
OFFLINE=${EUNHA_SPIKE_OFFLINE:-1}
SANDBOX='(version 1)
(allow default)
(deny network-outbound)
(allow network-outbound (remote ip "localhost:*"))
(allow network-outbound (remote unix-socket (path-regex #"^/(private/)?tmp/\.s\.PGSQL\.")))'
# Replaces the calling (sub)shell, so that the PID a caller holds is the
# process's own and killing it stops the process.
offline() {
  if [ "$OFFLINE" = 1 ]; then exec sandbox-exec -p "$SANDBOX" "$@"; else exec "$@"; fi
}
if [ "$OFFLINE" = 1 ] && (offline curl -s -m 3 -o /dev/null http://1.1.1.1 2>/dev/null); then
  echo "the sandbox let a request to 1.1.1.1 through; refusing to run" >&2
  exit 1
fi

for port in "$EUNHA_PORT" "$SIM_PORT"; do
  if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
    echo "port $port is already in use; is an earlier run still going?" >&2; exit 1
  fi
done
mkdir -p "$RESULTS"
WORK=$(mktemp -d /private/tmp/eunha-spike.XXXXXX)
PIDS=()
CREATED_DB=""
cleanup() {
  for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
  [ -n "$CREATED_DB" ] && dropdb --if-exists "$CREATED_DB" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

if [ -z "$DATABASE" ]; then
  DATABASE="eunha_spike_$$"
  CREATED_DB=$DATABASE
  createdb "$DATABASE"
  (cd "$WORK" && DATABASE_URL="postgres:///$DATABASE" "$EUNHA" migrate >/dev/null)
  psql -q -d "$DATABASE" -v ON_ERROR_STOP=1 -f "$ROOT/scripts/benchmark_seed.sql" >/dev/null
else
  # What follows deletes rows, so it only touches a database named as a copy.
  case "$DATABASE" in
    *spike* | *clone* | *rehearsal*) ;;
    *) echo "refusing $DATABASE: a spike target's name must contain spike, clone or rehearsal" >&2; exit 1 ;;
  esac
  # A clone carries its instance's push subscriptions. The proxy would refuse
  # the requests anyway; without the rows they are never attempted.
  psql -q -d "$DATABASE" -v ON_ERROR_STOP=1 -c "DELETE FROM web_push_subscriptions" >/dev/null
fi
DB_URL="postgres:///$DATABASE"
q() { psql -d "$DATABASE" -Atq -v ON_ERROR_STOP=1 -c "$1"; }

account_id=$(q "SELECT id FROM accounts WHERE username = '$ACCOUNT' AND domain IS NULL")
[ -n "$account_id" ] || { echo "no local account $ACCOUNT in $DATABASE" >&2; exit 1; }
token="eunha-spike-$$-$RANDOM"
q "WITH app AS (
     INSERT INTO oauth_applications (id, name, uid, secret, redirect_uri, scopes, created_at, updated_at)
     SELECT coalesce(max(id), 0) + 1, 'spike', 'spike-$$-$RANDOM', 'spike', 'urn:ietf:wg:oauth:2.0:oob', 'read write', now(), now()
     FROM oauth_applications
     RETURNING id)
   INSERT INTO oauth_access_tokens (id, token, resource_owner_id, application_id, scopes, created_at)
   SELECT (SELECT coalesce(max(id), 0) + 1 FROM oauth_access_tokens), '$token', u.id, app.id, 'read write', now()
   FROM users u, app WHERE u.account_id = $account_id" >/dev/null

vapid=$(openssl ecparam -genkey -name prime256v1 -noout 2>/dev/null)
vapid_priv=$(printf '%s' "$vapid" | openssl pkcs8 -topk8 -nocrypt 2>/dev/null)
vapid_pub=$(printf '%s' "$vapid" | openssl ec -pubout -outform DER 2>/dev/null | tail -c 65 | base64 | tr '+/' '-_' | tr -d '=\n')
cat > "$WORK/config.toml" <<EOF
database_url = "$DB_URL"
redis_url = "redis://127.0.0.1:6379/14"
redis_key_prefix = "spike-$$"
bind_address = "127.0.0.1:$EUNHA_PORT"
software_update_url = ""

[database_pool]
max_connections = $POOL
min_connections = 0
acquire_timeout_seconds = 5
idle_timeout_seconds = 20

[instance]
domain = "$DOMAIN"
title = "Eunha spike"
description = ""
short_description = ""
registrations_open = false
approval_required = false
vapid_private_key = """
$vapid_priv
"""
vapid_public_key = "$vapid_pub"
privacy_policy = ""
terms_of_service = ""

# Media goes to the simulator, which accepts and discards it.
[media_storage]
bucket = "spike"
region = "auto"
endpoint = "http://127.0.0.1:$SIM_PORT"
access_key_id = "spike"
secret_access_key = "spike"
base_url = "http://127.0.0.1:$SIM_PORT/spike"

[resend]
api_key = ""
from = "spike@spike.invalid"
EOF
[ -n "$CONFIG_APPEND" ] && { printf '\n'; cat "$CONFIG_APPEND"; } >> "$WORK/config.toml"
cp "$WORK/config.toml" "$RESULTS/config.toml"
sed -i '' '/vapid_private_key/,/^"""$/d' "$RESULTS/config.toml"

(offline "$FEDISIM" --listen "127.0.0.1:$SIM_PORT" --eunha "http://127.0.0.1:$EUNHA_PORT" \
  --domain "$DOMAIN" --servers "$SERVERS" --actors-per-server "$ACTORS" \
  > "$RESULTS/fedisim.log" 2>&1) &
PIDS+=($!)

# Every HTTP client eunha has honours these, and the simulator refuses every
# host it does not simulate — so a leak shows up as a count, not as traffic.
(cd "$WORK" && unset NO_PROXY no_proxy &&
  export HTTP_PROXY="http://127.0.0.1:$SIM_PORT" HTTPS_PROXY="http://127.0.0.1:$SIM_PORT" \
    ALL_PROXY="http://127.0.0.1:$SIM_PORT" RUST_LOG="${RUST_LOG:-warn}" &&
  offline "$EUNHA" > "$RESULTS/eunha.log" 2>&1) &
PIDS+=($!)

api() { curl -s -H "Host: $DOMAIN" -H "Authorization: Bearer $token" "$@"; }
for _ in $(seq 1 100); do
  curl -sf "http://127.0.0.1:$SIM_PORT/__stats?since=999999" >/dev/null 2>&1 &&
    api -f "http://127.0.0.1:$EUNHA_PORT/api/v1/accounts/verify_credentials" >/dev/null 2>&1 && break
  sleep 0.2
done
eunha_pid=$(pgrep -f "^$EUNHA\$" | head -1)
[ -n "$eunha_pid" ] || { echo "eunha did not start; see $RESULTS/eunha.log" >&2; exit 1; }
api -sf "http://127.0.0.1:$EUNHA_PORT/api/v1/accounts/verify_credentials" >/dev/null ||
  { echo "eunha is not answering as $ACCOUNT; see $RESULTS/eunha.log" >&2; exit 1; }

status=$(api -X POST -H 'Content-Type: application/json' \
  -d '{"status": "This one is going to travel. https://example.com/", "visibility": "public"}' \
  "http://127.0.0.1:$EUNHA_PORT/api/v1/statuses")
status_id=$(node -e 'console.log(JSON.parse(process.argv[1]).id)' "$status")
# The link is a canary: eunha fetches a preview for it, which has to be refused
# and counted, or the egress count cannot be trusted to mean anything.
status_uri=$(node -e 'console.log(JSON.parse(process.argv[1]).uri)' "$status")
author_uri="https://$DOMAIN/users/$ACCOUNT"
echo "spiking $status_uri"

pgss=0
if q "SELECT pg_stat_statements_reset()" >/dev/null 2>&1; then pgss=1; fi

# Sizes as vmmap and top print them (512K, 1.5M, 2G, or plain bytes) in MiB.
MIB_FN='function mib(s,  u, v) {
  sub(/[+-]$/, "", s); u = substr(s, length(s)); v = substr(s, 1, length(s) - 1) + 0
  if (u == "K") return sprintf("%.1f", v / 1024); if (u == "M") return sprintf("%.1f", v)
  if (u == "G") return sprintf("%.1f", v * 1024); return sprintf("%.1f", (s + 0) / 1048576)
}'

# Where memory stands at a moment: eunha's footprint, and of its malloc zones
# how much is live and how much is dirty but free — fragmentation the
# allocator cannot hand back; PostgreSQL's backends for this database, costed
# privately (their footprint less the shared buffer pool each one maps); Redis;
# and the database on disk. vmmap stops the process while it reads it, so this
# is taken between phases rather than during them.
echo "moment,eunha_footprint_mib,malloc_dirty_mib,malloc_live_mib,malloc_free_mib,pg_backends,pg_private_mib,redis_used_mib,database_mib" > "$RESULTS/memory.csv"
# vmmap suspends the process it reads, which shows up as a latency spike in
# whatever probe it overlaps. A counter, odd while a snapshot is running, lets
# the sampler mark those rows so the summary can leave them out.
echo 0 > "$WORK/snap"
memory_snapshot() {
  local moment=$1 eunha pg
  echo $(( $(cat "$WORK/snap") + 1 )) > "$WORK/snap"
  eunha=$(vmmap --summary "$eunha_pid" 2>/dev/null | awk "$MIB_FN"'
    /^Physical footprint:/ { fp = mib($3) }
    /^MALLOC ZONE/ { zones = 1 }
    zones && /^TOTAL/ { dirty = mib($4); live = mib($7); free = mib($8) }
    END { printf "%s,%s,%s,%s", fp, dirty, live, free }')
  pg=$(for pid in $(q "SELECT pid FROM pg_stat_activity WHERE datname = current_database() AND pid <> pg_backend_pid()"); do
      vmmap --summary "$pid" 2>/dev/null | awk "$MIB_FN"'
        /^Physical footprint:/ { fp = mib($3) }
        /^Untagged / && !seen { shared = mib($3); seen = 1 }
        END { if (fp != "") printf "%.2f\n", fp - shared }' || true
    done | awk '{ t += $1; n++ } END { printf "%d,%.1f", n, t }')
  printf '%s,%s,%s,%s,%s\n' "$moment" "${eunha:-,,,}" "$pg" \
    "$(redis-cli -n 14 info memory 2>/dev/null | awk -F: '/^used_memory:/ {printf "%.1f", $2 / 1048576}')" \
    "$(q "SELECT round(pg_database_size(current_database()) / 1048576.0, 1)")" >> "$RESULTS/memory.csv"
  echo $(( $(cat "$WORK/snap") + 1 )) > "$WORK/snap"
}

echo baseline > "$WORK/phase"
# Samples once a second, and keeps going whatever one probe does: a gap in the
# record is worse than a bad value in it.
sample_loop() {
  set +e +o pipefail
  echo "t,phase,inbox_pending,inbox_failed,delivery_pending,db_active,db_idle,db_lock_waits,eunha_cpu,eunha_rss_mib,home_ms,context_ms,notifications_ms,home_status,eunha_footprint_mib,redis_used_mib"
  local start=$SECONDS row ps_row home ctx notif fp redis_mib snap phase
  while kill -0 "$eunha_pid" 2>/dev/null; do
    snap=$(cat "$WORK/snap")
    row=$(q "SELECT (SELECT count(*) FROM eunha.inbox_jobs WHERE failed_at IS NULL),
                    (SELECT count(*) FROM eunha.inbox_jobs WHERE failed_at IS NOT NULL),
                    (SELECT count(*) FROM eunha.activity_delivery_jobs),
                    count(*) FILTER (WHERE state = 'active' AND pid <> pg_backend_pid()),
                    count(*) FILTER (WHERE state = 'idle'),
                    count(*) FILTER (WHERE wait_event_type = 'Lock')
             FROM pg_stat_activity WHERE datname = current_database()" 2>/dev/null | tr '|' ',')
    ps_row=$(ps -o %cpu=,rss= -p "$eunha_pid" 2>/dev/null | awk '{printf "%s,%.1f", $1, $2/1024}')
    home=$(api -m 30 -o /dev/null -w '%{time_total},%{http_code}' "http://127.0.0.1:$EUNHA_PORT/api/v1/timelines/home?limit=20")
    ctx=$(api -m 30 -o /dev/null -w '%{time_total}' "http://127.0.0.1:$EUNHA_PORT/api/v1/statuses/$status_id/context")
    notif=$(api -m 30 -o /dev/null -w '%{time_total}' "http://127.0.0.1:$EUNHA_PORT/api/v1/notifications?limit=20")
    # phys_footprint, not RSS: RSS counts the binary's shared text pages.
    fp=$(top -l 1 -pid "$eunha_pid" -stats mem 2>/dev/null | awk 'END { print mib($1) } '"$MIB_FN")
    redis_mib=$(redis-cli -n 14 info memory 2>/dev/null | awk -F: '/^used_memory:/ {printf "%.1f", $2 / 1048576}')
    phase=$(cat "$WORK/phase")
    # Overlapped a memory snapshot: kept, but marked and left out of the summary.
    if [ "$snap" != "$(cat "$WORK/snap")" ] || [ $((snap % 2)) = 1 ]; then phase="$phase (paused)"; fi
    printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' $((SECONDS - start)) "$phase" "${row:-,,,,,}" "${ps_row:-,}" \
      "$(awk -v s="${home%,*}" 'BEGIN{printf "%.1f", s*1000}')" \
      "$(awk -v s="$ctx" 'BEGIN{printf "%.1f", s*1000}')" \
      "$(awk -v s="$notif" 'BEGIN{printf "%.1f", s*1000}')" "${home#*,}" "$fp" "$redis_mib"
    # Every internet socket eunha holds, and whether its peer is loopback. An
    # audit that did not run is recorded as such, so that a dead auditor cannot
    # pass for a clean one.
    if lsof -a -p "$eunha_pid" -i -n -P -F n > "$WORK/lsof" 2>/dev/null; then
      echo audit >> "$RESULTS/socket-audit.txt"
      sed -n 's/^n//p' "$WORK/lsof" | awk '{
        peer = $0; if (index(peer, "->")) peer = substr(peer, index(peer, "->") + 2)
        if (peer !~ /^(127\.0\.0\.1|\[::1\]|\*):/) print "leak " $0
      }' >> "$RESULTS/socket-audit.txt"
    fi
    sleep 1
  done
}
: > "$RESULTS/socket-audit.txt"
sample_loop > "$RESULTS/timeseries.csv" &
PIDS+=($!)

# `sample` sees every thread, parked or not. Stacks that end in a wait are
# dropped so the flamegraph shows where CPU went; the unfiltered stacks stay in
# the .folded file.
IDLE_LEAF='(__psynch_cvwait|__psynch_mutexwait|kevent|kevent64|__semwait_signal|__workq_kernreturn|mach_msg2_trap|__select|__ulock_wait2?|__recvfrom|__accept|poll)( |$)'
flamegraph() {
  local name=$1
  sample "$eunha_pid" "$FLAME_SECONDS" -mayDie -file "$RESULTS/flame-$name.sample.txt" >/dev/null 2>&1 || return 0
  inferno-collapse-sample "$RESULTS/flame-$name.sample.txt" 2>/dev/null |
    sed -E 's/^Thread_[0-9]+(: [^;]*)?;//' | rustfilt > "$RESULTS/flame-$name.folded"
  awk -v idle="$IDLE_LEAF" '{ stack = $0; sub(/ [0-9]+$/, "", stack); n = split(stack, f, ";"); leaf = f[n]; sub(/^[^`]*`/, "", leaf); if (leaf !~ "^" idle) print }' \
    "$RESULTS/flame-$name.folded" |
    inferno-flamegraph --title "eunha on CPU: $name ($FLAME_SECONDS s)" > "$RESULTS/flame-$name.svg" 2>/dev/null ||
    rm -f "$RESULTS/flame-$name.svg"
}

set_phase() { echo "$1" > "$WORK/phase"; echo "[$(date +%T)] $1"; }

set_phase baseline
flamegraph baseline & flame_pid=$!
sleep "$BASELINE"
wait "$flame_pid" || true
memory_snapshot before

set_phase spike
sim_offset=$(curl -s "http://127.0.0.1:$SIM_PORT/__stats?since=999999" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).now_second))')
curl -sf -X POST "http://127.0.0.1:$SIM_PORT/__spikes" -H 'Content-Type: application/json' -d "{
  \"status_uri\": \"$status_uri\", \"author_uri\": \"$author_uri\",
  \"peak_rps\": $PEAK_RPS, \"rise_seconds\": $RISE, \"duration_seconds\": $DURATION,
  \"mix\": $MIX }"
# The curve peaks at RISE; sample around it.
sleep $(( RISE > FLAME_SECONDS / 2 ? RISE - FLAME_SECONDS / 2 : 0 ))
flamegraph peak
while curl -s "http://127.0.0.1:$SIM_PORT/__stats?since=999999" | grep -q '"running":true'; do sleep 1; done
memory_snapshot "spike ended"

set_phase recovery
recovery_start=$SECONDS
drained=""
flamegraph recovery & flame_pid=$!
while [ $((SECONDS - recovery_start)) -lt "$RECOVERY" ]; do
  pending=$(q "SELECT count(*) FROM eunha.inbox_jobs WHERE failed_at IS NULL")
  if [ "$pending" = 0 ]; then drained=$((SECONDS - recovery_start)); break; fi
  sleep 1
done
wait "$flame_pid" || true
memory_snapshot drained
set_phase cooldown
sleep "$COOLDOWN"
memory_snapshot "idle ${COOLDOWN} s later"
set_phase done

curl -s "http://127.0.0.1:$SIM_PORT/__stats?since=$sim_offset" > "$RESULTS/fedisim.json"
if [ "$pgss" = 1 ]; then
  q "SELECT calls, round(total_exec_time)::bigint AS total_ms, round(mean_exec_time::numeric, 2) AS mean_ms,
            regexp_replace(left(query, 160), '\s+', ' ', 'g')
     FROM pg_stat_statements WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
     ORDER BY total_exec_time DESC LIMIT 20" > "$RESULTS/top-queries.txt"
fi
q "SELECT
     (SELECT count(*) FROM favourites WHERE status_id = $status_id),
     (SELECT count(*) FROM statuses WHERE reblog_of_id = $status_id),
     (SELECT count(*) FROM statuses WHERE in_reply_to_id = $status_id),
     (SELECT count(*) FROM accounts WHERE domain LIKE '%.fedisim.test'),
     (SELECT count(*) FROM notifications WHERE account_id = $account_id),
     (SELECT count(*) FROM eunha.inbox_jobs WHERE failed_at IS NOT NULL)" > "$WORK/outcome"
IFS='|' read -r favs boosts replies remote_accounts notifications failed < "$WORK/outcome"

node - "$RESULTS" "$drained" "$RECOVERY" "$favs" "$boosts" "$replies" "$remote_accounts" "$notifications" "$failed" <<'EOF' | tee "$RESULTS/summary.md"
const fs = require("fs");
const [dir, drained, recovery, favs, boosts, replies, accounts, notifications, failed] = process.argv.slice(2);
const sim = JSON.parse(fs.readFileSync(`${dir}/fedisim.json`));
const rows = fs.readFileSync(`${dir}/timeseries.csv`, "utf8").trim().split("\n");
const head = rows.shift().split(",");
const data = rows.map(r => Object.fromEntries(r.split(",").map((v, i) => [head[i], v])));
const pct = (xs, p) => { const s = xs.filter(Number.isFinite).sort((a, b) => a - b); return s.length ? s[Math.round(p / 100 * (s.length - 1))] : 0; };
const by = ph => data.filter(r => r.phase === ph);
const col = (rs, c) => rs.map(r => Number(r[c]));
const t = sim.totals;
const peakSecond = sim.timeline.reduce((a, b) => (b.sent > a.sent ? b : a), { sent: 0 });
console.log(`# Viral post spike\n`);
console.log(`Offered ${t.sent + t.shed} activities, peak ${peakSecond.sent}/s. Accepted ${t.ok}, refused ${t.client_error}, failed ${t.server_error + t.transport_error}, shed ${t.shed} (client concurrency cap).`);
console.log(`Inbox POST latency: p50 ${t.p50_ms} ms, p95 ${t.p95_ms} ms, p99 ${t.p99_ms} ms.`);
const blockedHosts = sim.blocked_hosts;
const others = Object.keys(blockedHosts).filter(h => h !== "example.com");
console.log(`eunha fetched ${t.served_actor} actor documents and ${t.served_note} notes.`);
console.log(`Egress refused by the proxy: ${JSON.stringify(blockedHosts)}. Canary (example.com): ${blockedHosts["example.com"] ? "caught" : "**not seen**"}; ${others.length ? "**other hosts: " + others.join(", ") + "**" : "no other host was attempted"}.`);
const audit = fs.readFileSync(`${dir}/socket-audit.txt`, "utf8").trim().split("\n").filter(Boolean);
const audits = audit.filter(l => l === "audit").length;
const leaks = [...new Set(audit.filter(l => l.startsWith("leak ")).map(l => l.slice(5)))];
console.log(`Socket audits: ${audits}, non-loopback sockets: ${audits ? (leaks.length ? "**" + leaks.join(", ") + "**" : "none") : "**UNKNOWN, the audit never ran**"}. eunha and the simulator ran ${process.env.EUNHA_SPIKE_OFFLINE === "0" ? "**without** the network sandbox" : "sandboxed to loopback"}.`);
console.log(`Queue drained ${drained ? drained + " s after the last activity" : "NOT within " + recovery + " s"}; ${failed} jobs failed for good.`);
console.log(`Landed: ${favs} favourites, ${boosts} boosts, ${replies} replies, ${accounts} new remote accounts, ${notifications} notifications for the author.\n`);
console.log(`| Phase | Queue max | eunha CPU p95 | Footprint max | Home p95 | Context p95 | Notifications p95 | DB active max | Lock waits max |`);
console.log(`| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |`);
for (const ph of ["baseline", "spike", "recovery", "cooldown"]) {
  const rs = by(ph);
  if (!rs.length) continue;
  console.log(`| ${ph} | ${Math.max(...col(rs, "inbox_pending"))} | ${pct(col(rs, "eunha_cpu"), 95)}% | ${Math.max(...col(rs, "eunha_footprint_mib"))} MiB | ${pct(col(rs, "home_ms"), 95)} ms | ${pct(col(rs, "context_ms"), 95)} ms | ${pct(col(rs, "notifications_ms"), 95)} ms | ${Math.max(...col(rs, "db_active"))} | ${Math.max(...col(rs, "db_lock_waits"))} |`);
}
const mem = fs.readFileSync(`${dir}/memory.csv`, "utf8").trim().split("\n").map(r => r.split(","));
mem.shift();
console.log(`\n## Memory\n`);
console.log(`eunha's footprint, and of its malloc zones what is live and what is dirty but free (fragmentation). PostgreSQL is this database's backends, costed privately; the shared buffer pool is not included. Redis is the whole server.\n`);
console.log(`| Moment | eunha footprint | malloc live | malloc free | PG backends | PG private | Redis | Database on disk |`);
console.log(`| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |`);
for (const [moment, fp, , live, free, backends, pgPrivate, redis, db] of mem)
  console.log(`| ${moment} | ${fp} MiB | ${live} MiB | ${free} MiB | ${backends} | ${pgPrivate} MiB | ${redis} MiB | ${db} MiB |`);
EOF
echo
echo "results: $RESULTS"
ls "$RESULTS"/*.svg 2>/dev/null || true
grep -q '^audit$' "$RESULTS/socket-audit.txt" || { echo "the socket audit never ran" >&2; exit 1; }
if grep -q '^leak ' "$RESULTS/socket-audit.txt"; then echo "eunha held a non-loopback socket" >&2; exit 1; fi
