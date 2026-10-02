#!/usr/bin/env bash
# How many eunha tenants does one host hold?
#
# Brings up a PostgreSQL cluster and a Redis server of its own, then starts
# tenants in steps — one eunha process and one database each, configured the
# way docs/design/benchmarking.md recommends for a small hosted instance — and
# at every step measures what the whole population costs:
#
#  -  idle, after the pools' idle timeout has passed, which is what a host pays
#     for tenants nobody is using. Most tenants of a hosting service are idle
#     most of the time, so this, not throughput, usually decides density;
#  -  under an open-loop request load spread across every tenant
#     (benchmark_density_load.mjs), at each host-wide rate in LOAD_RPS.
#
# At the end it stops a burst of tenants and starts them all at once, which is
# what waking dormant tenants after an outage or a deploy looks like.
#
# Its own servers, rather than the developer's, because the point is to find
# where PostgreSQL runs out, and a cluster with max_connections=100 runs out
# before anything interesting happens. Everything is removed on exit; results
# go to benchmark-results/density-*.
#
# Traps:
#
#  -  eunha's memory is summed per process from `top`'s MEM column
#     (phys_footprint), not from RSS. RSS counts the binary's text once per
#     process although every process shares the same pages, and overstates the
#     marginal cost of a tenant by more than half.
#  -  PostgreSQL's is not, because a backend's footprint includes every page of
#     shared_buffers it has touched, so summing footprints counts the buffer
#     pool once per connection. A few backends are sampled with `vmmap`, the
#     shared mapping (its "Untagged" region) is subtracted to leave what the
#     connection costs privately, and shared_buffers is added once.
#  -  An idle tenant's cost is periodic, and the defaults below are shorter
#     than its period. A fresh tenant's queue loops back off from half a
#     second to `queue_idle_poll_seconds` over about twice that long, and its
#     timed tasks wake once per poll, so with the hosting profile's 300 seconds
#     a 75-second settle measures the ramp and a 60-second window can fall
#     between two rounds of wake-ups and report none. Tenants started together
#     also wake together. To measure idle connections, settle for at least
#     twice the poll and sample for at least one whole one.
#  -  CPU is summed over the processes alive at each end of a window, and a
#     backend that exits inside it takes its CPU time with it. When pools close
#     idle connections, PostgreSQL's CPU is a lower bound — it can even come out
#     negative — and postgres_sessions_per_min is the column to read instead.
#  -  pg_stat_database counters are flushed by backends at most once a second,
#     and by idle ones up to ten seconds late, so a short window misreports the
#     idle transaction rate. Keep WINDOW well above ten seconds.
#  -  Without LC_ALL, macOS's locale lookup starts a thread inside the
#     postmaster, which refuses to run multithreaded and exits before listening.
#  -  max_files_per_process is lowered, because PostgreSQL's default of 1000
#     times a thousand backends exceeds macOS's host-wide kern.maxfiles and
#     would starve every other program on the machine of file descriptors.
#  -  The run stops adding tenants when free memory falls below
#     MIN_FREE_PERCENT, rather than pushing a developer machine into swap and
#     reporting the swap.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
EUNHA=${EUNHA_BIN:-$ROOT/target/release/eunha}
COUNTS=${EUNHA_DENSITY_COUNTS:-"10 50 100 200"}
LOAD_RPS=${EUNHA_DENSITY_LOAD_RPS:-"50 200 800"}
WAKE_BURSTS=${EUNHA_DENSITY_WAKE_BURSTS:-"10 50"}
SETTLE=${EUNHA_DENSITY_SETTLE_SECONDS:-75}
WINDOW=${EUNHA_DENSITY_WINDOW_SECONDS:-60}
LOAD_SECONDS=${EUNHA_DENSITY_LOAD_SECONDS:-30}
POOL=${EUNHA_DENSITY_POOL:-2}
IDLE_TIMEOUT=${EUNHA_DENSITY_IDLE_TIMEOUT:-60}
QUEUE_IDLE_POLL=${EUNHA_DENSITY_QUEUE_IDLE_POLL:-}
LOG_FILTER=${EUNHA_DENSITY_LOG:-warn}
MIN_FREE_PERCENT=${EUNHA_DENSITY_MIN_FREE_PERCENT:-15}
PG_PORT=${EUNHA_DENSITY_PG_PORT:-55432}
PG_MAX_CONNECTIONS=${EUNHA_DENSITY_PG_MAX_CONNECTIONS:-1000}
PG_SHARED_BUFFERS=${EUNHA_DENSITY_PG_SHARED_BUFFERS:-1GB}
PG_MAX_FILES=${EUNHA_DENSITY_PG_MAX_FILES:-100}
REDIS_PORT=${EUNHA_DENSITY_REDIS_PORT:-56379}
BASE_PORT=${EUNHA_DENSITY_BASE_PORT:-21000}
RESULTS=${EUNHA_DENSITY_RESULTS:-$ROOT/benchmark-results/density-$(date +%Y%m%d-%H%M%S)}

