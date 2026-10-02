#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d /private/tmp/eunha-small-benchmark.XXXXXX)
RESULTS=${EUNHA_BENCH_RESULTS:-$ROOT/benchmark-results/$(date +%Y%m%d-%H%M%S)}
POOLS=${EUNHA_BENCH_POOLS:-"1 2 3 5"}
BENCH_DURATION=${EUNHA_BENCH_SECONDS:-30}
CONCURRENCY=${EUNHA_BENCH_CONCURRENCY:-8}
mkdir -p "$RESULTS"
PIDS=()
DATABASES=()

cleanup() {
  for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  for db in "${DATABASES[@]:-}"; do dropdb --if-exists "$db" >/dev/null 2>&1 || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT

seed_database() {
  local db=$1
  psql -q -d "$db" -v ON_ERROR_STOP=1 -f "$ROOT/scripts/benchmark_seed.sql"
}

vapid=$(openssl ecparam -genkey -name prime256v1 -noout 2>/dev/null)
vapid_priv=$(printf '%s' "$vapid" | openssl pkcs8 -topk8 -nocrypt 2>/dev/null)
vapid_pub=$(printf '%s' "$vapid" | openssl ec -pubout -outform DER 2>/dev/null | tail -c 65 | base64 | tr '+/' '-_' | tr -d '=\n')

printf 'pool,idle_rss_mib,peak_rss_mib,avg_cpu_percent,peak_connections,peak_active_connections,rps,p50_ms,p95_ms,p99_ms,errors,db_mib\n' > "$RESULTS/summary.csv"

for pool in $POOLS; do
  db="eunha_bench_${$}_${pool}"
  port=$((18100 + pool))
  run="$WORK/pool-$pool"
  mkdir -p "$run"
  DATABASES+=("$db")
  createdb "$db"
  (cd "$run" && DATABASE_URL="postgres:///$db" "$ROOT/target/release/eunha" migrate >/dev/null)
  seed_database "$db" >/dev/null
  cat > "$run/config.toml" <<EOF
database_url = "postgres:///$db"
redis_url = "redis://127.0.0.1:6379/14"
redis_key_prefix = "benchmark-${$}-${pool}"
bind_address = "127.0.0.1:$port"
software_update_url = ""

[database_pool]
max_connections = $pool
min_connections = 0
acquire_timeout_seconds = 5
idle_timeout_seconds = 10

[instance]
domain = "127.0.0.1:$port"
title = "Eunha benchmark"
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
  (cd "$run"; exec "$ROOT/target/release/eunha" >eunha.log 2>&1) &
  pid=$!
  PIDS+=("$pid")
  for _ in $(seq 1 60); do
    curl -sf -o /dev/null "http://127.0.0.1:$port/api/v1/instance" && break
    sleep 0.25
  done
  curl -sf -o /dev/null "http://127.0.0.1:$port/api/v1/instance"
  sleep 12
  idle_rss=$(ps -o rss= -p "$pid" | tr -d ' ')
  samples="$run/samples.csv"
  printf 'rss_kib,cpu_percent,connections,active_connections\n' > "$samples"
  (
    while kill -0 "$pid" 2>/dev/null; do
      read -r rss cpu < <(ps -o rss=,%cpu= -p "$pid")
      read -r connections active < <(psql -At -d postgres -c "SELECT count(*), count(*) FILTER (WHERE state = 'active') FROM pg_stat_activity WHERE datname = '$db'" | tr '|' ' ')
      printf '%s,%s,%s,%s\n' "$rss" "$cpu" "$connections" "$active" >> "$samples"
      sleep 1
    done
  ) &
  sampler=$!
  EUNHA_BENCH_URL="http://127.0.0.1:$port" EUNHA_BENCH_SECONDS="$BENCH_DURATION" EUNHA_BENCH_CONCURRENCY="$CONCURRENCY" \
    node "$ROOT/scripts/benchmark_small_instance.mjs" > "$run/workload.json"
  kill "$sampler" 2>/dev/null || true
  wait "$sampler" 2>/dev/null || true
  read -r peak_rss avg_cpu peak_conn peak_active < <(awk -F, 'NR>1 {if($1>r)r=$1; c+=$2; n++; if($3>p)p=$3; if($4>a)a=$4} END {print r, c/n, p, a}' "$samples")
  read -r rps p50 p95 p99 errors < <(node -e 'const j=require(process.argv[1]); console.log(j.requests_per_second,j.latency_ms.p50,j.latency_ms.p95,j.latency_ms.p99,j.errors)' "$run/workload.json")
  db_bytes=$(psql -At -d postgres -c "SELECT pg_database_size('$db')")
  awk -v pool="$pool" -v idle="$idle_rss" -v peak="$peak_rss" -v cpu="$avg_cpu" -v conn="$peak_conn" -v active="$peak_active" -v rps="$rps" -v p50="$p50" -v p95="$p95" -v p99="$p99" -v errors="$errors" -v db="$db_bytes" 'BEGIN {printf "%s,%.2f,%.2f,%.2f,%s,%s,%.2f,%.2f,%.2f,%.2f,%s,%.2f\n",pool,idle/1024,peak/1024,cpu,conn,active,rps,p50,p95,p99,errors,db/1048576}' >> "$RESULTS/summary.csv"
  cp "$samples" "$RESULTS/pool-$pool-samples.csv"
  cp "$run/workload.json" "$RESULTS/pool-$pool-workload.json"
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
done

printf '%s\n' "$RESULTS"
cat "$RESULTS/summary.csv"
