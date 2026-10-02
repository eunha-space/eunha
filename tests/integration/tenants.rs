//! Several instances served by one process, dispatched by `Host`.
//!
//! Each `TestContext` brings up an instance of its own — database, seeded
//! accounts, random tokens — so two of them behind one listener are two tenants
//! that share nothing but the process, which is exactly what must not leak.

use crate::helpers::{ApiClient, TestContext};
use futures::StreamExt as _;
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest as _, Message};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Serve these instances' states from one listener, as one process would.
async fn serve(states: Vec<eunha::state::AppState>) -> String {
    let tenants = eunha::tenants::Tenants::from_states("127.0.0.1:0", states)
        .expect("tenants with distinct domains form a registry");
    serve_tenants(Arc::new(tenants)).await
}

/// Serve a registry of tenants from one listener, as `eunha` does.
async fn serve_tenants(tenants: Arc<eunha::tenants::Tenants>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let router = tenants.router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    base_url
}

/// Two instances in one process each answer only for their own host: their
/// own instance metadata, their own statuses, their own tokens.
#[tokio::test]
async fn test_tenants_in_one_process_are_dispatched_by_host() {
    let a = TestContext::new("tenant-a").await;
    let b = TestContext::new("tenant-b").await;
    let base_url = serve(vec![a.state.clone(), b.state.clone()]).await;
    let to_a = ApiClient::new(&base_url, &a.domain);
    let to_b = ApiClient::new(&base_url, &b.domain);

    for (client, domain) in [(&to_a, &a.domain), (&to_b, &b.domain)] {
        let instance: Value = client
            .get("/api/v2/instance", None)
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(
            instance["domain"].as_str(),
            Some(domain.as_str()),
            "each host answers as its own instance",
        );
    }

    let status: Value = to_a
        .post_json(
            "/api/v1/statuses",
            Some(&a.alice_token),
            &json!({ "status": "only on A", "visibility": "public" }),
        )
        .await
        .json()
        .await
        .unwrap();
    let id = status["id"].as_str().expect("the status was created");
    assert!(
        status["uri"]
            .as_str()
            .is_some_and(|uri| uri.starts_with(&format!("https://{}/", a.domain))),
        "A's status is A's: {}",
        status["uri"],
    );
    assert_eq!(
        to_a.get(&format!("/api/v1/statuses/{id}"), None)
            .await
            .status(),
        StatusCode::OK,
        "A serves its own status",
    );
    assert_eq!(
        to_b.get(&format!("/api/v1/statuses/{id}"), None)
            .await
            .status(),
        StatusCode::NOT_FOUND,
        "B has never heard of A's status",
    );

    assert_eq!(
        to_a.get("/api/v1/accounts/verify_credentials", Some(&a.alice_token))
            .await
            .status(),
        StatusCode::OK,
        "A's token works on A",
    );
    assert_eq!(
        to_b.get("/api/v1/accounts/verify_credentials", Some(&a.alice_token))
            .await
            .status(),
        StatusCode::UNAUTHORIZED,
        "A's token means nothing to B",
    );

    let stranger = ApiClient::new(&base_url, "stranger.invalid");
    assert_eq!(
        stranger.get("/api/v2/instance", None).await.status(),
        StatusCode::MISDIRECTED_REQUEST,
        "a host no tenant serves is refused, not handed to one of them",
    );
}

/// A lone instance answers whatever host it is asked by, as eunha always has:
/// health checks and local clients do not send the instance's domain.
#[tokio::test]
async fn test_a_lone_tenant_answers_any_host() {
    let a = TestContext::new("tenant-lone").await;
    let base_url = serve(vec![a.state.clone()]).await;
    let other = ApiClient::new(&base_url, "localhost");
    assert_eq!(
        other.get("/api/v2/instance", None).await.status(),
        StatusCode::OK
    );
}

