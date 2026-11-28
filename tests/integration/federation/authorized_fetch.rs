//! What peers meet without authorized fetch (Mastodon's secure mode, which
//! eunha does not have) and in limited federation mode: what is served to an
//! unsigned or a signed fetch, and which servers may deliver at all.

use reqwest::StatusCode;
use serde_json::json;

use crate::helpers::TestContext;

/// A remote account whose key this instance holds, and its private key.
async fn remote_account(ctx: &TestContext, domain: &str, username: &str) -> (i64, String, String) {
    let (private_pem, public_pem) = ojak::sig::signature::generate_rsa_keypair(
        &mut rand::rand_core::UnwrapErr(rand::rngs::SysRng),
    )
    .unwrap();
    let uri = format!("https://{domain}/users/{username}");
    let id = eunha::snowflake::next_id();
    sqlx::query!(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key,
                                 inbox_url, outbox_url, protocol, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4::text, $4::text, $5, $4::text||'/inbox',
                   $4::text||'/outbox', 1, now(), now())"#,
        id,
        username,
        domain,
        uri,
        public_pem,
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    (id, format!("{uri}#main-key"), private_pem)
}

/// eunha has no authorized fetch mode (the `no-authorized-fetch`
/// divergence): an unsigned fetch of anything public is served, and so is a
/// signed one.
#[tokio::test]
async fn unsigned_fetches_of_public_documents_are_served() {
    let ctx = TestContext::new("af-none").await;
    let (_, key_id, pem) = remote_account(&ctx, "peer.invalid", "eve").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "for anyone", "public")
        .await;
    let status = format!("/users/alice/statuses/{}", status["id"].as_str().unwrap());
    for path in [
        "/users/alice",
        status.as_str(),
        "/users/alice/outbox",
        "/users/alice/followers",
        "/users/alice/following",
        "/users/alice/collections/featured",
        "/actor",
    ] {
        assert_eq!(
            ctx.api.ap_get(path, None).await.status(),
            StatusCode::OK,
            "{path}, unsigned"
        );
        assert_eq!(
            ctx.api.ap_get_signed(path, &key_id, &pem).await.status(),
            StatusCode::OK,
            "{path}, signed"
        );
    }
}

/// Mastodon's `authorized_fetch` site setting, as a database taken over from
/// Mastodon may carry it, changes nothing.
#[tokio::test]
async fn the_authorized_fetch_setting_is_ignored() {
    let ctx = TestContext::new("af-setting").await;
    for saved in ["actors", "all", "true"] {
        crate::helpers::set_setting(&ctx.db, "authorized_fetch", saved).await;
        for path in ["/users/alice", "/users/alice/outbox"] {
            assert_eq!(
                ctx.api.ap_get(path, None).await.status(),
                StatusCode::OK,
                "{path} with authorized_fetch = {saved}"
            );
        }
    }
}

/// Though no fetch needs a signature, one signed from a suspended domain is
/// refused, 403, and a status is not there for a signer its author blocks.
#[tokio::test]
async fn signed_fetches_from_blocked_signers_are_refused() {
    let ctx = TestContext::new("af-blocked").await;
    let (_, banned_key, banned_pem) = remote_account(&ctx, "banned.invalid", "mallory").await;
    sqlx::query!(
        "INSERT INTO domain_blocks (domain, severity, created_at, updated_at)
         VALUES ('banned.invalid', 1, now(), now())"
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        ctx.api
            .ap_get_signed("/users/alice", &banned_key, &banned_pem)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );

    let (eve_id, key_id, pem) = remote_account(&ctx, "peer.invalid", "eve").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "not for eve", "public")
        .await;
    let path = format!("/users/alice/statuses/{}", status["id"].as_str().unwrap());
    let blocked = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{eve_id}/block"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(blocked.status(), StatusCode::OK);
    assert_eq!(
        ctx.api.ap_get_signed(&path, &key_id, &pem).await.status(),
        StatusCode::NOT_FOUND
    );
    let outbox: serde_json::Value = ctx
        .api
        .ap_get_signed("/users/alice/outbox", &key_id, &pem)
        .await
        .json()
        .await
        .unwrap();
    let first = url::Url::parse(outbox["first"].as_str().expect("a first page")).unwrap();
    let first = match first.query() {
        Some(query) => format!("{}?{query}", first.path()),
        None => first.path().to_owned(),
    };
    let page: serde_json::Value = ctx
        .api
        .ap_get_signed(&first, &key_id, &pem)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(page["orderedItems"], json!([]), "{page}");
    let unblocked = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{eve_id}/unblock"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(unblocked.status(), StatusCode::OK);
    assert_eq!(
        ctx.api.ap_get_signed(&path, &key_id, &pem).await.status(),
        StatusCode::OK
    );
    let page: serde_json::Value = ctx
        .api
        .ap_get_signed(&first, &key_id, &pem)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        page["orderedItems"].as_array().map(Vec::len),
        Some(1),
        "{page}"
    );
}

