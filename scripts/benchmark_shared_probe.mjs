// Two small jobs against a shared eunha process, by `Host`:
//
//  -  `wait`: poll every instance's health check until each answers, and
//     report how long after EUNHA_SHARED_SINCE_MS the last and the median did.
//     One process polls every host, rather than a curl loop each, so that the
//     harness does not take the cores the instances are starting on.
//  -  `sweep`: what eunha.space's control plane does to every instance it
//     runs — one health check each per EUNHA_SHARED_SWEEP_SECONDS (300),
//     spread across the period — until killed.
import http from "node:http";
import { setTimeout as sleep } from "node:timers/promises";

const port = Number(process.env.EUNHA_SHARED_PORT);
const hosts = process.env.EUNHA_SHARED_HOSTS.split(",");
const agent = new http.Agent({ keepAlive: true, maxSockets: 64 });

function health(host) {
  return new Promise((resolve) => {
    const req = http.request({ agent, host: "127.0.0.1", port, path: "/api/eunha/v1/health", headers: { host }, timeout: 2_000 }, (res) => {
      res.resume();
      res.on("end", () => resolve(res.statusCode === 200));
    });
    req.on("timeout", () => req.destroy());
    req.on("error", () => resolve(false));
    req.end();
  });
}

if (process.argv[2] === "wait") {
  const since = Number(process.env.EUNHA_SHARED_SINCE_MS);
  const deadline = Date.now() + Number(process.env.EUNHA_SHARED_WAIT_SECONDS ?? 120) * 1000;
  const times = await Promise.all(hosts.map(async (host) => {
    while (Date.now() < deadline) {
      if (await health(host)) return Date.now() - since;
      await sleep(50);
    }
    return null;
  }));
  const ok = times.filter((t) => t !== null).sort((a, b) => a - b);
  console.log(JSON.stringify({
    tenants: hosts.length,
    failed: hosts.length - ok.length,
    all_healthy_ms: ok.at(-1) ?? null,
    p50_ms: ok[Math.floor((ok.length - 1) / 2)] ?? null,
  }));
  agent.destroy();
} else {
  const period = Number(process.env.EUNHA_SHARED_SWEEP_SECONDS ?? 300) * 1000;
  const gap = period / hosts.length;
  for (let i = 0; ; i = (i + 1) % hosts.length) {
    health(hosts[i]);
    await sleep(gap);
  }
}
