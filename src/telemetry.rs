//! Opt-in process metrics, served on a separate loopback listener.
use crate::{state::AppState, tenants::Tenants};
use axum::{
    extract::{MatchedPath, Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use metrics_util::MetricKindMask;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn validate_address(address: &str) -> anyhow::Result<SocketAddr> {
    let address: SocketAddr = address.parse()?;
    anyhow::ensure!(
        address.ip().is_loopback(),
        "metrics listener must use a loopback address"
    );
    Ok(address)
}

fn recorder() -> metrics_exporter_prometheus::PrometheusRecorder {
    PrometheusBuilder::new()
        .set_buckets(&[
            0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
        ])
        .expect("fixed histogram buckets are valid")
        .idle_timeout(MetricKindMask::ALL, Some(Duration::from_secs(900)))
        .build_recorder()
}

pub async fn start(
    address: Option<&str>,
    tenants: Arc<Tenants>,
) -> anyhow::Result<Option<(tokio::net::TcpListener, Router)>> {
    let Some(address) = address else {
        return Ok(None);
    };
    let listener = tokio::net::TcpListener::bind(validate_address(address)?).await?;
    let recorder = recorder();
    let handle = recorder.handle();
    metrics::set_global_recorder(recorder)?;
    ENABLED.store(true, Ordering::Relaxed);
    metrics::describe_counter!(
        "eunha_http_requests_total",
        "Responses produced, excluding body streaming time."
    );
    metrics::describe_counter!(
        "eunha_http_aborted_requests_total",
        "Requests dropped before producing a response."
    );
    metrics::describe_counter!(
        "eunha_http_capacity_rejections_total",
        "Requests refused by a tenant's admission limit."
    );
    metrics::describe_histogram!(
        "eunha_http_response_duration_seconds",
        "Time until response headers, including authentication."
    );
    metrics::describe_gauge!(
        "eunha_http_in_flight",
        "Requests awaiting response headers; not streaming connections."
    );
    metrics::describe_gauge!(
        "eunha_database_pool_connections",
        "Open application pool connections, including idle connections."
    );
    metrics::describe_gauge!(
        "eunha_database_pool_idle_connections",
        "Idle application pool connections."
    );
    let upkeep = handle.clone();
    crate::tenants::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            let states = tenants.states().await;
            metrics::gauge!("eunha_serving_tenants").set(states.len() as f64);
            for state in states {
                let tenant = state.instance.domain.clone();
                metrics::gauge!("eunha_database_pool_connections", "tenant" => tenant.clone())
                    .set(state.db.size() as f64);
                metrics::gauge!("eunha_database_pool_idle_connections", "tenant" => tenant.clone())
                    .set(state.db.num_idle() as f64);
                metrics::gauge!("eunha_http_in_flight", "tenant" => tenant)
                    .set(state.metrics_in_flight.load(Ordering::Relaxed) as f64);
            }
            upkeep.run_upkeep();
        }
    });
    tracing::info!(address = %listener.local_addr()?, "private metrics listener enabled");
    Ok(Some((listener, private_router(handle))))
}

fn private_router(handle: PrometheusHandle) -> Router {
    Router::new()
        .route("/metrics", get(export))
        .with_state(handle)
}

