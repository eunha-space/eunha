// Open-loop load on one shared eunha process, the way a hosting service's
// traffic reaches it: every request goes to the same port and names its
// instance in `Host`, and instances are not equally busy.
//
// Which instance a request goes to follows a Zipf law over the instances'
// ranks (EUNHA_SHARED_SKEW, 1 by default): the first is the busiest, and most
// of the long tail sees a request every few seconds or less. 0 spreads the
// load evenly, as the density benchmark does.
//
// EUNHA_SHARED_HOT_RPS adds a second stream, all of it to instance 1, on top of
// the spread: one instance saturating while its neighbours carry on. Its
// requests are reported apart from everyone else's.
//
// Latency is measured from when a request was due, not from when it was sent,
// so a saturated server shows up as latency rather than as a slower load.
import http from "node:http";
import { performance } from "node:perf_hooks";
import { setTimeout as sleep } from "node:timers/promises";

const port = Number(process.env.EUNHA_SHARED_PORT);
const hosts = process.env.EUNHA_SHARED_HOSTS.split(",");
const rate = Number(process.env.EUNHA_SHARED_RPS ?? 100);
const hotRate = Number(process.env.EUNHA_SHARED_HOT_RPS ?? 0);
const seconds = Number(process.env.EUNHA_SHARED_SECONDS ?? 60);
const skew = Number(process.env.EUNHA_SHARED_SKEW ?? 1);
const maxInflight = Number(process.env.EUNHA_SHARED_MAX_INFLIGHT ?? 4000);
const writes = process.env.EUNHA_SHARED_WRITES !== "0";

const agent = new http.Agent({ keepAlive: true, maxSockets: 1024 });

// What a small hosted instance is asked: its members' clients (signed in), and
// the rest of the fediverse and logged-out visitors (not). Federation inboxes
// are not here; the spike benchmark covers them.
const post = (n) => ({
  method: "POST",
  path: "/api/v1/statuses",
  auth: true,
  body: JSON.stringify({ status: `shared status ${n}`, visibility: "public" }),
  headers: { "content-type": "application/json", "idempotency-key": `shared-${process.pid}-${n}` },
});
const get = (path, auth, headers = {}) => () => ({ method: "GET", path, auth, headers });
const mix = [
  [28, "home", get("/api/v1/timelines/home?limit=20", true)],
  [14, "notifications", get("/api/v1/notifications?limit=20", true)],
  [10, "public_local", get("/api/v1/timelines/public?local=true&limit=20", true)],
  [8, "verify_credentials", get("/api/v1/accounts/verify_credentials", true)],
  [4, "post", post],
  [10, "instance", get("/api/v2/instance", false)],
  [8, "webfinger", null], // built per host below
  [6, "lookup", get("/api/v1/accounts/lookup?acct=user1", false)],
  [6, "nodeinfo", get("/nodeinfo/2.0", false)],
  [6, "public_anonymous", get("/api/v1/timelines/public?local=true&limit=20", false)],
].filter(([, name]) => writes || name !== "post");
const totalWeight = mix.reduce((sum, [w]) => sum + w, 0);

// Zipf weights over ranks, as a cumulative table for sampling.
const weights = hosts.map((_, i) => 1 / Math.pow(i + 1, skew));
const totalHostWeight = weights.reduce((a, b) => a + b, 0);
const cumulative = [];
weights.reduce((acc, w, i) => (cumulative[i] = acc + w / totalHostWeight), 0);
function pickHost() {
  const r = Math.random();
  let lo = 0, hi = cumulative.length - 1;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (cumulative[mid] < r) lo = mid + 1; else hi = mid;
  }
  return lo;
}
function pickRequest() {
  let r = Math.random() * totalWeight;
  for (const entry of mix) if ((r -= entry[0]) < 0) return entry;
  return mix[0];
}

// Busiest tenth of instances, the rest, and the hot one when there is one.
const hotCut = Math.max(1, Math.ceil(hosts.length / 10));
const groups = {};
const group = (name) => (groups[name] ??= { latencies: [], errors: 0, shed: 0 });
const byRequest = {};
const statusCounts = {};
let inflight = 0, dropped = 0, sequence = 0;
const pending = new Set();

