//! A fake fediverse for load spikes.
//!
//! One process plays thousands of remote actors spread over many servers. It
//! serves their actor documents and notes when eunha fetches them, signs the
//! activities they send with real keys, and — as the HTTP proxy eunha is started
//! behind — answers every request eunha makes to the outside world, so that an
//! instance cloned from production can be hammered without a single request
//! reaching a real server, push service or mail provider.
//!
//!   eunha-fedisim --listen 127.0.0.1:18990 --eunha http://127.0.0.1:18900 \
//!       --domain seoul.earth
//!   curl -X POST 127.0.0.1:18990/__spikes -d '{"status_uri": "…", …}'
//!   curl 127.0.0.1:18990/__stats
//!
//! Simulated servers are `s<n>.fedisim.test`, reached only through the proxy,
//! over plain HTTP. A request for any other host is refused and counted: that
//! count should stay at zero, and anything in it is traffic that would have
//! left the machine. `CONNECT` — every `https://` URL — is refused the same way.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use clap::Parser;
use rand::seq::SliceRandom;
use rand::Rng;
use serde::Deserialize;
use serde_json::{json, Value};

const SIM_SUFFIX: &str = ".fedisim.test";
const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

#[derive(Parser, Debug)]
#[command(
    name = "eunha-fedisim",
    about = "Simulate remote servers for load spikes"
)]
struct Args {
    /// Where to listen: the proxy, the simulated servers and the control API.
    #[arg(long, default_value = "127.0.0.1:18990")]
    listen: SocketAddr,
    /// The eunha instance under test, as reached from here.
    #[arg(long)]
    eunha: String,
    /// The instance's own domain, which its signatures and `Host` must name.
    #[arg(long)]
    domain: String,
    /// Simulated servers.
    #[arg(long, default_value_t = 50)]
    servers: usize,
    /// Actors on each simulated server.
    #[arg(long, default_value_t = 200)]
    actors_per_server: usize,
    /// Distinct signing keys, shared round-robin between actors. Generating a
    /// key costs about 100 ms, and eunha cannot tell keys are shared.
    #[arg(long, default_value_t = 16)]
    keys: usize,
}

struct Key {
    private_pem: String,
    public_pem: String,
}

struct Sim {
    args: Args,
    keys: Vec<Key>,
    http: reqwest::Client,
    started: Instant,
    timeline: Mutex<HashMap<u64, Bucket>>,
    blocked_hosts: Mutex<HashMap<String, u64>>,
    notes: Mutex<HashMap<String, Value>>,
    spike: Mutex<Option<SpikeState>>,
    running: AtomicBool,
}

#[derive(Default)]
struct Bucket {
    sent: u64,
    ok: u64,
    client_error: u64,
    server_error: u64,
    transport_error: u64,
    shed: u64,
    latency_us: Vec<u32>,
    served_actor: u64,
    served_note: u64,
    served_other: u64,
    received_post: u64,
    blocked: u64,
}

#[derive(Clone, Deserialize)]
struct SpikeRequest {
    /// The local status that goes viral.
    status_uri: String,
    /// Its author, mentioned by replies and addressed by boosts.
    author_uri: String,
    /// Activities per second at the peak.
    #[serde(default = "default_peak")]
    peak_rps: f64,
    /// Seconds from the first activity to the peak.
    #[serde(default = "default_rise")]
    rise_seconds: f64,
    /// How long activities keep arriving.
    #[serde(default = "default_duration")]
    duration_seconds: f64,
    /// Relative weights of each kind of activity.
    #[serde(default = "default_mix")]
    mix: HashMap<String, f64>,
    /// Requests in flight at once. When the instance answers too slowly to keep
    /// up, activities beyond this are counted as shed rather than queued, so
    /// that the offered load stays what the curve says.
    #[serde(default = "default_concurrency")]
    concurrency: usize,
}

fn default_peak() -> f64 {
    100.0
}
fn default_rise() -> f64 {
    30.0
}
fn default_duration() -> f64 {
    180.0
}
fn default_mix() -> HashMap<String, f64> {
    HashMap::from([
        ("like".into(), 6.0),
        ("announce".into(), 3.0),
        ("reply".into(), 1.0),
    ])
}
fn default_concurrency() -> usize {
    512
}

