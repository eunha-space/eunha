#!/usr/bin/env bash
# Many instances in one eunha process, run the way eunha.space runs them.
#
# The density benchmark (benchmark_density.sh) measures a process per tenant.
# A hosting service now runs one `eunha --tenants <dir>` process per runtime,
# so this measures that instead, set up as eunha.space renders it:
#
#  -  one process serving a directory of tenant files, one listener, requests
#     dispatched by `Host`, Tokio's default of one worker per core;
#  -  a database per tenant, reached through PgBouncer in transaction mode with
#     eunha.space's pooler settings, `database_url` left direct for migrations
#     and `search_path` set on the database rather than by the pool;
#  -  the hosting profile: two-connection pools, a 20-second idle timeout, a
#     300-second queue poll, a Redis key prefix per tenant;
#  -  the control plane's health sweep, one check per tenant every 300 seconds,
#     running for the whole benchmark.
#
# For each tenant count it starts a fresh process (timing until every tenant
# answers, which is what a deploy or a host restart costs), lets it settle and
# measures it idle over one whole poll period, then runs an open-loop load whose
# requests follow a Zipf law over the tenants (benchmark_shared_load.mjs), and
# finally the same load with one tenant flooded on top.
#
# PostgreSQL, PgBouncer and Redis are its own and removed on exit. Results go to
# benchmark-results/shared-*. The traps listed in benchmark_density.sh apply
# here too: memory is phys_footprint, PostgreSQL's is costed privately, CPU of
# backends that exit inside a window is lost, and the idle window must cover a
# whole poll.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
EUNHA=${EUNHA_BIN:-$ROOT/target/release/eunha}

# Everything it needs, and how to get it, before anything is started.
missing=()
for tool in initdb pg_ctl psql pgbouncer redis-server redis-cli node perl bc; do
  command -v "$tool" >/dev/null || missing+=("$tool")
done
if [ ${#missing[@]} -gt 0 ]; then
  echo "missing: ${missing[*]}" >&2
  echo "on macOS: brew install postgresql@18 pgbouncer redis node" >&2
  exit 1
fi
if [ ! -x "$EUNHA" ]; then
  echo "no eunha binary at $EUNHA: run \`mise run bench:shared\`, which builds it, or set EUNHA_BIN" >&2
  exit 1
fi
case "$(uname)" in
  Darwin) ;;
  *) echo "measures memory with macOS's top, vmmap and vm_stat; it runs on macOS only" >&2; exit 1 ;;
esac
COUNTS=${EUNHA_SHARED_COUNTS:-"10 40 100 200"}
LOAD_RPS=${EUNHA_SHARED_LOAD_RPS:-"100 400 1600"}
SKEW=${EUNHA_SHARED_SKEW:-1}
HOT_RPS=${EUNHA_SHARED_HOT_RPS:-1500}
HOT_BASE_RPS=${EUNHA_SHARED_HOT_BASE_RPS:-200}
SETTLE=${EUNHA_SHARED_SETTLE_SECONDS:-330}
WINDOW=${EUNHA_SHARED_WINDOW_SECONDS:-300}
LOAD_SECONDS=${EUNHA_SHARED_LOAD_SECONDS:-60}
COOLDOWN=${EUNHA_SHARED_COOLDOWN_SECONDS:-120}
WARM_POSTS=${EUNHA_SHARED_WARM_POSTS:-20}
POOL=${EUNHA_SHARED_POOL:-2}
POOLER=${EUNHA_SHARED_POOLER:-1}
LOG_FILTER=${EUNHA_SHARED_LOG:-warn}
PG_PORT=${EUNHA_SHARED_PG_PORT:-55433}
PG_MAX_CONNECTIONS=${EUNHA_SHARED_PG_MAX_CONNECTIONS:-300}
PG_SHARED_BUFFERS=${EUNHA_SHARED_PG_SHARED_BUFFERS:-1GB}
BOUNCER_PORT=${EUNHA_SHARED_BOUNCER_PORT:-56433}
REDIS_PORT=${EUNHA_SHARED_REDIS_PORT:-56380}
PORT=${EUNHA_SHARED_PORT:-22000}
RESULTS=${EUNHA_SHARED_RESULTS:-$ROOT/benchmark-results/shared-$(date +%Y%m%d-%H%M%S)}