# Tokio sizes its worker pool from this when set; unset, one worker per core.
if [ -n "${EUNHA_DENSITY_WORKER_THREADS:-}" ]; then
  export TOKIO_WORKER_THREADS=$EUNHA_DENSITY_WORKER_THREADS
fi

WORK=$(mktemp -d "${TMPDIR:-/tmp}/eunha-density.XXXXXX")
mkdir -p "$RESULTS"
PIDS=()
STARTED=0

cat > "$RESULTS/config.txt" <<EOF
binary=$EUNHA
host=$(sysctl -n hw.model) cores=$(sysctl -n hw.ncpu) memory_gib=$(( $(sysctl -n hw.memsize) / 1073741824 ))
pool=$POOL idle_timeout=$IDLE_TIMEOUT queue_idle_poll=${QUEUE_IDLE_POLL:-default} worker_threads=${TOKIO_WORKER_THREADS:-cores}
postgres max_connections=$PG_MAX_CONNECTIONS shared_buffers=$PG_SHARED_BUFFERS max_files_per_process=$PG_MAX_FILES
settle=$SETTLE window=$WINDOW load_seconds=$LOAD_SECONDS log=$LOG_FILTER
EOF

cleanup() {
  for pid in "${PIDS[@]:-}"; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
  [ -f "$WORK/pg.log" ] && tail -200 "$WORK/pg.log" > "$RESULTS/postgres.log.tail"
  [ -f "$WORK/t1/eunha.log" ] && tail -200 "$WORK/t1/eunha.log" > "$RESULTS/tenant1.log.tail"
  pg_ctl -D "$WORK/pg" -m fast stop >/dev/null 2>&1 || true
  redis-cli -p "$REDIS_PORT" shutdown nosave >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

pgq() { psql -h 127.0.0.1 -p "$PG_PORT" -U eunha -At -F ' ' -v ON_ERROR_STOP=1 "$@"; }
now_ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'; }
free_percent() { memory_pressure | awk -F': ' '/free percentage/ {print $2+0}'; }
join_pids() { local IFS=,; echo "$*"; }

# ── Servers ────────────────────────────────────────────────────────────────

initdb -D "$WORK/pg" -U eunha -A trust -E UTF8 --locale=C >/dev/null
cat >> "$WORK/pg/postgresql.conf" <<EOF
port = $PG_PORT
listen_addresses = '127.0.0.1'
unix_socket_directories = ''
max_connections = $PG_MAX_CONNECTIONS
shared_buffers = $PG_SHARED_BUFFERS
max_files_per_process = $PG_MAX_FILES
EOF
LC_ALL=C pg_ctl -D "$WORK/pg" -l "$WORK/pg.log" -w start >/dev/null
redis-server --port "$REDIS_PORT" --bind 127.0.0.1 --save '' --appendonly no \
  --daemonize yes --dir "$WORK" --logfile "$WORK/redis.log" >/dev/null

pgq -d postgres -c "CREATE DATABASE eunha_template" >/dev/null
(cd "$WORK" && DATABASE_URL="postgres://eunha@127.0.0.1:$PG_PORT/eunha_template" "$EUNHA" migrate >/dev/null)
pgq -q -d eunha_template -f "$ROOT/scripts/benchmark_seed.sql" >/dev/null
shared_buffers_mib=$(pgq -d postgres -c "SELECT setting::bigint * 8 / 1024 FROM pg_settings WHERE name = 'shared_buffers'")

vapid=$(openssl ecparam -genkey -name prime256v1 -noout 2>/dev/null)
vapid_priv=$(printf '%s' "$vapid" | openssl pkcs8 -topk8 -nocrypt 2>/dev/null)
vapid_pub=$(printf '%s' "$vapid" | openssl ec -pubout -outform DER 2>/dev/null | tail -c 65 | base64 | tr '+/' '-_' | tr -d '=\n')

# ── Tenants ────────────────────────────────────────────────────────────────

provision() {
  local i=$1 dir="$WORK/t$1" port=$((BASE_PORT + $1))
  pgq -d postgres -c "CREATE DATABASE eunha_t$i TEMPLATE eunha_template" >/dev/null
  mkdir -p "$dir"
  cat > "$dir/config.toml" <<EOF
database_url = "postgres://eunha@127.0.0.1:$PG_PORT/eunha_t$i"
redis_url = "redis://127.0.0.1:$REDIS_PORT/0"
redis_key_prefix = "t$i"
redis_process_metrics = false
bind_address = "127.0.0.1:$port"
software_update_url = ""

[database_pool]
max_connections = $POOL
min_connections = 0
acquire_timeout_seconds = 5
idle_timeout_seconds = $IDLE_TIMEOUT

[instance]
domain = "127.0.0.1:$port"
title = "Eunha density benchmark $i"
description = ""
short_description = ""
contact_email = "user1@bench.invalid"
registrations_open = false
approval_required = false
vapid_private_key = """
$vapid_priv
"""
vapid_public_key = "$vapid_pub"
privacy_policy = ""
terms_of_service = ""

[media_storage]
bucket = "benchmark"
region = "auto"
endpoint = "http://127.0.0.1:9"
access_key_id = "benchmark"
secret_access_key = "benchmark"
base_url = "http://127.0.0.1:9"

EOF
  if [ -n "$QUEUE_IDLE_POLL" ]; then
    printf '\n[workers]\nqueue_idle_poll_seconds = %s\n' "$QUEUE_IDLE_POLL" >> "$dir/config.toml"
  fi
}

spawn() {
  local i=$1
  (cd "$WORK/t$i"; RUST_LOG=$LOG_FILTER exec "$EUNHA" >>eunha.log 2>&1) &
  PIDS[i]=$!
}

# Milliseconds from `since` until the tenant answers, or failure after a minute.
wait_healthy() {
  local port=$((BASE_PORT + $1)) since=$2
  for _ in $(seq 1 1200); do
    if curl -sf -o /dev/null "http://127.0.0.1:$port/api/v1/instance"; then
      echo $(( $(now_ms) - since ))
      return 0
    fi
    kill -0 "${PIDS[$1]}" 2>/dev/null || { echo "tenant $1 exited; see its log" >&2; tail -5 "$WORK/t$1/eunha.log" >&2; return 1; }
    sleep 0.05
  done
  echo "tenant $1 did not become healthy" >&2
  return 1
}

running_pids() { local i; for i in $(seq 1 "$STARTED"); do printf '%s\n' "${PIDS[$i]}"; done; }
postgres_pids() { local pm; pm=$(head -1 "$WORK/pg/postmaster.pid"); echo "$pm"; pgrep -P "$pm"; }

# ── Measurement ───────────────────────────────────────────────────────────

cpu_seconds() {
  ps -o time= -p "$(join_pids "$@")" 2>/dev/null |
    awk -F: '{ s = 0; for (i = 1; i <= NF; i++) s = s * 60 + $i; t += s } END { printf "%.2f\n", t }'
}

# eunha CPU s, postgres CPU s, committed transactions, sessions opened, Redis
# commands, and pages the host has decompressed. The last is how memory
# pressure shows up on macOS before any swap does: an idle tenant's pages are
# compressed, and a request to it waits for them to be decompressed.
counters() {
  local eunha pg db cmds decompressions
  eunha=$(cpu_seconds $(running_pids))
  pg=$(cpu_seconds $(postgres_pids))
  db=$(pgq -d postgres -c "SELECT coalesce(sum(xact_commit + xact_rollback), 0), coalesce(sum(sessions), 0) FROM pg_stat_database WHERE datname LIKE 'eunha\_t%'")
  cmds=$(redis-cli -p "$REDIS_PORT" info stats | awk -F: '/^total_commands_processed/ {print $2+0}')
  decompressions=$(vm_stat | awk '/^Decompressions:/ {gsub(/\./, "", $2); print $2}')
  echo "$eunha $pg $db $cmds $decompressions"
}

# Sum of phys_footprint (MiB) and threads over a set of pids, from one `top`.
footprint_of() {
  top -l 1 -stats pid,mem,th 2>/dev/null | awk -v list="$1" '
    BEGIN { n = split(list, p, ","); for (i = 1; i <= n; i++) want[p[i]] = 1 }
    ($1 in want) {
      m = $2; sub(/[+-]$/, "", m)
      unit = substr(m, length(m)); v = substr(m, 1, length(m) - 1) + 0
      if (unit == "K") v /= 1024; else if (unit == "G") v *= 1024; else if (unit == "B") v /= 1048576
      mem += v; th += $3 + 0
    }
    END { printf "%.1f %d\n", mem, th }'
}

# Mean private memory (MiB) of up to five tenant backends: footprint less the
# shared mapping, which is the buffer pool every backend's footprint repeats.
# The oldest are sampled, and one that closes between being listed and being
# inspected is skipped: a pool with a short idle timeout closes them constantly,
# and vmmap exits 255 on a process that has gone.
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

printf 'tenants,startup_ms_p50,startup_ms_max,eunha_rss_mib,eunha_footprint_mib,footprint_mib_per_tenant,eunha_threads,eunha_cpu_pct,postgres_cpu_pct,postgres_tps,postgres_sessions_per_min,redis_ops_per_s,postgres_backends_mean,postgres_backends_max,postgres_private_mib_per_backend,postgres_est_mib,database_mib_per_tenant,redis_used_mib,open_files,free_percent,decompressions_per_s\n' > "$RESULTS/idle.csv"
printf 'tenants,offered_rps,achieved_rps,p50_ms,p95_ms,p99_ms,max_ms,errors,dropped,eunha_cpu_pct,postgres_cpu_pct,postgres_backends_mid,decompressions_per_s\n' > "$RESULTS/load.csv"
printf 'burst,all_healthy_ms,p50_ms,failed\n' > "$RESULTS/wake.csv"

echo "results: $RESULTS"
for count in $COUNTS; do
  # BSD seq counts down when its bounds are reversed, so a step that adds no
  # tenants has to be skipped rather than left to `seq`.
  [ "$count" -gt "$STARTED" ] || continue
  free=$(free_percent)
  if [ "$free" -lt "$MIN_FREE_PERCENT" ]; then
    echo "stopping at $STARTED tenants: $free% memory free, below $MIN_FREE_PERCENT%" >&2
    break
  fi

  # Start one at a time, so each startup time is a tenant starting on a host
  # already carrying the others, not a tenant competing with its siblings.
  startups="$WORK/startup-$count.txt"
  : > "$startups"
  for i in $(seq $((STARTED + 1)) "$count"); do
    provision "$i"
    since=$(now_ms)
    spawn "$i"
    wait_healthy "$i" "$since" >> "$startups"
    STARTED=$i
  done
  read -r startup_p50 startup_max < <(sort -n "$startups" | awk '{v[NR] = $1} END {print v[int((NR + 1) / 2)] + 0, v[NR] + 0}')

  echo "$count tenants up; settling ${SETTLE}s"
  sleep "$SETTLE"

  read -r e0 p0 x0 s0 c0 d0 < <(counters)
  # Connections are counted throughout the window rather than once at its end.
  # A pool that closes idle connections holds one only while a periodic task
  # runs, and a single count says more about when it was taken than about what
  # the tenants hold.
  : > "$WORK/backends-$count"
  for _ in $(seq 1 $((WINDOW / 2))); do
    pgq -d postgres -c "SELECT count(*) FROM pg_stat_activity WHERE datname LIKE 'eunha\_t%'" >> "$WORK/backends-$count"
    sleep 2
  done
  read -r e1 p1 x1 s1 c1 d1 < <(counters)
  pids=$(join_pids $(running_pids))
  read -r footprint threads < <(footprint_of "$pids")
  rss=$(ps -o rss= -p "$pids" | awk '{t += $1} END {printf "%.1f", t / 1024}')
  read -r backends backends_max < <(awk '{ t += $1; if ($1 > m) m = $1 } END { printf "%.1f %d\n", t / NR, m }' "$WORK/backends-$count")
  private=$(backend_private_mib)
  db_mib=$(pgq -d postgres -c "SELECT round(avg(pg_database_size(datname)) / 1048576.0, 1) FROM pg_database WHERE datname LIKE 'eunha\_t%'")
  redis_mib=$(redis-cli -p "$REDIS_PORT" info memory | awk -F: '/^used_memory:/ {printf "%.1f", $2 / 1048576}')
  awk -v n="$count" -v sp="$startup_p50" -v sm="$startup_max" -v rss="$rss" -v fp="$footprint" -v th="$threads" \
      -v e="$(echo "$e1 - $e0" | bc)" -v p="$(echo "$p1 - $p0" | bc)" -v x=$((x1 - x0)) -v s=$((s1 - s0)) -v c=$((c1 - c0)) -v w="$WINDOW" \
      -v b="$backends" -v bm="$backends_max" -v pv="$private" -v sb="$shared_buffers_mib" -v db="$db_mib" -v rm="$redis_mib" \
      -v of="$(sysctl -n kern.num_files)" -v fr="$(free_percent)" -v d=$((d1 - d0)) \
      'BEGIN {printf "%d,%d,%d,%s,%s,%.2f,%d,%.1f,%.1f,%.1f,%.1f,%.1f,%.1f,%d,%.2f,%.0f,%s,%s,%d,%d,%.1f\n", n, sp, sm, rss, fp, fp / n, th, e / w * 100, p / w * 100, x / w, s / w * 60, c / w, b, bm, pv, bm * pv + sb, db, rm, of, fr, d / w}' \
      >> "$RESULTS/idle.csv"
  tail -1 "$RESULTS/idle.csv"

  ports=$(for i in $(seq 1 "$STARTED"); do printf '%s\n' $((BASE_PORT + i)); done | paste -sd, -)
  for rps in $LOAD_RPS; do
    read -r e0 p0 _ _ _ d0 < <(counters)
    ( sleep $((LOAD_SECONDS / 2)); pgq -d postgres -c "SELECT count(*) FROM pg_stat_activity WHERE datname LIKE 'eunha\_t%'" > "$WORK/mid-backends" ) &
    EUNHA_DENSITY_PORTS=$ports EUNHA_DENSITY_RPS=$rps EUNHA_DENSITY_SECONDS=$LOAD_SECONDS \
      node "$ROOT/scripts/benchmark_density_load.mjs" > "$RESULTS/load-$count-$rps.json"
    wait $!
    read -r e1 p1 _ _ _ d1 < <(counters)
    node -e '
      const [file, e, p, w, mid, d] = process.argv.slice(1);
      const j = require(file);
      const l = j.latency_ms;
      console.log([j.tenants, j.offered_rps, j.achieved_rps.toFixed(1), l.p50.toFixed(1), l.p95.toFixed(1), l.p99.toFixed(1), l.max.toFixed(1),
        j.errors, j.dropped, (e / w * 100).toFixed(1), (p / w * 100).toFixed(1), mid.trim(), (d / w).toFixed(1)].join(","));
    ' "$RESULTS/load-$count-$rps.json" "$(echo "$e1 - $e0" | bc)" "$(echo "$p1 - $p0" | bc)" "$LOAD_SECONDS" "$(cat "$WORK/mid-backends")" "$((d1 - d0))" >> "$RESULTS/load.csv"
    tail -1 "$RESULTS/load.csv"
  done
done

# ── Waking a burst ────────────────────────────────────────────────────────

for burst in $WAKE_BURSTS; do
  [ "$burst" -le "$STARTED" ] || continue
  for i in $(seq 1 "$burst"); do kill "${PIDS[$i]}" 2>/dev/null || true; done
  for i in $(seq 1 "$burst"); do wait "${PIDS[$i]}" 2>/dev/null || true; done
  since=$(now_ms)
  for i in $(seq 1 "$burst"); do spawn "$i"; done
  EUNHA_DENSITY_PORTS=$(for i in $(seq 1 "$burst"); do printf '%s\n' $((BASE_PORT + i)); done | paste -sd, -) \
    EUNHA_DENSITY_SINCE_MS=$since node "$ROOT/scripts/benchmark_density_wake.mjs" > "$WORK/wake-$burst.json"
  node -e 'const j = require(process.argv[1]); console.log([j.burst, j.all_healthy_ms, j.p50_ms, j.failed].join(","))' \
    "$WORK/wake-$burst.json" >> "$RESULTS/wake.csv"
  tail -1 "$RESULTS/wake.csv"
done

printf '\n%s\n' "$RESULTS"
for f in idle load wake; do printf '\n%s\n' "$f"; column -s, -t < "$RESULTS/$f.csv"; done