struct SpikeState {
    request: SpikeRequest,
    started_second: u64,
    finished_second: Option<u64>,
}

#[derive(Clone, Copy)]
enum Kind {
    Like,
    Announce,
    Reply,
}

impl Sim {
    fn second(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    fn record(&self, f: impl FnOnce(&mut Bucket)) {
        let second = self.second();
        let mut timeline = self.timeline.lock().unwrap();
        f(timeline.entry(second).or_default());
    }

    fn actor_uri(&self, index: usize) -> String {
        let server = index / self.args.actors_per_server;
        let actor = index % self.args.actors_per_server;
        format!("http://s{server}{SIM_SUFFIX}/users/u{actor}")
    }

    fn key_for(&self, host: &str, name: &str) -> &Key {
        let hash = host
            .bytes()
            .chain(name.bytes())
            .fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize));
        &self.keys[hash % self.keys.len()]
    }

    fn actor_document(&self, host: &str, name: &str) -> Value {
        let id = format!("http://{host}/users/{name}");
        json!({
            "@context": ["https://www.w3.org/ns/activitystreams", "https://w3id.org/security/v1"],
            "id": id,
            "type": "Person",
            "preferredUsername": name,
            "name": format!("{name} of {host}"),
            "summary": "",
            "url": id,
            "inbox": format!("{id}/inbox"),
            "outbox": format!("{id}/outbox"),
            "followers": format!("{id}/followers"),
            "following": format!("{id}/following"),
            "endpoints": { "sharedInbox": format!("http://{host}/inbox") },
            "manuallyApprovesFollowers": false,
            "discoverable": true,
            "published": "2025-01-01T00:00:00Z",
            "publicKey": {
                "id": format!("{id}#main-key"),
                "owner": id,
                "publicKeyPem": self.key_for(host, name).public_pem,
            },
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "eunha_fedisim=info".into()),
        )
        .init();
    let args = Args::parse();

    let generating =
        (0..args.keys).map(|_| eunha::tenants::spawn_blocking(eunha::crypto::generate_rsa_keypair));
    let mut keys = Vec::with_capacity(args.keys);
    for handle in generating {
        let (private_pem, public_pem) = handle.await??;
        keys.push(Key {
            private_pem,
            public_pem,
        });
    }

    let http = reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(1024)
        .timeout(Duration::from_secs(30))
        .build()?;

    let listen = args.listen;
    tracing::info!(
        %listen,
        servers = args.servers,
        actors = args.servers * args.actors_per_server,
        "fake fediverse up"
    );
    let sim = Arc::new(Sim {
        args,
        keys,
        http,
        started: Instant::now(),
        timeline: Mutex::default(),
        blocked_hosts: Mutex::default(),
        notes: Mutex::default(),
        spike: Mutex::default(),
        running: AtomicBool::new(false),
    });

    let app = axum::Router::new().fallback(dispatch).with_state(sim);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Every request lands here: proxied requests carry the simulated host in an
/// absolute URI, the control API and the media sink arrive addressed to us.
async fn dispatch(State(sim): State<Arc<Sim>>, req: Request) -> Response {
    let host = req
        .uri()
        .host()
        .map(str::to_owned)
        .or_else(|| {
            req.headers()
                .get(header::HOST)
                .and_then(|h| h.to_str().ok())
                .map(|h| h.split(':').next().unwrap_or(h).to_owned())
        })
        .unwrap_or_default();

    if req.method() == Method::CONNECT {
        return blocked(&sim, &host);
    }
    if host.ends_with(SIM_SUFFIX) {
        return simulated_server(&sim, &host, req).await;
    }
    if host == "127.0.0.1" || host == "localhost" {
        return control(&sim, req).await;
    }
    blocked(&sim, &host)
}

fn blocked(sim: &Sim, host: &str) -> Response {
    sim.record(|b| b.blocked += 1);
    let mut hosts = sim.blocked_hosts.lock().unwrap();
    let count = hosts.entry(host.to_owned()).or_default();
    if *count == 0 {
        tracing::warn!(host, "refused egress to a host outside the simulation");
    }
    *count += 1;
    (StatusCode::BAD_GATEWAY, "fedisim: egress refused").into_response()
}

fn activity_json(value: Value) -> Response {
    (
        [(header::CONTENT_TYPE, "application/activity+json")],
        Json(value),
    )
        .into_response()
}

fn empty_collection(id: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": id,
        "type": "OrderedCollection",
        "totalItems": 0,
        "orderedItems": [],
    })
}