/// Open a streaming connection to whichever tenant `host` names, through the
/// shared listener.
async fn open_stream(base_url: &str, host: &str, stream: &str, token: &str) -> Ws {
    let url = format!(
        "{}/api/v1/streaming?stream={stream}&access_token={token}",
        base_url.replace("http://", "ws://")
    );
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert("host", host.parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("a WebSocket upgrade survives dispatch");
    ws
}

/// The next event on `ws`, or `None` if nothing arrives within `wait`.
async fn next_event(ws: &mut Ws, wait: Duration) -> Option<Value> {
    loop {
        match tokio::time::timeout(wait, ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => return serde_json::from_str(&text).ok(),
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            _ => return None,
        }
    }
}

/// Each tenant's streaming events reach only its own streams. The streaming bus
/// lives in the process, which makes it the first place a shared process could
/// leak — and the connection has to survive being upgraded through the
/// dispatcher at all.
#[tokio::test]
async fn test_streams_do_not_cross_tenants() {
    let a = TestContext::new("tenant-stream-a").await;
    let b = TestContext::new("tenant-stream-b").await;
    let base_url = serve(vec![a.state.clone(), b.state.clone()]).await;
    let mut on_a = open_stream(&base_url, &a.domain, "public", &a.bob_token).await;
    let mut on_b = open_stream(&base_url, &b.domain, "public", &b.bob_token).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    ApiClient::new(&base_url, &a.domain)
        .post_status(&a.alice_token, "streamed on A", "public")
        .await;

    let event = next_event(&mut on_a, Duration::from_secs(3))
        .await
        .expect("A's stream hears A's status");
    assert_eq!(event["event"], "update");
    assert!(
        next_event(&mut on_b, Duration::from_secs(1))
            .await
            .is_none(),
        "B's stream must hear nothing of A's status",
    );
}

/// Seed a remote actor, with its public key, into one tenant's database.
async fn seed_remote_actor(db: &sqlx::PgPool, uri: &str, public_key: &str) {
    sqlx::query(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'carol', 'remote-tenant.invalid', 'carol', '', $2, $2, $3,
                   $2 || '/inbox', $2 || '/outbox', now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(uri)
    .bind(public_key)
    .execute(db)
    .await
    .unwrap();
}

async fn followers_of_alice(ctx: &TestContext) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM follows WHERE target_account_id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap()
}

/// An activity delivered to one tenant's host lands in that tenant's database
/// and nowhere else — including one that names another tenant's account.
#[tokio::test]
async fn test_inbox_deliveries_stay_with_the_tenant_addressed() {
    let a = TestContext::new("tenant-inbox-a").await;
    let b = TestContext::new("tenant-inbox-b").await;
    let base_url = serve(vec![a.state.clone(), b.state.clone()]).await;
    let (private_key, public_key) = eunha::crypto::generate_rsa_keypair().unwrap();
    let carol = "https://remote-tenant.invalid/users/carol";
    seed_remote_actor(&a.db, carol, &public_key).await;
    seed_remote_actor(&b.db, carol, &public_key).await;
    let to_b = ApiClient::new(&base_url, &b.domain);
    let key_id = format!("{carol}#main-key");

    let follow = |id: &str, domain: &str| {
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("https://remote-tenant.invalid/activities/{id}"),
            "type": "Follow",
            "actor": carol,
            "object": format!("https://{domain}/users/alice"),
        })
    };

    let response = to_b
        .post_signed(
            "/inbox",
            &follow("follow-b", &b.domain),
            &key_id,
            &private_key,
        )
        .await;
    assert!(
        response.status().is_success(),
        "B accepts a follow of its own alice: {}",
        response.status()
    );
    assert_eq!(
        followers_of_alice(&b).await,
        1,
        "B's alice gained a follower"
    );
    assert_eq!(followers_of_alice(&a).await, 0, "A's alice did not");

    // Names A's alice, but is delivered to B.
    to_b.post_signed(
        "/inbox",
        &follow("follow-a", &a.domain),
        &key_id,
        &private_key,
    )
    .await;
    assert_eq!(
        followers_of_alice(&a).await,
        0,
        "B cannot act on A's accounts"
    );
    assert_eq!(
        followers_of_alice(&b).await,
        1,
        "nor may B take A's alice for its own"
    );
}

