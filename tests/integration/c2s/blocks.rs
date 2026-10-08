use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::TestContext;

/// Authenticated user can list their blocked accounts.
#[tokio::test]
async fn test_blocks_returns_blocked_accounts() {
    let ctx = TestContext::new("blocks-list").await;

    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.alice_id),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;

    let resp = ctx.api.get("/api/v1/blocks", Some(&ctx.bob_token)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Vec<Value> = resp.json().await.unwrap();
    let ids: Vec<&str> = body.iter().filter_map(|a| a["id"].as_str()).collect();
    assert!(
        ids.contains(&ctx.alice_id.as_str()),
        "alice not in blocks list"
    );
}

/// Block list is empty when nothing has been blocked.
#[tokio::test]
async fn test_blocks_empty_when_none() {
    let ctx = TestContext::new("blocks-empty").await;

    let resp = ctx.api.get("/api/v1/blocks", Some(&ctx.alice_token)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Vec<Value> = resp.json().await.unwrap();
    assert!(body.is_empty());
}

/// Unauthenticated request returns 401.
#[tokio::test]
async fn test_blocks_requires_auth() {
    let ctx = TestContext::new("blocks-unauth").await;

    let resp = ctx.api.get("/api/v1/blocks", None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// GET /api/v1/blocks with limit=1 returns at most 1 account.
#[tokio::test]
async fn test_blocks_limit_param() {
    let ctx = TestContext::new("blocks-limit").await;

    // Alice blocks bob (so alice has at least 1 block).
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;

    let resp = ctx
        .api
        .get("/api/v1/blocks?limit=1", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Vec<Value> = resp.json().await.unwrap();
    assert!(body.len() <= 1, "limit=1 should return at most 1 block");
}

/// Blocking an account then unblocking it removes it from the list.
#[tokio::test]
async fn test_blocks_unblock_removes_from_list() {
    let ctx = TestContext::new("blocks-unblock").await;

    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.alice_id),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;

    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/unblock", ctx.alice_id),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;

    let resp = ctx.api.get("/api/v1/blocks", Some(&ctx.bob_token)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Vec<Value> = resp.json().await.unwrap();
    let ids: Vec<&str> = body.iter().filter_map(|a| a["id"].as_str()).collect();
    assert!(
        !ids.contains(&ctx.alice_id.as_str()),
        "alice still in blocks list after unblock"
    );
}

/// A collection of `owner`'s, made through the API, featuring `featured`.
async fn collection_featuring(ctx: &TestContext, token: &str, featured: &str) -> (i64, i64) {
    let created: Value = ctx
        .api
        .post_json(
            "/api/v1/collections",
            Some(token),
            &serde_json::json!({"name": "People", "discoverable": true}),
        )
        .await
        .json()
        .await
        .unwrap();
    let cid = created["collection"]["id"].as_str().unwrap().to_owned();
    let item: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/collections/{cid}/items"),
            Some(token),
            &serde_json::json!({"account_id": featured}),
        )
        .await
        .json()
        .await
        .unwrap();
    let item_id = item["collection_item"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    (cid.parse().unwrap(), item_id)
}

/// `BlockService#handle_collections`: the blocker is taken out of the
/// blocked account's collections, its item revoked
/// (`RevokeCollectionItemService`), and the blocked account out of the
/// blocker's, its item deleted (`DeleteCollectionItemService`). Out of a
/// remote collection, the consent the blocker gave is taken back at its
/// owner's inbox.
#[tokio::test]
async fn test_blocking_takes_both_accounts_out_of_each_others_collections() {
    let ctx = TestContext::new("blocks-collections").await;
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    sqlx::query(
        "UPDATE accounts SET private_key = $1, public_key = $2 WHERE username = 'alice' AND domain IS NULL",
    )
    .bind(&priv_pem)
    .bind(&pub_pem)
    .execute(&ctx.db)
    .await
    .unwrap();
    let (alices, bob_in_alices) = collection_featuring(&ctx, &ctx.alice_token, &ctx.bob_id).await;
    let (_, alice_in_bobs) = collection_featuring(&ctx, &ctx.bob_token, &ctx.alice_id).await;

    // Rob, on another server, features alice, with her consent.
    let rob_id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, uri, url, inbox_url,
                                 created_at, updated_at)
           VALUES ($1, 'rob', 'remote.invalid', 'rob', '', 'https://remote.invalid/users/rob',
                   'https://remote.invalid/@rob', 'https://remote.invalid/users/rob/inbox', now(), now())"#,
    )
    .bind(rob_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    let robs: i64 = sqlx::query_scalar(
        r#"INSERT INTO collections (account_id, name, uri, local, sensitive, discoverable, created_at, updated_at)
           VALUES ($1, 'Rob''s', 'https://remote.invalid/collections/7', false, false, true, now(), now())
           RETURNING id"#,
    )
    .bind(rob_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let alice_in_robs: i64 = sqlx::query_scalar(
        r#"INSERT INTO collection_items (collection_id, account_id, state, position, uri, created_at, updated_at)
           VALUES ($1, $2, 1, 1, 'https://remote.invalid/collections/7/items/1', now(), now())
           RETURNING id"#,
    )
    .bind(robs)
    .bind(ctx.alice_id.parse::<i64>().unwrap())
    .fetch_one(&ctx.db)
    .await
    .unwrap();

    for target in [&ctx.bob_id, &rob_id.to_string()] {
        let resp = ctx
            .api
            .post_json(
                &format!("/api/v1/accounts/{target}/block"),
                Some(&ctx.alice_token),
                &serde_json::json!({}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let db = ctx.db.clone();
    let state = |id: i64| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, i32>("SELECT state FROM collection_items WHERE id = $1")
                .bind(id)
                .fetch_optional(&db)
                .await
                .unwrap()
        }
    };
    assert_eq!(state(bob_in_alices).await, None, "deleted from alice's");
    assert_eq!(state(alice_in_bobs).await, Some(3), "revoked in bob's");
    assert_eq!(state(alice_in_robs).await, Some(3), "revoked in rob's");
    let count: Option<i32> = sqlx::query_scalar("SELECT item_count FROM collections WHERE id = $1")
        .bind(alices)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(count, Some(0));

    let sent: Vec<Value> = sqlx::query_scalar(
        "SELECT payload->'activity' FROM eunha.ojak_queue
         WHERE queue IN ('delivery', 'delivery-priority')
           AND payload->>'inbox' = 'https://remote.invalid/users/rob/inbox'",
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    let stamp = format!(
        "https://{}/ap/users/{}/feature_authorizations/{alice_in_robs}",
        ctx.domain, ctx.alice_id
    );
    let deletion = sent
        .iter()
        .find(|a| a["type"] == "Delete")
        .unwrap_or_else(|| panic!("no Delete sent to rob: {sent:?}"));
    assert_eq!(deletion["id"], format!("{stamp}#delete"));
    assert_eq!(deletion["object"]["type"], "FeatureAuthorization");
    assert_eq!(deletion["object"]["id"], stamp);
    assert_eq!(
        deletion["object"]["interactingObject"],
        "https://remote.invalid/collections/7"
    );
}
