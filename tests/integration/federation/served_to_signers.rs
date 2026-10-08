//! Who a post and its collections are served to, as `StatusPolicy#show?`
//! decides for the account that signed the fetch, how long a cache may
//! keep each document (`expires_in`, `vary_by`), and how an outbox is
//! paged.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

/// A remote account whose key this instance holds: its id, key id and
/// private key.
async fn remote_account(ctx: &TestContext, domain: &str, username: &str) -> (i64, String, String) {
    let (private_pem, public_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    let uri = format!("https://{domain}/users/{username}");
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key,
                                 inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, $5, $4 || '/inbox', $4 || '/outbox', now(), now())"#,
    )
    .bind(id)
    .bind(username)
    .bind(domain)
    .bind(&uri)
    .bind(public_pem)
    .execute(&ctx.db)
    .await
    .unwrap();
    (id, format!("{uri}#main-key"), private_pem)
}

async fn follow(ctx: &TestContext, follower: i64, target: &str) {
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, now(), now())",
    )
    .bind(follower)
    .bind(target.parse::<i64>().unwrap())
    .execute(&ctx.db)
    .await
    .unwrap();
}

fn header<'a>(resp: &'a reqwest::Response, name: &str) -> Option<&'a str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

/// What a response varies by, besides what eunha's compression and CORS
/// layers add to every response they touch, as Mastodon's Rack middleware
/// and front proxy do.
fn vary(resp: &reqwest::Response) -> Option<String> {
    let vary: Vec<&str> = resp
        .headers()
        .get_all("vary")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|v| {
            !v.is_empty()
                && ![
                    "accept-encoding",
                    "origin",
                    "access-control-request-method",
                    "access-control-request-headers",
                ]
                .iter()
                .any(|added| v.eq_ignore_ascii_case(added))
        })
        .collect();
    (!vary.is_empty()).then(|| vary.join(", "))
}

