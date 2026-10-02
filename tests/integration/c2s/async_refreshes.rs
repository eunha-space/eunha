//! `GET /api/v1_alpha/async_refreshes/:id` and the `Mastodon-Async-Refresh`
//! header (`AsyncRefresh`, `AsyncRefreshesConcern`).

use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::{seed_token_with_scopes, TestContext};
use eunha::async_refresh::{self, AsyncRefresh};

fn header_id(header: &str) -> String {
    header
        .strip_prefix("id=\"")
        .and_then(|rest| rest.split_once('"'))
        .map(|(id, _)| id.to_owned())
        .unwrap_or_else(|| panic!("no id in {header:?}"))
}

/// A running refresh reads back as running, with its count; a finished one as
/// finished; an id that was not signed here, or names nothing, is a 404.
#[tokio::test]
async fn test_async_refresh_lifecycle() {
    let ctx = TestContext::new("async-refresh").await;
    let refresh = AsyncRefresh::create(&ctx.state, "test:refresh", true).await;
    let header = refresh.header_value(&ctx.state, 3).unwrap();
    assert!(header.ends_with(", retry=3, result_count=0"), "{header}");
    let id = header_id(&header);
    assert_eq!(id, refresh.id(&ctx.state));

    let path = format!("/api/v1_alpha/async_refreshes/{id}");
    let resp = ctx.api.get(&path, Some(&ctx.alice_token)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["async_refresh"]["id"], id.as_str());
    assert_eq!(body["async_refresh"]["status"], "running");
    assert_eq!(body["async_refresh"]["result_count"], 0);

    async_refresh::increment_result_count(&ctx.state, "test:refresh", 2).await;
    async_refresh::finish(&ctx.state, "test:refresh").await;
    let body: Value = ctx
        .api
        .get(&path, Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["async_refresh"]["status"], "finished");
    assert_eq!(body["async_refresh"]["result_count"], 2);
    // A finished refresh sends no header.
    let finished = AsyncRefresh::new(&ctx.state, "test:refresh").await;
    assert!(finished.is_finished());
    assert!(finished.header_value(&ctx.state, 3).is_none());

    // Work that does not count results reports a null count.
    let uncounted = AsyncRefresh::create(&ctx.state, "test:uncounted", false).await;
    let body: Value = ctx
        .api
        .get(
            &format!("/api/v1_alpha/async_refreshes/{}", uncounted.id(&ctx.state)),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(body["async_refresh"]["result_count"].is_null());
    assert!(!uncounted
        .header_value(&ctx.state, 2)
        .unwrap()
        .contains("result_count"));

    // Tampered with: the key changed, the signature kept.
    let (_, digest) = id.rsplit_once("--").unwrap();
    let forged = format!("dGVzdDpvdGhlcg--{digest}");
    for bad in [forged.as_str(), "garbage", "--"] {
        let resp = ctx
            .api
            .get(
                &format!("/api/v1_alpha/async_refreshes/{bad}"),
                Some(&ctx.alice_token),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{bad}");
    }

    // Genuine, but naming a refresh that does not exist (or has expired).
    let gone = AsyncRefresh::new(&ctx.state, "test:never-created").await;
    let resp = ctx
        .api
        .get(
            &format!("/api/v1_alpha/async_refreshes/{}", gone.id(&ctx.state)),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// With the Mastodon's `secret_key_base`, ids are what its
/// `message_verifier('async_refreshes')` makes (the vector from
/// *scripts/rails_signing_vectors.rb*), and one signed from the VAPID key, as
/// eunha's were without it, is still read.
#[tokio::test]
async fn test_async_refresh_ids_with_a_secret_key_base() {
    let ctx = TestContext::with_instance_config("async-refresh-skb", |instance| {
        instance.secret_key_base = Some(eunha::secret_key_base::SecretKeyBase::new(
            "0123456789abcdef".repeat(8),
        ));
    })
    .await;
    let key = "async_refreshes:v1:accounts:123:refresh_followers";
    let mastodon_id = "ImFzeW5jX3JlZnJlc2hlczp2MTphY2NvdW50czoxMjM6cmVmcmVzaF9mb2xsb3dlcnMi--509406b78242a360e365efdfc925d0e7c576618f";
    let refresh = AsyncRefresh::create(&ctx.state, key, true).await;
    assert_eq!(refresh.id(&ctx.state), mastodon_id);

    let vapid_signed = eunha::crypto::sign_message(
        &ctx.state.instance.vapid_private_key,
        b"async_refreshes",
        key,
    );
    for id in [mastodon_id, vapid_signed.as_str()] {
        let resp = ctx
            .api
            .get(
                &format!("/api/v1_alpha/async_refreshes/{id}"),
                Some(&ctx.alice_token),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK, "{id}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["async_refresh"]["status"], "running");
    }
}

/// `doorkeeper_authorize! :read` and `require_user!`.
#[tokio::test]
async fn test_async_refresh_requires_a_user_with_read() {
    let ctx = TestContext::new("async-refresh-auth").await;
    let refresh = AsyncRefresh::create(&ctx.state, "test:auth", false).await;
    let path = format!("/api/v1_alpha/async_refreshes/{}", refresh.id(&ctx.state));

    let resp = ctx.api.get(&path, None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let alice: i64 = ctx.alice_id.parse().unwrap();
    let write_only = seed_token_with_scopes(&ctx.db, alice, "write").await;
    let resp = ctx.api.get(&path, Some(&write_only)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let read_statuses = seed_token_with_scopes(&ctx.db, alice, "read:statuses").await;
    let resp = ctx.api.get(&path, Some(&read_statuses)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // A user who may not use the API (here: unconfirmed) is refused.
    sqlx::query("UPDATE users SET confirmed_at = NULL WHERE account_id = $1")
        .bind(alice)
        .execute(&ctx.db)
        .await
        .unwrap();
    let resp = ctx.api.get(&path, Some(&ctx.alice_token)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// While a report is being generated, its state says so and carries the
/// refresh, and asking again starts nothing new.
#[tokio::test]
async fn test_annual_report_generating_carries_the_refresh() {
    let ctx = TestContext::new("async-refresh-wrap").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    sqlx::query(
        "INSERT INTO statuses (id, account_id, text, visibility, created_at, updated_at)
         VALUES ($1, $2, 'a post in 2021', 0, '2021-06-15T12:00:00Z'::timestamptz, now())",
    )
    .bind(eunha::snowflake::next_id())
    .bind(alice)
    .execute(&ctx.db)
    .await
    .unwrap();
    let key = format!("wrapstodon:{alice}:2021");
    let refresh = AsyncRefresh::create(&ctx.state, &key, false).await;
    let expected = format!("id=\"{}\", retry=2", refresh.id(&ctx.state));

    let resp = ctx
        .api
        .get("/api/v1/annual_reports/2021/state", Some(&ctx.alice_token))
        .await;
    assert_eq!(
        resp.headers()["mastodon-async-refresh"].to_str().unwrap(),
        expected
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["state"], "generating");

    let resp = ctx
        .api
        .post_json(
            "/api/v1/annual_reports/2021/generate",
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(
        resp.headers()["mastodon-async-refresh"].to_str().unwrap(),
        expected
    );
    // Nothing was generated behind the running refresh.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let generated: i64 =
        sqlx::query_scalar("SELECT count(*) FROM generated_annual_reports WHERE account_id = $1")
            .bind(alice)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(generated, 0);
}

/// Divergence `home-feed-built-from-database`: a home feed Redis does not
/// hold is read from the database there and then, so it is never answered as
/// partial (206) with a refresh to wait for, as Mastodon answers while it
/// regenerates one.
#[tokio::test]
async fn test_cold_home_feed_is_whole_without_a_refresh() {
    let ctx = TestContext::new("async-refresh-home").await;
    ctx.api
        .post_status(&ctx.bob_token, "bob was here", "public")
        .await;
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    // Forget whatever the follow put in alice's feed.
    let mut redis = ctx.state.redis.clone();
    let _: () = redis::cmd("DEL")
        .arg(
            ctx.state
                .redis_keys
                .key(format!("feed:home:{}", ctx.alice_id)),
        )
        .arg(
            ctx.state
                .redis_keys
                .key(format!("feed:home:{}:populated", ctx.alice_id)),
        )
        .query_async(&mut redis)
        .await
        .unwrap();

    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("mastodon-async-refresh").is_none());
    let body: Value = resp.json().await.unwrap();
    assert!(body
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["content"].as_str().unwrap().contains("bob was here")));
}