function send(hostIndex, name, spec) {
  const host = hosts[hostIndex];
  return new Promise((resolve) => {
    const headers = { host, ...spec.headers };
    if (spec.auth) headers.authorization = `Bearer eunha-bench-token-${1 + Math.floor(Math.random() * 10)}`;
    // Visitors and other servers arrive through the proxy from addresses of
    // their own, as they do behind eunha.space's HAProxy.
    else headers["x-forwarded-for"] = `198.51.${Math.floor(Math.random() * 256)}.${1 + Math.floor(Math.random() * 254)}`;
    if (spec.body) headers["content-length"] = Buffer.byteLength(spec.body);
    const req = http.request({ agent, host: "127.0.0.1", port, method: spec.method, path: spec.path, headers, timeout: 15_000 }, (res) => {
      res.resume();
      res.on("end", () => resolve(res.statusCode));
      res.on("error", () => resolve(0));
    });
    req.on("timeout", () => req.destroy());
    req.on("error", () => resolve(0));
    if (spec.body) req.write(spec.body);
    req.end();
  });
}

async function fire(due, hot) {
  const n = sequence++;
  const hostIndex = hot ? 0 : pickHost();
  const [, name, build] = pickRequest();
  const spec = build ? build(n) : get(`/.well-known/webfinger?resource=acct:user1@${hosts[hostIndex]}`, false)();
  const status = await send(hostIndex, name, spec);
  const latency = performance.now() - due;
  statusCounts[status] = (statusCounts[status] ?? 0) + 1;
  const g = group(hot ? "hot" : hostIndex < hotCut ? "busiest_tenth" : "rest");
  const r = (byRequest[name] ??= { latencies: [], errors: 0 });
  for (const s of [g, r]) {
    s.latencies.push(latency);
    if (status < 200 || status >= 400) s.errors++;
  }
  if (status === 503) g.shed++;
}

function stream(rps, hot) {
  if (rps <= 0) return Promise.resolve();
  return (async () => {
    const start = performance.now();
    const deadline = start + seconds * 1000;
    let next = start;
    while (next < deadline) {
      const now = performance.now();
      while (next <= now && next < deadline) {
        if (inflight >= maxInflight) {
          dropped++;
        } else {
          inflight++;
          const p = fire(next, hot).finally(() => { inflight--; pending.delete(p); });
          pending.add(p);
        }
        next += (-Math.log(1 - Math.random()) / rps) * 1000; // Poisson arrivals
      }
      await sleep(Math.max(0, Math.min(5, next - performance.now())));
    }
  })();
}

const start = performance.now();
await Promise.all([stream(rate, false), stream(hotRate, true)]);
await Promise.all(pending);
const elapsed = (performance.now() - start) / 1000;
agent.destroy();

const percentile = (sorted, p) => sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))] ?? 0;
const summarize = ({ latencies, errors, shed }) => {
  const sorted = [...latencies].sort((a, b) => a - b);
  const out = { count: sorted.length, p50: percentile(sorted, 0.5), p95: percentile(sorted, 0.95), p99: percentile(sorted, 0.99), max: sorted.at(-1) ?? 0, errors };
  if (shed !== undefined) out.shed = shed;
  return out;
};
const everyone = Object.entries(groups).filter(([k]) => k !== "hot").flatMap(([, g]) => g.latencies);
const everyoneErrors = Object.entries(groups).filter(([k]) => k !== "hot").reduce((s, [, g]) => s + g.errors, 0);
console.log(JSON.stringify({
  tenants: hosts.length,
  skew,
  offered_rps: rate,
  hot_rps: hotRate,
  achieved_rps: Object.values(groups).reduce((s, g) => s + g.latencies.length, 0) / elapsed,
  seconds: elapsed,
  dropped,
  statuses: statusCounts,
  latency_ms: summarize({ latencies: everyone, errors: everyoneErrors }),
  by_group: Object.fromEntries(Object.entries(groups).map(([k, g]) => [k, summarize(g)])),
  by_request: Object.fromEntries(Object.entries(byRequest).map(([k, g]) => [k, summarize(g)])),
}));