/// Limited federation mode, which shows visitors nothing, needs every fetch
/// signed, except the instance actor's and WebFinger, which a peer needs
/// before it can sign.
#[tokio::test]
async fn limited_federation_requires_signed_fetches() {
    let ctx = TestContext::with_instance("lf-fetch", |instance| {
        instance.limited_federation_mode = true;
    })
    .await;
    crate::helpers::set_setting(&ctx.db, "authorized_fetch", "none").await;
    for path in [
        "/users/alice",
        "/users/alice/outbox",
        "/users/alice/followers",
    ] {
        assert_eq!(
            ctx.api.ap_get(path, None).await.status(),
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }
    assert_eq!(
        ctx.api.ap_get("/actor", None).await.status(),
        StatusCode::OK,
        "the instance actor stays public"
    );
    let webfinger = ctx
        .api
        .get(
            &format!("/.well-known/webfinger?resource=acct:alice@{}", ctx.domain),
            None,
        )
        .await;
    assert_eq!(webfinger.status(), StatusCode::OK);
    // A signer from a domain not on the allow list is refused before its key
    // is looked at; one from an allowed domain is served.
    let (_, key_id, pem) = remote_account(&ctx, "elsewhere.invalid", "eve").await;
    assert_eq!(
        ctx.api
            .ap_get_signed("/users/alice", &key_id, &pem)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    sqlx::query!(
        "INSERT INTO domain_allows (domain, created_at, updated_at)
         VALUES ('elsewhere.invalid', now(), now())"
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        ctx.api
            .ap_get_signed("/users/alice", &key_id, &pem)
            .await
            .status(),
        StatusCode::OK
    );
}

/// In limited federation mode only a domain on the allow list may deliver.
#[tokio::test]
async fn limited_federation_takes_deliveries_only_from_allowed_domains() {
    let ctx = TestContext::with_instance("lf-inbox", |instance| {
        instance.limited_federation_mode = true
    })
    .await;
    sqlx::query!(
        "INSERT INTO domain_allows (domain, created_at, updated_at)
         VALUES ('friend.invalid', now(), now())"
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let alice = format!("https://{}/users/alice", ctx.domain);
    let follows_of = |account_id: i64| {
        sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!" FROM follows WHERE account_id = $1"#,
            account_id
        )
        .fetch_one(&ctx.db)
    };

    let (stranger_id, stranger_key, stranger_pem) =
        remote_account(&ctx, "stranger.invalid", "sam").await;
    let follow = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://stranger.invalid/follows/1",
        "type": "Follow",
        "actor": "https://stranger.invalid/users/sam",
        "object": alice,
    });
    let refused = ctx
        .api
        .post_signed("/inbox", &follow, &stranger_key, &stranger_pem)
        .await;
    // Refused before the key is looked for, as `keypair_from_key_id` refuses
    // it, with the body `require_actor_signature!` renders.
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = refused.json().await.unwrap();
    assert_eq!(
        body,
        json!({ "error": format!("Public key not found for key {stranger_key}") })
    );
    assert_eq!(follows_of(stranger_id).await.unwrap(), 0);

    let (friend_id, friend_key, friend_pem) = remote_account(&ctx, "friend.invalid", "fran").await;
    let follow = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://friend.invalid/follows/1",
        "type": "Follow",
        "actor": "https://friend.invalid/users/fran",
        "object": alice,
    });
    let taken = ctx
        .api
        .post_signed("/inbox", &follow, &friend_key, &friend_pem)
        .await;
    assert_eq!(taken.status(), StatusCode::ACCEPTED);
    assert_eq!(follows_of(friend_id).await.unwrap(), 1);
}