/// A followers-only post, and its replies, likes and shares, to a signed
/// follower; a direct one to whom it mentions; neither to anyone else.
#[tokio::test]
async fn private_posts_are_served_to_who_may_see_them() {
    let ctx = TestContext::new("ap-private-served").await;
    let (eve, eve_key, eve_pem) = remote_account(&ctx, "peer.invalid", "eve").await;
    let (_, mallory_key, mallory_pem) = remote_account(&ctx, "peer.invalid", "mallory").await;
    follow(&ctx, eve, &ctx.alice_id).await;

    let private = ctx
        .api
        .post_status(&ctx.alice_token, "for followers", "private")
        .await;
    let id = private["id"].as_str().unwrap();
    for suffix in ["", "/activity", "/replies", "/likes", "/shares"] {
        let path = format!("/users/alice/statuses/{id}{suffix}");
        assert_eq!(
            ctx.api.ap_get(&path, None).await.status(),
            StatusCode::NOT_FOUND,
            "unsigned {path}"
        );
        assert_eq!(
            ctx.api
                .ap_get_signed(&path, &mallory_key, &mallory_pem)
                .await
                .status(),
            StatusCode::NOT_FOUND,
            "a stranger {path}"
        );
        let resp = ctx.api.ap_get_signed(&path, &eve_key, &eve_pem).await;
        assert_eq!(resp.status(), StatusCode::OK, "a follower {path}");
        assert_eq!(
            header(&resp, "cache-control"),
            Some(if suffix.is_empty() {
                "private, no-store"
            } else if suffix == "/activity" {
                "max-age=180, private"
            } else {
                "max-age=0, private"
            }),
            "what is not distributable is no shared cache's: {path}"
        );
    }

    // `@eve@peer.invalid` is not resolved here, so the mention is made by hand.
    let direct = ctx
        .api
        .post_status(&ctx.alice_token, "just you", "direct")
        .await;
    let direct_id = direct["id"].as_str().unwrap();
    sqlx::query(
        "INSERT INTO mentions (status_id, account_id, silent, created_at, updated_at)
         VALUES ($1, $2, false, now(), now())",
    )
    .bind(direct_id.parse::<i64>().unwrap())
    .bind(eve)
    .execute(&ctx.db)
    .await
    .unwrap();
    let path = format!("/users/alice/statuses/{direct_id}");
    assert_eq!(
        ctx.api
            .ap_get_signed(&path, &eve_key, &eve_pem)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        ctx.api
            .ap_get_signed(&path, &mallory_key, &mallory_pem)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

/// `expires_in` and `vary_by`, as each controller sets them, in public
/// fetch mode and in authorized fetch mode.
#[tokio::test]
async fn documents_say_how_long_they_may_be_cached() {
    let ctx = TestContext::new("ap-cache-headers").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "cache me", "public")
        .await;
    let id = status["id"].as_str().unwrap();
    for (path, cache, vary_expected) in [
        (
            format!("/users/alice/statuses/{id}"),
            "max-age=180, public",
            Some("Accept, Accept-Language, Cookie"),
        ),
        (
            format!("/users/alice/statuses/{id}/replies"),
            "max-age=0, public",
            None,
        ),
        (
            "/users/alice".to_owned(),
            "max-age=180, public",
            Some("Accept, Accept-Language, Cookie"),
        ),
        (
            "/users/alice/outbox".to_owned(),
            "max-age=180, public",
            None,
        ),
        (
            "/users/alice/outbox?page=true".to_owned(),
            "max-age=60, public",
            Some("Signature"),
        ),
        (
            "/users/alice/collections/featured".to_owned(),
            "max-age=180, public",
            None,
        ),
        ("/actor".to_owned(), "max-age=600, public", None),
    ] {
        let resp = ctx.api.ap_get(&path, None).await;
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        assert_eq!(header(&resp, "cache-control"), Some(cache), "{path}");
        assert_eq!(vary(&resp).as_deref(), vary_expected, "{path}");
    }
    let missing = ctx.api.ap_get("/users/alice/statuses/1", None).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(header(&missing, "cache-control"), Some("private, no-store"));

    let ctx = TestContext::with_instance("ap-cache-headers-af", |instance| {
        instance.authorized_fetch = Some(true)
    })
    .await;
    let (_, key, pem) = remote_account(&ctx, "peer.invalid", "eve").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "cache me", "public")
        .await;
    let id = status["id"].as_str().unwrap();
    let path = format!("/users/alice/statuses/{id}/replies");
    let unsigned = ctx.api.ap_get(&path, None).await;
    assert_eq!(unsigned.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(vary(&unsigned).as_deref(), Some("Signature"));
    let signed = ctx.api.ap_get_signed(&path, &key, &pem).await;
    assert_eq!(signed.status(), StatusCode::OK);
    assert_eq!(vary(&signed).as_deref(), Some("Signature"));
    assert_eq!(
        header(&signed, "cache-control"),
        Some("private, no-store"),
        "what varies by a signature it carried is kept by no cache"
    );
}

/// `OutboxesController`: `?page=true`, `max_id` and `min_id`, twenty to a
/// page, boosts as `Announce`s, and followers-only posts to a follower.
#[tokio::test]
async fn an_outbox_is_paged_as_mastodon_pages_it() {
    let ctx = TestContext::new("ap-outbox-pages").await;
    let mut ids = Vec::new();
    for n in 0..21 {
        let status = ctx
            .api
            .post_status(&ctx.alice_token, &format!("post {n}"), "public")
            .await;
        ids.push(status["id"].as_str().unwrap().to_owned());
    }
    let private = ctx
        .api
        .post_status(&ctx.alice_token, "followers", "private")
        .await;
    let bob_post = ctx
        .api
        .post_status(&ctx.bob_token, "boost me", "public")
        .await;
    let resp = ctx
        .api
        .post_json(
            &format!(
                "/api/v1/statuses/{}/reblog",
                bob_post["id"].as_str().unwrap()
            ),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let base = format!("https://{}/users/alice/outbox", ctx.domain);
    let outbox: Value = ctx
        .api
        .ap_get("/users/alice/outbox", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(outbox["id"].as_str(), Some(base.as_str()));
    assert_eq!(
        outbox["first"].as_str(),
        Some(format!("{base}?page=true").as_str())
    );
    assert_eq!(
        outbox["last"].as_str(),
        Some(format!("{base}?min_id=0&page=true").as_str())
    );

    let page: Value = ctx
        .api
        .ap_get("/users/alice/outbox?page=true", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(page["type"].as_str(), Some("OrderedCollectionPage"));
    assert_eq!(
        page["id"].as_str(),
        Some(format!("{base}?page=true").as_str())
    );
    assert_eq!(page["partOf"].as_str(), Some(base.as_str()));
    let items = page["orderedItems"].as_array().unwrap();
    assert_eq!(items.len(), 20);
    assert_eq!(
        items[0]["type"].as_str(),
        Some("Announce"),
        "the boost, newest"
    );
    assert!(items.iter().all(|item| item.get("@context").is_none()));
    assert!(
        !items.iter().any(|item| item["object"]["id"]
            .as_str()
            .is_some_and(|id| id.ends_with(private["id"].as_str().unwrap()))),
        "no followers-only post to the unsigned"
    );
    let last = items[19]["object"]["id"].as_str().unwrap();
    let last_id = last.rsplit('/').next().unwrap();
    assert_eq!(
        page["next"].as_str(),
        Some(format!("{base}?max_id={last_id}&page=true").as_str())
    );
    let next: Value = ctx
        .api
        .ap_get(
            &format!("/users/alice/outbox?max_id={last_id}&page=true"),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    let rest = next["orderedItems"].as_array().unwrap();
    assert_eq!(rest.len(), 2, "{next}");
    assert!(next.get("next").is_none());
    assert!(rest[1]["object"]["id"].as_str().unwrap().ends_with(&ids[0]));

    // Up from the oldest: the twenty just above it, newest first.
    let up: Value = ctx
        .api
        .ap_get(
            &format!("/users/alice/outbox?min_id={}&page=true", ids[0]),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    let up = up["orderedItems"].as_array().unwrap();
    assert_eq!(up.len(), 20);
    assert!(up[19]["object"]["id"].as_str().unwrap().ends_with(&ids[1]));

    let (eve, key, pem) = remote_account(&ctx, "peer.invalid", "eve").await;
    follow(&ctx, eve, &ctx.alice_id).await;
    let signed: Value = ctx
        .api
        .ap_get_signed("/users/alice/outbox?page=true", &key, &pem)
        .await
        .json()
        .await
        .unwrap();
    assert!(
        signed["orderedItems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["object"]["id"]
                .as_str()
                .is_some_and(|id| id.ends_with(private["id"].as_str().unwrap()))),
        "a follower's outbox has the followers-only post"
    );
}

/// A context with no conversation behind it fails as Mastodon's does.
#[tokio::test]
async fn a_context_that_is_not_there_fails_as_mastodons_does() {
    let ctx = TestContext::new("ap-context-missing").await;
    let resp = ctx.api.ap_get("/contexts/1-2", None).await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let resp = ctx.api.ap_get("/contexts/nothing", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// `REST::FeaturedTagSerializer#name` is how the tag was featured, and a
/// tagged collection is rendered for whoever reads it.
#[tokio::test]
async fn rest_featured_tags_and_tagged_collections_are_mastodons() {
    let ctx = TestContext::new("rest-featured-tagged").await;
    let resp = ctx
        .api
        .post_json(
            "/api/v1/featured_tags",
            Some(&ctx.alice_token),
            &json!({"name": "Cats"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let tags: Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/featured_tags", ctx.alice_id),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(tags[0]["name"].as_str(), Some("Cats"), "{tags}");
    assert!(tags[0]["url"]
        .as_str()
        .unwrap()
        .ends_with("/@alice/tagged/cats"));

    let collection: Value = ctx
        .api
        .post_json(
            "/api/v1/collections",
            Some(&ctx.alice_token),
            &json!({"name": "Mine", "discoverable": true}),
        )
        .await
        .json()
        .await
        .unwrap();
    let cid: i64 = collection["collection"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    // A pending item, which only the owner sees.
    let (rob, _, _) = remote_account(&ctx, "peer.invalid", "rob").await;
    sqlx::query(
        "INSERT INTO collection_items (collection_id, account_id, state, position, created_at, updated_at)
         VALUES ($1, $2, 0, 1, now(), now())",
    )
    .bind(cid)
    .bind(rob)
    .execute(&ctx.db)
    .await
    .unwrap();
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "see my collection", "public")
        .await;
    let sid: i64 = status["id"].as_str().unwrap().parse().unwrap();
    sqlx::query(
        "INSERT INTO tagged_objects (status_id, object_type, object_id, ap_type, created_at, updated_at)
         VALUES ($1, 'Collection', $2, 'FeaturedCollection', now(), now())",
    )
    .bind(sid)
    .bind(cid)
    .execute(&ctx.db)
    .await
    .unwrap();
    for (token, items) in [(&ctx.alice_token, 1), (&ctx.bob_token, 0)] {
        let status: Value = ctx
            .api
            .get(&format!("/api/v1/statuses/{sid}"), Some(token))
            .await
            .json()
            .await
            .unwrap();
        let tagged = &status["tagged_collections"];
        assert_eq!(
            tagged[0]["id"].as_str(),
            Some(cid.to_string().as_str()),
            "{status}"
        );
        assert_eq!(tagged[0]["items"].as_array().map(Vec::len), Some(items));
    }
}