/// Discovery answers only for the tenant asked: WebFinger in both its `acct:`
/// and URL forms, and the actor document.
#[tokio::test]
async fn test_discovery_answers_only_for_the_tenant_asked() {
    let a = TestContext::new("tenant-webfinger-a").await;
    let b = TestContext::new("tenant-webfinger-b").await;
    let base_url = serve(vec![a.state.clone(), b.state.clone()]).await;
    let to_a = ApiClient::new(&base_url, &a.domain);
    let to_b = ApiClient::new(&base_url, &b.domain);

    for resource in [
        format!("acct:alice@{}", a.domain),
        format!("https://{}/users/alice", a.domain),
    ] {
        let path = format!(
            "/.well-known/webfinger?resource={}",
            urlencoding::encode(&resource)
        );
        let found: Value = to_a.get(&path, None).await.json().await.unwrap();
        assert_eq!(
            found["subject"].as_str(),
            Some(format!("acct:alice@{}", a.domain).as_str()),
            "A answers for {resource}",
        );
        assert_eq!(
            to_b.get(&path, None).await.status(),
            StatusCode::NOT_FOUND,
            "B must not answer for {resource}",
        );
    }

    for (client, domain) in [(&to_a, &a.domain), (&to_b, &b.domain)] {
        let actor: Value = client
            .ap_get("/users/alice", None)
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(
            actor["id"].as_str(),
            Some(format!("https://{domain}/users/alice").as_str()),
            "each tenant serves its own alice",
        );
    }
}

/// Tenants in one process share none of the in-process state that requests
/// and background tasks read: their queue wake-ups and their URLs are their own.
#[tokio::test]
async fn test_tenants_share_no_in_process_state() {
    let a = TestContext::new("tenant-state-a").await;
    let b = TestContext::new("tenant-state-b").await;
    assert!(
        !Arc::ptr_eq(&a.state.queues, &b.state.queues),
        "queue wake-ups are per tenant"
    );
    assert!(
        !Arc::ptr_eq(&a.state.urls, &b.state.urls),
        "URLs are per tenant"
    );
    assert_eq!(a.state.urls.local_domain, a.domain);
    assert_eq!(b.state.urls.local_domain, b.domain);
}

/// Log output kept in memory, to read back what was logged and in which span.
#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn line_with(&self, needle: &str) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("nothing logged {needle:?}"))
            .to_string()
    }
}

/// With two tenants behind one listener, what each one's request logs names
/// that tenant and not its neighbour — the difference between one process's
/// log and a log that cannot say whose trouble a line is.
#[tokio::test]
async fn test_what_a_request_logs_names_its_tenant() {
    let logs = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer({
            let logs = logs.clone();
            move || logs.clone()
        })
        .finish();
    let _default = tracing::subscriber::set_default(subscriber);

    let a = TestContext::new("tenant-logs-a").await;
    let b = TestContext::new("tenant-logs-b").await;
    let base_url = serve(vec![a.state.clone(), b.state.clone()]).await;
    // Whether anything wants an event is cached per callsite for the whole
    // process. The other tests running alongside log with no subscriber at
    // all, and one of them registering the failure log's callsite while this
    // subscriber was being registered cached "nobody" for it — so this test
    // passed alone and, in the full suite, captured nothing. Ask again now
    // that this subscriber is certainly there.
    tracing::callsite::rebuild_interest_cache();
    for ctx in [&a, &b] {
        let response = ApiClient::new(&base_url, &ctx.domain)
            .get(&format!("/api/v1/nowhere-{}", ctx.domain), None)
            .await;
        assert!(response.status().is_client_error(), "{}", response.status());
    }

    for (ctx, other) in [(&a, &b), (&b, &a)] {
        let line = logs.line_with(&format!("path=/api/v1/nowhere-{}", ctx.domain));
        assert!(
            line.contains(&format!("tenant{{domain={}}}", ctx.domain)),
            "{line}"
        );
        assert!(
            !line.contains(&format!("domain={}", other.domain)),
            "{line}"
        );
    }
}