async fn export(State(handle): State<PrometheusHandle>, headers: HeaderMap) -> Response {
    // Reject browser DNS-rebinding requests even when the socket is local.
    let local_host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .and_then(|host| host.parse::<axum::http::uri::Authority>().ok())
        .is_some_and(|host| {
            host.host() == "localhost"
                || host
                    .host()
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
    if !local_host {
        return StatusCode::FORBIDDEN.into_response();
    }
    (
        [
            (
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        handle.render(),
    )
        .into_response()
}

fn method_label(method: &Method) -> &'static str {
    match method.as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        "CONNECT" => "CONNECT",
        "TRACE" => "TRACE",
        _ => "OTHER",
    }
}

struct InFlight {
    tenant: String,
    route: String,
    method: &'static str,
    count: Arc<AtomicU64>,
    finished: bool,
}
impl InFlight {
    fn new(tenant: String, route: String, method: &'static str, count: Arc<AtomicU64>) -> Self {
        let active = count.fetch_add(1, Ordering::Relaxed) + 1;
        metrics::gauge!("eunha_http_in_flight", "tenant" => tenant.clone()).set(active as f64);
        Self {
            tenant,
            route,
            method,
            count,
            finished: false,
        }
    }
}
impl Drop for InFlight {
    fn drop(&mut self) {
        let active = self.count.fetch_sub(1, Ordering::Relaxed) - 1;
        metrics::gauge!("eunha_http_in_flight", "tenant" => self.tenant.clone()).set(active as f64);
        if !self.finished {
            metrics::counter!("eunha_http_aborted_requests_total", "tenant" => self.tenant.clone(), "route" => self.route.clone(), "method" => self.method).increment(1);
        }
    }
}

pub async fn observe(state: AppState, req: Request, next: Next) -> Response {
    if !ENABLED.load(Ordering::Relaxed) {
        return next.run(req).await;
    }
    observe_for(
        state.instance.domain.clone(),
        state.metrics_in_flight,
        req,
        next,
    )
    .await
}

async fn observe_for(tenant: String, count: Arc<AtomicU64>, req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", MatchedPath::as_str)
        .to_string();
    let method = method_label(req.method());
    let mut guard = InFlight::new(tenant.clone(), route.clone(), method, count);
    let started = Instant::now();
    let response = next.run(req).await;
    let status_class = match response.status().as_u16() / 100 {
        1 => "1xx",
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        _ => "5xx",
    };
    metrics::counter!("eunha_http_requests_total", "tenant" => tenant.clone(), "route" => route.clone(), "method" => method, "status_class" => status_class).increment(1);
    metrics::histogram!("eunha_http_response_duration_seconds", "tenant" => tenant, "route" => route, "method" => method).record(started.elapsed().as_secs_f64());
    guard.finished = true;
    response
}

pub fn capacity_rejection(domain: &str) {
    if ENABLED.load(Ordering::Relaxed) {
        metrics::counter!("eunha_http_capacity_rejections_total", "tenant" => domain.to_string())
            .increment(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use tower::ServiceExt;

    #[test]
    fn metrics_listener_is_private_and_labels_are_bounded() {
        assert!(validate_address("127.0.0.1:9394").is_ok());
        assert!(validate_address("[::1]:9394").is_ok());
        assert!(validate_address("0.0.0.0:9394").is_err());
        assert!(validate_address("100.113.148.66:9394").is_err());
        assert_eq!(
            method_label(&Method::from_bytes(b"arbitrary-secret").unwrap()),
            "OTHER"
        );
        let recorder = recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let count = Arc::new(AtomicU64::new(0));
            let guard = InFlight::new(
                "garden.example".into(),
                "unmatched".into(),
                "GET",
                count.clone(),
            );
            assert_eq!(count.load(Ordering::Relaxed), 1);
            drop(guard);
            assert_eq!(count.load(Ordering::Relaxed), 0);
        });
        let output = handle.render();
        assert!(output.contains("eunha_http_aborted_requests_total"));
        assert!(output.contains("tenant=\"garden.example\""));
    }

    #[test]
    fn requests_use_templates_and_export_only_on_private_router() {
        let recorder = recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let count = Arc::new(AtomicU64::new(0));
                    let app = Router::new()
                        .route("/things/{id}", get(|| async { StatusCode::OK }))
                        .layer(axum::middleware::from_fn(move |req, next| {
                            observe_for("garden.example".into(), count.clone(), req, next)
                        }));
                    let request = Request::builder()
                        .uri("/things/private-account?access_token=private-token")
                        .header(header::HOST, "arbitrary-host.example")
                        .body(Body::empty())
                        .unwrap();
                    assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
                    let private = private_router(handle.clone());
                    let request = |path, host| {
                        Request::builder()
                            .uri(path)
                            .header(header::HOST, host)
                            .body(Body::empty())
                            .unwrap()
                    };
                    assert_eq!(
                        private
                            .clone()
                            .oneshot(request("/metrics", "public.example"))
                            .await
                            .unwrap()
                            .status(),
                        StatusCode::FORBIDDEN
                    );
                    assert_eq!(
                        private
                            .clone()
                            .oneshot(request("/anything", "localhost"))
                            .await
                            .unwrap()
                            .status(),
                        StatusCode::NOT_FOUND
                    );
                    let response = private
                        .oneshot(request("/metrics", "127.0.0.1:9394"))
                        .await
                        .unwrap();
                    assert_eq!(response.status(), StatusCode::OK);
                    assert!(response.headers()[header::CONTENT_TYPE]
                        .to_str()
                        .unwrap()
                        .contains("version=0.0.4"));
                    let body = to_bytes(response.into_body(), 1_000_000).await.unwrap();
                    let output = String::from_utf8(body.to_vec()).unwrap();
                    assert!(output.contains("route=\"/things/{id}\""));
                    assert!(output.contains("status_class=\"2xx\""));
                    assert!(output.contains("eunha_http_response_duration_seconds_bucket"));
                    assert!(!output.contains("private-account"));
                    assert!(!output.contains("private-token"));
                    assert!(!output.contains("arbitrary-host"));
                });
        });
    }
}