async fn simulated_server(sim: &Sim, host: &str, req: Request) -> Response {
    let path = req.uri().path().to_owned();
    if req.method() == Method::POST {
        sim.record(|b| b.received_post += 1);
        return StatusCode::ACCEPTED.into_response();
    }
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match segments.as_slice() {
        ["users", name] => {
            sim.record(|b| b.served_actor += 1);
            activity_json(sim.actor_document(host, name))
        }
        ["users", _, "statuses", _] => {
            let id = format!("http://{host}{path}");
            let note = sim.notes.lock().unwrap().get(&id).cloned();
            sim.record(|b| b.served_note += 1);
            match note {
                Some(note) => activity_json(note),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
        ["users", _, "followers" | "following" | "outbox"] | ["users", _, "collections", _] => {
            sim.record(|b| b.served_other += 1);
            activity_json(empty_collection(&format!("http://{host}{path}")))
        }
        _ => {
            sim.record(|b| b.served_other += 1);
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

async fn control(sim: &Arc<Sim>, req: Request) -> Response {
    let path = req.uri().path().to_owned();
    match (req.method().clone(), path.as_str()) {
        (Method::POST, "/__spikes") => {
            let body = match axum::body::to_bytes(req.into_body(), 1 << 20).await {
                Ok(b) => b,
                Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
            };
            let spike: SpikeRequest = match serde_json::from_slice(&body) {
                Ok(s) => s,
                Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
            };
            if sim.running.swap(true, Ordering::SeqCst) {
                return (StatusCode::CONFLICT, "a spike is already running").into_response();
            }
            *sim.spike.lock().unwrap() = Some(SpikeState {
                request: spike.clone(),
                started_second: sim.second(),
                finished_second: None,
            });
            eunha::tenants::spawn(run_spike(sim.clone(), spike));
            StatusCode::ACCEPTED.into_response()
        }
        (Method::GET, "/__stats") => {
            let since: u64 = req
                .uri()
                .query()
                .and_then(|q| q.strip_prefix("since="))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            Json(stats(sim, since)).into_response()
        }
        // Anything else addressed to us is the media store: eunha is started
        // with its S3 endpoint pointing here, so uploads succeed and go nowhere.
        _ => {
            let _ = axum::body::to_bytes(req.into_body(), usize::MAX).await;
            ([(header::ETAG, "\"fedisim\"")], StatusCode::OK).into_response()
        }
    }
}

fn percentile(sorted: &[u32], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[rank] as f64 / 1000.0
}

fn stats(sim: &Sim, since: u64) -> Value {
    let timeline = sim.timeline.lock().unwrap();
    let mut seconds: Vec<&u64> = timeline.keys().filter(|s| **s >= since).collect();
    seconds.sort();
    let mut all_latency = Vec::new();
    let rows: Vec<Value> = seconds
        .into_iter()
        .map(|s| {
            let b = &timeline[s];
            let mut lat = b.latency_us.clone();
            lat.sort_unstable();
            all_latency.extend_from_slice(&lat);
            json!({
                "second": s,
                "sent": b.sent,
                "ok": b.ok,
                "client_error": b.client_error,
                "server_error": b.server_error,
                "transport_error": b.transport_error,
                "shed": b.shed,
                "p50_ms": percentile(&lat, 50.0),
                "p95_ms": percentile(&lat, 95.0),
                "p99_ms": percentile(&lat, 99.0),
                "max_ms": lat.last().map(|v| *v as f64 / 1000.0).unwrap_or(0.0),
                "served_actor": b.served_actor,
                "served_note": b.served_note,
                "served_other": b.served_other,
                "received_post": b.received_post,
                "blocked": b.blocked,
            })
        })
        .collect();
    all_latency.sort_unstable();
    let sum = |f: fn(&Bucket) -> u64| {
        timeline
            .iter()
            .filter(|(s, _)| **s >= since)
            .map(|(_, b)| f(b))
            .sum::<u64>()
    };
    let spike = sim.spike.lock().unwrap();
    json!({
        "now_second": sim.second(),
        "running": sim.running.load(Ordering::SeqCst),
        "spike": spike.as_ref().map(|s| json!({
            "status_uri": s.request.status_uri,
            "peak_rps": s.request.peak_rps,
            "rise_seconds": s.request.rise_seconds,
            "duration_seconds": s.request.duration_seconds,
            "started_second": s.started_second,
            "finished_second": s.finished_second,
        })),
        "totals": {
            "sent": sum(|b| b.sent),
            "ok": sum(|b| b.ok),
            "client_error": sum(|b| b.client_error),
            "server_error": sum(|b| b.server_error),
            "transport_error": sum(|b| b.transport_error),
            "shed": sum(|b| b.shed),
            "served_actor": sum(|b| b.served_actor),
            "served_note": sum(|b| b.served_note),
            "received_post": sum(|b| b.received_post),
            "blocked": sum(|b| b.blocked),
            "p50_ms": percentile(&all_latency, 50.0),
            "p95_ms": percentile(&all_latency, 95.0),
            "p99_ms": percentile(&all_latency, 99.0),
        },
        "blocked_hosts": *sim.blocked_hosts.lock().unwrap(),
        "timeline": rows,
    })
}

/// The arrival curve of a post going viral: a gamma-shaped rise to `peak` at
/// `rise` seconds and a long decay, as boosts reach new audiences and then
/// run out of them.
fn rate_at(t: f64, peak: f64, rise: f64) -> f64 {
    let x = t / rise;
    peak * x * (1.0 - x).exp()
}

async fn run_spike(sim: Arc<Sim>, spike: SpikeRequest) {
    let population = sim.args.servers * sim.args.actors_per_server;
    // Each actor likes, boosts and replies at most once, as people do.
    let mut queues: Vec<(Kind, f64, Vec<usize>)> = [
        (Kind::Like, "like"),
        (Kind::Announce, "announce"),
        (Kind::Reply, "reply"),
    ]
    .into_iter()
    .filter_map(|(kind, name)| {
        let weight = spike.mix.get(name).copied().unwrap_or(0.0);
        (weight > 0.0).then(|| {
            let mut actors: Vec<usize> = (0..population).collect();
            actors.shuffle(&mut rand::rng());
            (kind, weight, actors)
        })
    })
    .collect();

    let permits = Arc::new(tokio::sync::Semaphore::new(spike.concurrency));
    let spike = Arc::new(spike);
    let start = Instant::now();
    let tick = Duration::from_millis(10);
    let mut owed = 0.0;
    let mut serial: u64 = 0;
    tracing::info!(
        peak = spike.peak_rps,
        rise = spike.rise_seconds,
        "spike started"
    );

    while start.elapsed().as_secs_f64() < spike.duration_seconds {
        let t = start.elapsed().as_secs_f64();
        owed += rate_at(t, spike.peak_rps, spike.rise_seconds) * tick.as_secs_f64();
        while owed >= 1.0 {
            owed -= 1.0;
            let total: f64 = queues.iter().filter(|q| !q.2.is_empty()).map(|q| q.1).sum();
            if total <= 0.0 {
                break;
            }
            let mut pick = rand::rng().random_range(0.0..total);
            let Some(queue) = queues.iter_mut().filter(|q| !q.2.is_empty()).find(|q| {
                pick -= q.1;
                pick < 0.0
            }) else {
                continue;
            };
            let kind = queue.0;
            let actor = queue.2.pop().expect("filtered to non-empty");
            serial += 1;
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                sim.record(|b| b.shed += 1);
                continue;
            };
            let sim = sim.clone();
            let spike = spike.clone();
            eunha::tenants::spawn(async move {
                send(&sim, &spike, kind, actor, serial).await;
                drop(permit);
            });
        }
        tokio::time::sleep(tick).await;
    }

    // Let what is in flight finish before calling the spike over.
    let _ = permits.acquire_many(spike.concurrency as u32).await;
    if let Some(state) = sim.spike.lock().unwrap().as_mut() {
        state.finished_second = Some(sim.second());
    }
    sim.running.store(false, Ordering::SeqCst);
    tracing::info!(activities = serial, "spike finished");
}

async fn send(sim: &Sim, spike: &SpikeRequest, kind: Kind, actor_index: usize, serial: u64) {
    let actor = sim.actor_uri(actor_index);
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let activity = match kind {
        Kind::Like => json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{actor}#likes/{serial}"),
            "type": "Like",
            "actor": actor,
            "object": spike.status_uri,
        }),
        Kind::Announce => json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{actor}/statuses/{serial}/activity"),
            "type": "Announce",
            "actor": actor,
            "published": now,
            "to": [AS_PUBLIC],
            "cc": [spike.author_uri, format!("{actor}/followers")],
            "object": spike.status_uri,
        }),
        Kind::Reply => {
            let id = format!("{actor}/statuses/{serial}");
            let note = json!({
                "@context": "https://www.w3.org/ns/activitystreams",
                "id": id,
                "type": "Note",
                "attributedTo": actor,
                "inReplyTo": spike.status_uri,
                "published": now,
                "url": id,
                "to": [AS_PUBLIC],
                "cc": [spike.author_uri, format!("{actor}/followers")],
                "content": format!("<p>This is reply {serial}, and it will not be the last.</p>"),
                "tag": [{ "type": "Mention", "href": spike.author_uri }],
            });
            sim.notes.lock().unwrap().insert(id.clone(), note.clone());
            json!({
                "@context": "https://www.w3.org/ns/activitystreams",
                "id": format!("{id}/activity"),
                "type": "Create",
                "actor": actor,
                "published": now,
                "to": [AS_PUBLIC],
                "cc": [spike.author_uri, format!("{actor}/followers")],
                "object": note,
            })
        }
    };

    let url = url::Url::parse(&actor).expect("actor URIs are built well-formed");
    let host = url.host_str().unwrap_or_default();
    let name = url.path().rsplit('/').next().unwrap_or_default();
    let key = sim.key_for(host, name);
    let key_id = format!("{actor}#main-key");
    let body = serde_json::to_vec(&activity).expect("activity serializes");
    let content_type = "application/activity+json";

    // Signed as a request to the instance's own domain, then sent to wherever
    // it is actually listening with that domain as its `Host`.
    let signed = match feder_runtime::signature::sign_request(
        "post",
        &format!("https://{}/inbox", sim.args.domain),
        &body,
        &key_id,
        &key.private_pem,
        &[("content-type", content_type)],
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "signing failed");
            sim.record(|b| b.transport_error += 1);
            return;
        }
    };

    let began = Instant::now();
    let result = sim
        .http
        .post(format!("{}/inbox", sim.args.eunha.trim_end_matches('/')))
        .header(header::HOST, &sim.args.domain)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT, content_type)
        .header(header::DATE, signed.date)
        .header("Digest", signed.digest)
        .header("Signature", signed.signature)
        .body(Bytes::from(body))
        .send()
        .await;
    let elapsed = began.elapsed().as_micros().min(u32::MAX as u128) as u32;

    sim.record(|b| {
        b.sent += 1;
        b.latency_us.push(elapsed);
        match &result {
            Ok(r) if r.status().is_success() => b.ok += 1,
            Ok(r) if r.status().is_client_error() => b.client_error += 1,
            Ok(_) => b.server_error += 1,
            Err(_) => b.transport_error += 1,
        }
    });
    if let Ok(r) = result {
        if !r.status().is_success() && r.status() != StatusCode::TOO_MANY_REQUESTS {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            tracing::debug!(%status, body = %text.chars().take(200).collect::<String>(), "inbox refused");
        }
    }
}