/// A failed request is logged, but never with what it carried: a refused
/// password grant, sign-in or sign-up holds the credentials that were refused.
#[tokio::test]
async fn test_failed_requests_do_not_log_their_credentials() {
    let logs = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer({
            let logs = logs.clone();
            move || logs.clone()
        })
        .finish();
    let _default = tracing::subscriber::set_default(subscriber);

    let ctx = TestContext::new("tenant-logs-secrets").await;
    let base_url = serve(vec![ctx.state.clone()]).await;
    // See `test_what_a_request_logs_names_its_tenant`.
    tracing::callsite::rebuild_interest_cache();
    let client = ApiClient::new(&base_url, &ctx.domain);

    let json_password = "refused-json-password-8c1f";
    let refused = client
        .post_json(
            "/oauth/token",
            None,
            &json!({
                "grant_type": "password",
                "client_id": "no-such-client",
                "client_secret": "refused-client-secret-2b7d",
                "username": "alice@test.invalid",
                "password": json_password,
            }),
        )
        .await;
    assert!(refused.status().is_client_error(), "{}", refused.status());

    let form_password = "refused-form-password-51e0";
    let refused = client
        .post_form(
            "/api/v1/accounts",
            None,
            &[
                ("username", "not a valid username"),
                ("email", "someone@example.com"),
                ("password", form_password),
                ("agreement", "true"),
            ],
        )
        .await;
    assert!(refused.status().is_client_error(), "{}", refused.status());

    let line = logs.line_with("path=/oauth/token");
    assert!(line.contains("status=4"), "{line}");
    logs.line_with("path=/api/v1/accounts");
    let logged = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    for secret in [json_password, "refused-client-secret-2b7d", form_password] {
        assert!(!logged.contains(secret), "{secret} was logged:\n{logged}");
    }
}

