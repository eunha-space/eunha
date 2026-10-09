//! `eunha domains`, which is `tootctl domains`: purging what is known of
//! other servers, and crawling them for their statistics.

use serde_json::{json, Value};

use eunha::tootctl::{domains, Recorder};

use crate::helpers::TestContext;

async fn remote(ctx: &TestContext, username: &str, domain: &str, uri_host: &str) -> i64 {
    sqlx::query_scalar(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri,
                                 protocol, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', 'https://' || $4 || '/@' || $2,
                   'https://' || $4 || '/users/' || $2, 1, now(), now())
           RETURNING id"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(username)
    .bind(domain)
    .bind(uri_host)
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

fn options() -> domains::PurgeOptions {
    domains::PurgeOptions {
        concurrency: 2,
        verbose: false,
        dry_run: false,
        limited_federation_mode: false,
        by_uri: false,
        include_subdomains: false,
        purge_domain_blocks: false,
    }
}

async fn known(ctx: &TestContext) -> Vec<String> {
    sqlx::query_scalar("SELECT username FROM accounts WHERE domain IS NOT NULL ORDER BY username")
        .fetch_all(&ctx.db)
        .await
        .unwrap()
}

/// What `purge` removes, by each of its ways of naming domains.
#[tokio::test]
async fn test_purge_removes_a_domains_accounts_without_a_trace() {
    let ctx = TestContext::new("cli-purge").await;
    remote(&ctx, "ann", "a.invalid", "a.invalid").await;
    remote(&ctx, "sam", "sub.a.invalid", "sub.a.invalid").await;
    remote(&ctx, "ben", "b.invalid", "b.invalid").await;
    remote(&ctx, "hal", "handle.invalid", "host.b.invalid").await;
    remote(&ctx, "cal", "c.invalid", "c.invalid").await;
    sqlx::query(
        "INSERT INTO custom_emojis (shortcode, domain, uri, created_at, updated_at)
         VALUES ('a', 'a.invalid', 'https://a.invalid/emojis/1', now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    for domain in ["a.invalid", "sub.a.invalid"] {
        sqlx::query(
            "INSERT INTO domain_blocks (domain, severity, created_at, updated_at)
             VALUES ($1, 0, now(), now())",
        )
        .bind(domain)
        .execute(&ctx.db)
        .await
        .unwrap();
    }

    let console = Recorder::default();
    for refused in [
        (vec![], options(), "No domain(s) given"),
        (
            vec!["a.invalid".to_owned()],
            domains::PurgeOptions {
                limited_federation_mode: true,
                ..options()
            },
            "DOMAIN parameter not supported with --limited-federation-mode",
        ),
    ] {
        let error = domains::purge(&ctx.state, &console, &refused.0, &refused.1)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), refused.2);
    }

    let console = Recorder::default();
    domains::purge(
        &ctx.state,
        &console,
        &["A.invalid".to_owned()],
        &domains::PurgeOptions {
            dry_run: true,
            include_subdomains: true,
            purge_domain_blocks: true,
            ..options()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        console.lines(),
        [
            "Removed 2 accounts (DRY RUN)",
            "Removed 2 domain blocks (DRY RUN)",
            "Removed 1 custom emojis (DRY RUN)"
        ]
    );
    assert_eq!(known(&ctx).await, ["ann", "ben", "cal", "hal", "sam"]);

    let console = Recorder::default();
    domains::purge(
        &ctx.state,
        &console,
        &["a.invalid".to_owned(), "*.a.invalid".to_owned()],
        &domains::PurgeOptions {
            purge_domain_blocks: true,
            ..options()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        console.lines(),
        [
            "Removed 2 accounts",
            "Removed 2 domain blocks",
            "Removed 1 custom emojis"
        ]
    );
    assert_eq!(known(&ctx).await, ["ben", "cal", "hal"]);

    // By the host of the actor id rather than the handle's domain.
    let console = Recorder::default();
    domains::purge(
        &ctx.state,
        &console,
        &["b.invalid".to_owned()],
        &domains::PurgeOptions {
            by_uri: true,
            include_subdomains: true,
            ..options()
        },
    )
    .await
    .unwrap();
    assert_eq!(known(&ctx).await, ["cal"]);

    // Everything not explicitly allowed.
    sqlx::query(
        "INSERT INTO domain_allows (domain, created_at, updated_at) VALUES ('c.invalid', now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    remote(&ctx, "dee", "d.invalid", "d.invalid").await;
    let console = Recorder::default();
    domains::purge(
        &ctx.state,
        &console,
        &[],
        &domains::PurgeOptions {
            limited_federation_mode: true,
            ..options()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        console.lines(),
        ["Removed 1 accounts", "Removed 0 custom emojis"]
    );
    assert_eq!(known(&ctx).await, ["cal"]);
}

/// A server of the crawl: its instance, peers and activity.
async fn spawn_server(
    instance: Value,
    peers: Value,
    activity: Value,
) -> (String, std::sync::Arc<std::sync::Mutex<Value>>) {
    use axum::response::IntoResponse;
    let peers = std::sync::Arc::new(std::sync::Mutex::new(peers));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = listener.local_addr().unwrap().to_string();
    let served = peers.clone();
    let app = axum::Router::new()
        .route(
            "/api/v1/instance",
            axum::routing::get(move || {
                let instance = instance.clone();
                async move { axum::Json(instance).into_response() }
            }),
        )
        .route(
            "/api/v1/instance/peers",
            axum::routing::get(move || {
                let peers = served.lock().unwrap().clone();
                async move { axum::Json(peers).into_response() }
            }),
        )
        .route(
            "/api/v1/instance/activity",
            axum::routing::get(move || {
                let activity = activity.clone();
                async move { axum::Json(activity).into_response() }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (host, peers)
}

fn week(logins: &str, registrations: &str) -> Value {
    json!([
        {"week": "1", "statuses": "0", "logins": "0", "registrations": "0"},
        {"week": "2", "statuses": "9", "logins": logins, "registrations": registrations},
        {"week": "3", "statuses": "0", "logins": "0", "registrations": "0"}
    ])
}

/// The crawl follows each server's peers, and sums what they say.
#[tokio::test]
async fn test_crawl_follows_peers_and_sums_their_statistics() {
    let ctx = TestContext::reaching_loopback("cli-crawl").await;
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().to_string()
    };
    let (b, b_peers) = spawn_server(
        json!({"uri": "b", "stats": {"user_count": 5}}),
        json!([]),
        week("2", "1"),
    )
    .await;
    let (a, _) = spawn_server(
        json!({"uri": "a", "stats": {"user_count": 10}}),
        json!([b.clone(), closed.clone()]),
        week("7", "3"),
    )
    .await;
    *b_peers.lock().unwrap() = json!([a.clone()]);

    let crawled = domains::crawl(&ctx.state, Some(&a), 4, false, "http")
        .await
        .unwrap();
    assert_eq!(crawled.processed, 3);
    assert_eq!(crawled.failed, 1, "the server nothing answers for");
    let console = Recorder::default();
    crawled.print(&console, domains::Format::Summary);
    let lines = console.lines();
    assert!(
        lines[0].starts_with("Visited 3 domains, 1 failed ("),
        "{lines:?}"
    );
    assert_eq!(
        lines[1..],
        [
            "Total servers: 2",
            "Total registered: 15",
            "Total active last week: 9",
            "Total joined last week: 4"
        ]
    );

    let console = Recorder::default();
    crawled.print(&console, domains::Format::Domains);
    let mut found = console.lines();
    found.sort();
    let mut expected = vec![a.clone(), b.clone(), closed.clone()];
    expected.sort();
    assert_eq!(found, expected, "every domain visited, answered or not");

    let console = Recorder::default();
    crawled.print(&console, domains::Format::Json);
    let json: Value = serde_json::from_str(&console.lines()[0]).unwrap();
    assert_eq!(json[&a]["activity"][1]["logins"], "7");
    assert!(json.get(&closed).is_none());

    // Suspended here: left out, with its subdomains.
    sqlx::query(
        "INSERT INTO domain_blocks (domain, severity, created_at, updated_at)
         VALUES ($1, 1, now(), now())",
    )
    .bind(&b)
    .execute(&ctx.db)
    .await
    .unwrap();
    let crawled = domains::crawl(&ctx.state, Some(&a), 4, true, "http")
        .await
        .unwrap();
    assert!(!crawled.stats.contains_key(&b));
    assert_eq!(crawled.processed, 2);
}