WORK=$(mktemp -d "${TMPDIR:-/tmp}/eunha-shared.XXXXXX")
mkdir -p "$RESULTS" "$WORK/tenants"
EUNHA_PID=
SWEEP_PID=
PROVISIONED=0

cat > "$RESULTS/config.txt" <<EOF
binary=$EUNHA ($(cd "$ROOT" && git rev-parse --short HEAD))
host=$(sysctl -n hw.model) cores=$(sysctl -n hw.ncpu) memory_gib=$(( $(sysctl -n hw.memsize) / 1073741824 ))
load_average_at_start=$(sysctl -n vm.loadavg)
pool=$POOL pooler=$POOLER skew=$SKEW worker_threads=${TOKIO_WORKER_THREADS:-cores}
postgres max_connections=$PG_MAX_CONNECTIONS shared_buffers=$PG_SHARED_BUFFERS
settle=$SETTLE window=$WINDOW load_seconds=$LOAD_SECONDS cooldown=$COOLDOWN warm_posts=$WARM_POSTS
hot_rps=$HOT_RPS over hot_base_rps=$HOT_BASE_RPS
EOF

stop_eunha() {
  [ -n "$SWEEP_PID" ] && kill "$SWEEP_PID" 2>/dev/null || true
  [ -n "$EUNHA_PID" ] && kill "$EUNHA_PID" 2>/dev/null || true
  [ -n "$EUNHA_PID" ] && wait "$EUNHA_PID" 2>/dev/null || true
  [ -n "$SWEEP_PID" ] && wait "$SWEEP_PID" 2>/dev/null || true
  EUNHA_PID= SWEEP_PID=
}
cleanup() {
  stop_eunha
  [ -f "$WORK/eunha.log" ] && tail -300 "$WORK/eunha.log" > "$RESULTS/eunha.log.tail"
  [ -f "$WORK/pg.log" ] && tail -200 "$WORK/pg.log" > "$RESULTS/postgres.log.tail"
  [ -f "$WORK/pgbouncer.log" ] && tail -200 "$WORK/pgbouncer.log" > "$RESULTS/pgbouncer.log.tail"
  [ -f "$WORK/pgbouncer.pid" ] && kill "$(cat "$WORK/pgbouncer.pid")" 2>/dev/null || true
  pg_ctl -D "$WORK/pg" -m fast stop >/dev/null 2>&1 || true
  redis-cli -p "$REDIS_PORT" shutdown nosave >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

pgq() { psql -h 127.0.0.1 -p "$PG_PORT" -U eunha -At -F ' ' -v ON_ERROR_STOP=1 "$@"; }
bq() { psql -h 127.0.0.1 -p "$BOUNCER_PORT" -U eunha -At -F ' ' pgbouncer -c "$1" 2>/dev/null; }
now_ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'; }
free_percent() { memory_pressure | awk -F': ' '/free percentage/ {print $2+0}'; }
host_of() { echo "t$1.bench.test"; }
hosts_upto() { local i; for i in $(seq 1 "$1"); do host_of "$i"; done | paste -sd, -; }

# ── Servers ────────────────────────────────────────────────────────────────

initdb -D "$WORK/pg" -U eunha -A trust -E UTF8 --locale=C >/dev/null
cat >> "$WORK/pg/postgresql.conf" <<EOF
port = $PG_PORT
listen_addresses = '127.0.0.1'
unix_socket_directories = ''
max_connections = $PG_MAX_CONNECTIONS
shared_buffers = $PG_SHARED_BUFFERS
max_files_per_process = 200
EOF
LC_ALL=C pg_ctl -D "$WORK/pg" -l "$WORK/pg.log" -w start >/dev/null
redis-server --port "$REDIS_PORT" --bind 127.0.0.1 --save '' --appendonly no \
  --daemonize yes --dir "$WORK" --logfile "$WORK/redis.log" >/dev/null

# eunha.space's pooler (infra's pgbouncer.ini), with trust in place of SCRAM
# and auth_query. Every tenant here shares one role where eunha.space gives
# each its own, which leaves PgBouncer's pools — keyed by user and database —
# one per tenant all the same.
printf '"eunha" ""\n"tenant" ""\n' > "$WORK/userlist.txt"
cat > "$WORK/pgbouncer.ini" <<EOF
[databases]
* = host=127.0.0.1 port=$PG_PORT
[pgbouncer]
listen_addr = 127.0.0.1
listen_port = $BOUNCER_PORT
unix_socket_dir =
pool_mode = transaction
max_prepared_statements = 200
auth_type = trust
auth_file = $WORK/userlist.txt
max_client_conn = 10000
default_pool_size = 3
min_pool_size = 0
max_db_connections = 5
server_idle_timeout = 30
server_lifetime = 3600
logfile = $WORK/pgbouncer.log
pidfile = $WORK/pgbouncer.pid
admin_users = eunha
stats_users = eunha
ignore_startup_parameters = extra_float_digits,options,search_path
EOF
if [ "$POOLER" = 1 ]; then
  (ulimit -n 20000; pgbouncer -d "$WORK/pgbouncer.ini")
fi

# Tenants connect as a role of their own, not a superuser, as eunha.space
# provisions them. That also leaves PostgreSQL's reserved superuser slots to
# this script, so that it can still look when the tenants have taken every
# other connection — which is one of the things it is here to find.
pgq -d postgres -c "CREATE ROLE tenant LOGIN" >/dev/null
pgq -d postgres -c "CREATE DATABASE eunha_template OWNER tenant" >/dev/null
(cd "$WORK" && DATABASE_URL="postgres://tenant@127.0.0.1:$PG_PORT/eunha_template" "$EUNHA" migrate >/dev/null)
pgq -q -d eunha_template -f "$ROOT/scripts/benchmark_seed.sql" >/dev/null

vapid=$(openssl ecparam -genkey -name prime256v1 -noout 2>/dev/null)
vapid_priv=$(printf '%s' "$vapid" | openssl pkcs8 -topk8 -nocrypt 2>/dev/null)
vapid_pub=$(printf '%s' "$vapid" | openssl ec -pubout -outform DER 2>/dev/null | tail -c 65 | base64 | tr '+/' '-_' | tr -d '=\n')

# ── Tenants ────────────────────────────────────────────────────────────────

# Every file names the limits of the largest step, since a directory must agree
# on them, and those limits are raised past eunha's defaults (50 tenants) and
# eunha.space's (process_database_connections = 80, which at two connections a
# tenant admits 40) so that the larger steps can run at all. Mastodon's rate
# limits are off: each tenant has ten users carrying all of its signed-in
# traffic, far past the five requests a second one user is allowed, which real
# instances spread over many more.
max_count=$(printf '%s\n' $COUNTS | sort -n | tail -1)
provision() {
  local i=$1 db="eunha_t$1" pooled=
  pgq -d postgres -c "CREATE DATABASE $db OWNER tenant TEMPLATE eunha_template" >/dev/null
  pgq -d postgres -c "ALTER DATABASE $db SET search_path TO eunha, public" >/dev/null
  if [ "$POOLER" = 1 ]; then
    pooled="pooled_database_url = \"postgres://tenant@127.0.0.1:$BOUNCER_PORT/$db\"
pooled_client_slots = 10000"
  fi
  cat > "$WORK/tenants.all/t$i.toml" <<EOF
database_url = "postgres://tenant@127.0.0.1:$PG_PORT/$db"
$pooled
redis_url = "redis://127.0.0.1:$REDIS_PORT/0"
redis_key_prefix = "t$i"
redis_process_metrics = false
bind_address = "127.0.0.1:$PORT"
software_update_url = ""

[database_pool]
max_connections = $POOL
min_connections = 0
acquire_timeout_seconds = 5
idle_timeout_seconds = 20

[workers]
queue_idle_poll_seconds = 300

[limits]
rate_limits = false
process_max_tenants = $max_count
process_database_connections = $((max_count * POOL))

[instance]
domain = "$(host_of "$i")"
vapid_private_key = """
$vapid_priv
"""
vapid_public_key = "$vapid_pub"

[media_storage]
bucket = "benchmark"
region = "auto"
endpoint = "http://127.0.0.1:9"
access_key_id = "benchmark"
secret_access_key = "benchmark"
base_url = "http://127.0.0.1:9"
key_prefix = "tenants/t$i"

[active_record_encryption]
primary_key = "shared-bench-primary-key-$i-not-a-secret"
key_derivation_salt = "shared-bench-salt-$i-not-a-secret"
EOF
}
mkdir -p "$WORK/tenants.all"

# ── Measurement ───────────────────────────────────────────────────────────

cpu_seconds() {
  [ $# -gt 0 ] || { echo 0; return; }
  ps -o time= -p "$(IFS=,; echo "$*")" 2>/dev/null |
    awk -F: '{ s = 0; for (i = 1; i <= NF; i++) s = s * 60 + $i; t += s } END { printf "%.2f\n", t }'
}
postgres_pids() { local pm; pm=$(head -1 "$WORK/pg/postmaster.pid"); echo "$pm"; pgrep -P "$pm" || true; }
bouncer_pid() { [ -f "$WORK/pgbouncer.pid" ] && cat "$WORK/pgbouncer.pid" || true; }

# eunha CPU s, PostgreSQL CPU s, PgBouncer CPU s, transactions, sessions opened,
# Redis commands, pages decompressed.
counters() {
  local db
  db=$(pgq -d postgres -c "SELECT coalesce(sum(xact_commit + xact_rollback), 0), coalesce(sum(sessions), 0) FROM pg_stat_database WHERE datname LIKE 'eunha\_t%'")
  echo "$(cpu_seconds "$EUNHA_PID") $(cpu_seconds $(postgres_pids)) $(cpu_seconds $(bouncer_pid)) $db" \
    "$(redis-cli -p "$REDIS_PORT" info stats | awk -F: '/^total_commands_processed/ {print $2+0}')" \
    "$(vm_stat | awk '/^Decompressions:/ {gsub(/\./, "", $2); print $2}')"
}

# phys_footprint (MiB) and threads of the eunha process.
footprint() {
  top -l 1 -pid "$EUNHA_PID" -stats pid,mem,th 2>/dev/null | awk -v pid="$EUNHA_PID" '
    $1 == pid {
      m = $2; sub(/[+-]$/, "", m)
      u = substr(m, length(m)); v = substr(m, 1, length(m) - 1) + 0
      if (u == "K") v /= 1024; else if (u == "G") v *= 1024; else if (u == "B") v /= 1048576
      printf "%.1f %d\n", v, $3 + 0
    }'
}

backends() { pgq -d postgres -c "SELECT count(*) FROM pg_stat_activity WHERE datname LIKE 'eunha\_t%'"; }
# Client connections eunha holds open to PgBouncer (or to PostgreSQL directly).
clients() {
  if [ "$POOLER" = 1 ]; then
    bq "SHOW POOLS" | awk '$1 ~ /^eunha_t/ { t += $3 + $4 + $5 + $6 } END { print t + 0 }'
  else
    backends
  fi
}

# Mean private memory (MiB) of up to five tenant backends; see density.
backend_private_mib() {
  local pid
  for pid in $(pgq -d postgres -c "SELECT pid FROM pg_stat_activity WHERE datname LIKE 'eunha\_t%' ORDER BY backend_start LIMIT 5"); do
    vmmap --summary "$pid" 2>/dev/null | awk '
      function mib(s,  u, v) {
        u = substr(s, length(s)); v = substr(s, 1, length(s) - 1) + 0
        if (u == "K") return v / 1024; if (u == "G") return v * 1024; if (u == "M") return v; return v / 1048576
      }
      /^Physical footprint:/ { fp = mib($3) }
      /^Untagged / && !seen { shared = mib($3); seen = 1 }
      END { if (fp) printf "%.2f\n", fp - shared }' || true
  done | awk '{ t += $1; n++ } END { printf "%.2f\n", n ? t / n : 0 }'
}

# Samples backends and clients every two seconds into $1 until killed.
sample_connections() {
  while :; do echo "$(backends) $(clients)" >> "$1"; sleep 2; done
}
summarize_connections() {
  awk '{ b += $1; c += $2; if ($1 > bm) bm = $1; if ($2 > cm) cm = $2 } END { printf "%.1f %d %.1f %d\n", b / NR, bm, c / NR, cm }' "$1"
}

start_eunha() {
  local count=$1
  rm -f "$WORK/tenants"/*.toml
  for i in $(seq 1 "$count"); do cp "$WORK/tenants.all/t$i.toml" "$WORK/tenants/"; done
  local since
  since=$(now_ms)
  RUST_LOG=$LOG_FILTER "$EUNHA" --tenants "$WORK/tenants" >>"$WORK/eunha.log" 2>&1 &
  EUNHA_PID=$!
  EUNHA_SHARED_PORT=$PORT EUNHA_SHARED_HOSTS=$(hosts_upto "$count") EUNHA_SHARED_SINCE_MS=$since \
    node "$ROOT/scripts/benchmark_shared_probe.mjs" wait > "$WORK/wait.json"
  kill -0 "$EUNHA_PID" 2>/dev/null || { echo "eunha exited; see its log" >&2; tail -20 "$WORK/eunha.log" >&2; exit 1; }
  if ! node -e 'process.exit(require(process.argv[1]).failed ? 1 : 0)' "$WORK/wait.json"; then
    echo "tenants did not start: $(cat "$WORK/wait.json")" >&2; grep ERROR "$WORK/eunha.log" | tail -5 >&2; exit 1
  fi
  EUNHA_SHARED_PORT=$PORT EUNHA_SHARED_HOSTS=$(hosts_upto "$count") \
    node "$ROOT/scripts/benchmark_shared_probe.mjs" sweep &
  SWEEP_PID=$!
}

run_load() { # count rps hot_rps output
  EUNHA_SHARED_PORT=$PORT EUNHA_SHARED_HOSTS=$(hosts_upto "$1") EUNHA_SHARED_RPS=$2 EUNHA_SHARED_HOT_RPS=$3 \
    EUNHA_SHARED_SECONDS=$LOAD_SECONDS EUNHA_SHARED_SKEW=$SKEW node "$ROOT/scripts/benchmark_shared_load.mjs" > "$4"
}

printf 'tenants,startup_all_ms,startup_p50_ms,startup_failed,footprint_mib,footprint_mib_per_tenant,threads,eunha_cpu_pct,postgres_cpu_pct,pgbouncer_cpu_pct,postgres_tps,postgres_sessions_per_min,redis_ops_per_s,backends_mean,backends_max,clients_mean,clients_max,postgres_private_mib_per_backend,database_mib_per_tenant,redis_used_mib,free_percent,decompressions_per_s\n' > "$RESULTS/idle.csv"
printf 'tenants,offered_rps,hot_rps,achieved_rps,p50_ms,p95_ms,p99_ms,max_ms,errors,busiest_p95_ms,rest_p95_ms,hot_p95_ms,hot_shed,eunha_cpu_pct,postgres_cpu_pct,pgbouncer_cpu_pct,backends_mean,backends_max,clients_max,footprint_after_mib,footprint_cooldown_mib,decompressions_per_s\n' > "$RESULTS/load.csv"

echo "results: $RESULTS"
for count in $COUNTS; do
  while [ "$PROVISIONED" -lt "$count" ]; do PROVISIONED=$((PROVISIONED + 1)); provision "$PROVISIONED"; done
  stop_eunha
  start_eunha "$count"
  read -r all p50 failed < <(node -e 'const j = require(process.argv[1]); console.log(j.all_healthy_ms, j.p50_ms, j.failed)' "$WORK/wait.json")
  echo "$count tenants answering after ${all} ms (median ${p50} ms, ${failed} failed)"

  # Every tenant posts a little first, so that timelines and notifications
  # are not empty when they are read. Tenants carried over from the last step
  # post again, which does them no harm.
  node - "$PORT" "$(hosts_upto "$count")" "$WARM_POSTS" <<'NODE'
const http = require("node:http");
const [port, hosts, posts] = [Number(process.argv[2]), process.argv[3].split(","), Number(process.argv[4])];
const agent = new http.Agent({ keepAlive: true, maxSockets: 32 });
const jobs = hosts.flatMap((host) => Array.from({ length: posts }, (_, n) => [host, n]));
const one = ([host, n]) => new Promise((resolve) => {
  const body = JSON.stringify({ status: `warm-up ${n} on ${host} @user${1 + (n % 10)}`, visibility: "public" });
  const req = http.request({ agent, host: "127.0.0.1", port, method: "POST", path: "/api/v1/statuses",
    headers: { host, "content-type": "application/json", "content-length": Buffer.byteLength(body),
      authorization: `Bearer eunha-bench-token-${1 + (n % 10)}` } }, (res) => { res.resume(); res.on("end", resolve); });
  req.on("error", resolve);
  req.end(body);
});
(async () => {
  for (let i = 0; i < jobs.length; i += 32) await Promise.all(jobs.slice(i, i + 32).map(one));
  agent.destroy();
})();
NODE

  echo "settling ${SETTLE}s"
  sleep "$SETTLE"
  read -r e0 p0 b0 x0 s0 c0 d0 < <(counters)
  : > "$WORK/conn-idle"
  sample_connections "$WORK/conn-idle" & sampler=$!
  sleep "$WINDOW"
  kill "$sampler"; wait "$sampler" 2>/dev/null || true
  read -r e1 p1 b1 x1 s1 c1 d1 < <(counters)
  read -r fp th < <(footprint)
  read -r bmean bmax cmean cmax < <(summarize_connections "$WORK/conn-idle")
  private=$(backend_private_mib)
  db_mib=$(pgq -d postgres -c "SELECT round(avg(pg_database_size(datname)) / 1048576.0, 1) FROM pg_database WHERE datname LIKE 'eunha\_t%'")
  redis_mib=$(redis-cli -p "$REDIS_PORT" info memory | awk -F: '/^used_memory:/ {printf "%.1f", $2 / 1048576}')
  awk -v n="$count" -v all="$all" -v p50="$p50" -v f="$failed" -v fp="$fp" -v th="$th" -v w="$WINDOW" \
      -v e="$(echo "$e1 - $e0" | bc)" -v p="$(echo "$p1 - $p0" | bc)" -v b="$(echo "$b1 - $b0" | bc)" \
      -v x=$((x1 - x0)) -v s=$((s1 - s0)) -v c=$((c1 - c0)) -v d=$((d1 - d0)) \
      -v bmean="$bmean" -v bmax="$bmax" -v cmean="$cmean" -v cmax="$cmax" -v pv="$private" -v db="$db_mib" -v rm="$redis_mib" -v fr="$(free_percent)" \
      'BEGIN { printf "%d,%d,%d,%d,%s,%.2f,%d,%.2f,%.2f,%.2f,%.2f,%.2f,%.2f,%s,%d,%s,%d,%s,%s,%s,%d,%.1f\n",
        n, all, p50, f, fp, fp / n, th, e / w * 100, p / w * 100, b / w * 100, x / w, s / w * 60, c / w, bmean, bmax, cmean, cmax, pv, db, rm, fr, d / w }' \
      >> "$RESULTS/idle.csv"
  tail -1 "$RESULTS/idle.csv"

  cases=()
  for rps in $LOAD_RPS; do cases+=("$rps:0"); done
  [ "$HOT_RPS" -gt 0 ] && [ "$count" -gt 1 ] && cases+=("$HOT_BASE_RPS:$HOT_RPS")
  for c in "${cases[@]}"; do
    rps=${c%:*} hot=${c#*:}
    out="$RESULTS/load-$count-$rps-$hot.json"
    read -r e0 p0 b0 _ _ _ d0 < <(counters)
    : > "$WORK/conn-load"
    sample_connections "$WORK/conn-load" & sampler=$!
    run_load "$count" "$rps" "$hot" "$out"
    kill "$sampler"; wait "$sampler" 2>/dev/null || true
    read -r e1 p1 b1 _ _ _ d1 < <(counters)
    read -r fp_after _ < <(footprint)
    read -r bmean bmax _ cmax < <(summarize_connections "$WORK/conn-load")
    sleep "$COOLDOWN"
    read -r fp_cool _ < <(footprint)
    node -e '
      const [file, e, p, b, w, bmean, bmax, cmax, fa, fc, d] = process.argv.slice(1);
      const j = require(file), l = j.latency_ms, g = j.by_group;
      const f = (x) => (x === undefined ? "" : x.toFixed(1));
      console.log([j.tenants, j.offered_rps, j.hot_rps, j.achieved_rps.toFixed(1), f(l.p50), f(l.p95), f(l.p99), f(l.max), l.errors,
        f(g.busiest_tenth?.p95), f(g.rest?.p95), f(g.hot?.p95), g.hot?.shed ?? "",
        (e / w * 100).toFixed(1), (p / w * 100).toFixed(1), (b / w * 100).toFixed(1), bmean, bmax, cmax, fa, fc, (d / w).toFixed(1)].join(","));
    ' "$out" "$(echo "$e1 - $e0" | bc)" "$(echo "$p1 - $p0" | bc)" "$(echo "$b1 - $b0" | bc)" "$LOAD_SECONDS" \
      "$bmean" "$bmax" "$cmax" "$fp_after" "$fp_cool" "$((d1 - d0))" >> "$RESULTS/load.csv"
    tail -1 "$RESULTS/load.csv"
  done
done

echo "load_average_at_end=$(sysctl -n vm.loadavg)" >> "$RESULTS/config.txt"
printf '\n%s\n' "$RESULTS"
for f in idle load; do printf '\n%s\n' "$f"; column -s, -t < "$RESULTS/$f.csv"; done