/// A process asks the real PostgreSQL server how many connections it accepts,
/// and refuses to start with a pool that could open more, rather than failing
/// some request later on. Were the server not asked, this pool would start.
#[tokio::test]
async fn test_a_process_refuses_pools_its_database_server_cannot_hold() {
    let a = TestContext::new("tenant-pools").await;
    let mut config = (*a.state.config).clone();
    config.database_pool.max_connections = 100_000;
    let started = eunha::tenants::start(vec![eunha::tenants::TenantConfig {
        source: "pools.toml".into(),
        config,
    }])
    .await;
    let error = match started {
        Ok(_) => panic!("a pool of 100,000 connections was admitted"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        error.contains("pools.toml")
            && error.contains("100000 database connections")
            && error.contains("database_pool.max_connections"),
        "{error}"
    );
}

/// A tenant's configuration as a tenants directory would hold it.
fn tenant_config(ctx: &TestContext) -> eunha::tenants::TenantConfig {
    eunha::tenants::TenantConfig {
        source: format!("{}.toml", ctx.domain),
        config: (*ctx.state.config).clone(),
    }
}

/// What `/api/v2/instance` on `host` answers: its status, and its title.
async fn instance_title(base_url: &str, host: &str) -> (StatusCode, Option<String>) {
    let response = ApiClient::new(base_url, host)
        .get("/api/v2/instance", None)
        .await;
    let status = response.status();
    let title = response
        .json::<Value>()
        .await
        .ok()
        .and_then(|instance| instance["title"].as_str().map(str::to_string));
    (status, title)
}

/// Whether `/api/v2/instance` on `host` says it is in limited federation mode.
async fn limited_federation(base_url: &str, host: &str) -> (StatusCode, Option<bool>) {
    let response = ApiClient::new(base_url, host)
        .get("/api/v2/instance", None)
        .await;
    let status = response.status();
    let limited = response
        .json::<Value>()
        .await
        .ok()
        .and_then(|instance| instance["configuration"]["limited_federation"].as_bool());
    (status, limited)
}

/// Tenants are added, changed and removed while the process goes on serving the
/// rest: a new one answers, a changed one comes back with its new
/// configuration, and a removed one's host is refused and its background work
/// stops by itself. A reload that would move the listener is refused and
/// changes nothing.
#[tokio::test]
async fn test_tenants_come_and_go_without_a_restart() {
    let a = TestContext::new("reload-a").await;
    let b = TestContext::new("reload-b").await;
    let c = TestContext::new("reload-c").await;
    let tenants = Arc::new(
        eunha::tenants::start(vec![tenant_config(&a), tenant_config(&b)])
            .await
            .expect("two tenants start"),
    );
    let base_url = serve_tenants(tenants.clone()).await;
    assert_eq!(
        instance_title(&base_url, &c.domain).await.0,
        StatusCode::MISDIRECTED_REQUEST
    );

    let reloaded = tenants
        .reload(vec![
            tenant_config(&a),
            tenant_config(&b),
            tenant_config(&c),
        ])
        .await
        .expect("adding a tenant reloads");
    assert!(
        reloaded.unavailable.is_empty(),
        "a tenant did not start: {:?}",
        reloaded.unavailable
    );
    assert_eq!(reloaded.started, std::slice::from_ref(&c.domain));
    assert_eq!(reloaded.kept.len(), 2);
    assert_eq!(
        instance_title(&base_url, &c.domain).await.0,
        StatusCode::OK,
        "the added tenant answers"
    );

    let a_running = tenants
        .states()
        .await
        .into_iter()
        .find(|state| state.instance.domain == a.domain)
        .expect("a is running");
    let began = std::time::Instant::now();
    let reloaded = tenants
        .reload(vec![tenant_config(&b), tenant_config(&c)])
        .await
        .expect("removing a tenant reloads");
    assert_eq!(reloaded.stopped, std::slice::from_ref(&a.domain));
    assert!(
        a_running.stop.is_cancelled(),
        "the removed tenant is told to stop"
    );
    assert!(
        began.elapsed() < eunha::background::STOP_GRACE,
        "its background tasks stopped by themselves, not when the grace period ran out ({:?})",
        began.elapsed()
    );
    assert_eq!(
        instance_title(&base_url, &a.domain).await.0,
        StatusCode::MISDIRECTED_REQUEST,
        "the removed tenant's host is refused"
    );
    assert_eq!(
        instance_title(&base_url, &b.domain).await.0,
        StatusCode::OK,
        "its neighbours serve on"
    );

    let renamed = || {
        let mut tenant = tenant_config(&c);
        tenant.config.instance.limited_federation_mode = true;
        tenant
    };
    let reloaded = tenants
        .reload(vec![tenant_config(&b), renamed()])
        .await
        .expect("changing a tenant reloads");
    assert_eq!(reloaded.restarted, std::slice::from_ref(&c.domain));
    assert_eq!(
        limited_federation(&base_url, &c.domain).await,
        (StatusCode::OK, Some(true)),
        "the changed tenant comes back with its new configuration"
    );

    let mut moved = vec![tenant_config(&b), renamed()];
    for tenant in &mut moved {
        tenant.config.bind_address = "127.0.0.1:1".into();
    }
    let error = match tenants.reload(moved).await {
        Ok(_) => panic!("a reload moving the listener was accepted"),
        Err(e) => format!("{e:#}"),
    };
    assert!(error.contains("bind_address"), "{error}");
    assert_eq!(
        limited_federation(&base_url, &c.domain).await,
        (StatusCode::OK, Some(true)),
        "a refused reload leaves the tenants as they were"
    );
}
