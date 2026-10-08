//! What Mastodon serves of an account and its posts beyond the actor and
//! the posts themselves: a post's replies, likes and shares, the thread it
//! started, the account's featured hashtags, the instance actor's outbox,
//! and what the actor document says of them.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

async fn post(ctx: &TestContext, token: &str, body: &Value) -> String {
    let resp = ctx
        .api
        .post_json("/api/v1/statuses", Some(token), body)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let status: Value = resp.json().await.unwrap();
    status["id"].as_str().expect("status id").to_string()
}

async fn ap(ctx: &TestContext, path: &str) -> Value {
    let resp = ctx.api.ap_get(path, None).await;
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{path}: {text}");
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("{path} is not JSON: {text:.120}"))
}

/// `ActivityPub::RepliesController`: the author's own replies first, then
/// everyone else's, and a local reply embedded.
#[tokio::test]
async fn replies_are_the_authors_then_everyone_elses() {
    let ctx = TestContext::new("ap-replies").await;
    let id = post(
        &ctx,
        &ctx.alice_token,
        &json!({"status": "a thread", "visibility": "public"}),
    )
    .await;
    let own = post(
        &ctx,
        &ctx.alice_token,
        &json!({"status": "and more", "visibility": "public", "in_reply_to_id": id}),
    )
    .await;
    let other = post(
        &ctx,
        &ctx.bob_token,
        &json!({"status": "@alice hi", "visibility": "public", "in_reply_to_id": id}),
    )
    .await;
    // Not served: a private reply.
    post(
        &ctx,
        &ctx.bob_token,
        &json!({"status": "@alice psst", "visibility": "private", "in_reply_to_id": id}),
    )
    .await;

    let base = format!("https://{}/users/alice/statuses/{id}/replies", ctx.domain);
    let replies = ap(&ctx, &format!("/users/alice/statuses/{id}/replies")).await;
    assert_eq!(replies["id"].as_str(), Some(base.as_str()));
    assert_eq!(replies["type"].as_str(), Some("Collection"));
    let first = &replies["first"];
    assert_eq!(first["type"].as_str(), Some("CollectionPage"));
    assert_eq!(
        first["id"].as_str(),
        Some(format!("{base}?page=true").as_str())
    );
    assert_eq!(first["partOf"].as_str(), Some(base.as_str()));
    assert_eq!(
        first["next"].as_str(),
        Some(format!("{base}?only_other_accounts=true&page=true").as_str())
    );
    let items = first["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{first}");
    assert_eq!(
        items[0]["type"].as_str(),
        Some("Note"),
        "a local reply is embedded"
    );
    assert!(items[0]["id"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/{own}")));

    let others = ap(
        &ctx,
        &format!("/users/alice/statuses/{id}/replies?only_other_accounts=true&page=true"),
    )
    .await;
    assert_eq!(others["type"].as_str(), Some("CollectionPage"));
    assert_eq!(
        others["id"].as_str(),
        Some(format!("{base}?only_other_accounts=true&page=true").as_str())
    );
    assert!(others.get("next").is_none(), "fewer than sixty: {others}");
    let items = others["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "the private reply is not there: {others}");
    assert!(items[0]["id"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/{other}")));

    // The note names the collection it is served at.
    let note = ap(&ctx, &format!("/users/alice/statuses/{id}")).await;
    assert_eq!(note["replies"]["id"].as_str(), Some(base.as_str()));

    // A private post's replies are not there.
    let private = post(
        &ctx,
        &ctx.alice_token,
        &json!({"status": "mine", "visibility": "private"}),
    )
    .await;
    let resp = ctx
        .api
        .ap_get(&format!("/users/alice/statuses/{private}/replies"), None)
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // Nor another account's post under this one.
    let resp = ctx
        .api
        .ap_get(&format!("/users/alice/statuses/{other}/replies"), None)
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// `ActivityPub::LikesController` and `SharesController`: how many, under
/// either scheme.
#[tokio::test]
async fn likes_and_shares_are_counted() {
    let ctx = TestContext::new("ap-likes").await;
    let id = post(
        &ctx,
        &ctx.alice_token,
        &json!({"status": "like me", "visibility": "public"}),
    )
    .await;
    for action in ["favourite", "reblog"] {
        let resp = ctx
            .api
            .post_json(
                &format!("/api/v1/statuses/{id}/{action}"),
                Some(&ctx.bob_token),
                &json!({}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
    for (path, suffix) in [("likes", "likes"), ("shares", "shares")] {
        let doc = ap(&ctx, &format!("/users/alice/statuses/{id}/{path}")).await;
        assert_eq!(
            doc,
            json!({
                "@context": "https://www.w3.org/ns/activitystreams",
                "id": format!("https://{}/users/alice/statuses/{id}/{suffix}", ctx.domain),
                "type": "Collection",
                "totalItems": 1,
            })
        );
        // Under the numeric scheme too, named by the scheme the account uses.
        let doc = ap(
            &ctx,
            &format!("/ap/users/{}/statuses/{id}/{path}", ctx.alice_id),
        )
        .await;
        assert_eq!(
            doc["id"].as_str(),
            Some(format!("https://{}/users/alice/statuses/{id}/{suffix}", ctx.domain).as_str())
        );
    }
}

/// `ActivityPub::ContextsController`: a thread started here, by the post
/// that started it.
#[tokio::test]
async fn a_thread_is_served_as_its_context() {
    let ctx = TestContext::new("ap-context").await;
    let id = post(
        &ctx,
        &ctx.alice_token,
        &json!({"status": "start", "visibility": "public"}),
    )
    .await;
    let reply = post(
        &ctx,
        &ctx.bob_token,
        &json!({"status": "@alice reply", "visibility": "unlisted", "in_reply_to_id": id}),
    )
    .await;

    let note = ap(&ctx, &format!("/users/alice/statuses/{id}")).await;
    let context = note["context"].as_str().expect("a context").to_owned();
    assert_eq!(
        context,
        format!("https://{}/contexts/{}-{id}", ctx.domain, ctx.alice_id)
    );
    let path = context.trim_start_matches(&format!("https://{}", ctx.domain));
    let doc = ap(&ctx, path).await;
    assert_eq!(doc["id"].as_str(), Some(context.as_str()));
    assert_eq!(doc["type"].as_str(), Some("Collection"));
    assert_eq!(
        doc["attributedTo"].as_str(),
        Some(format!("https://{}/users/alice", ctx.domain).as_str())
    );
    assert_eq!(doc["first"]["type"].as_str(), Some("CollectionPage"));
    assert_eq!(doc["first"]["partOf"].as_str(), Some(context.as_str()));
    let items: Vec<&str> = doc["first"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(items.len(), 2, "{doc}");
    assert!(items[1].ends_with(&format!("/statuses/{reply}")));

    let page = ap(&ctx, &format!("{path}/items?page=true")).await;
    assert_eq!(page["type"].as_str(), Some("CollectionPage"));
    assert_eq!(
        page["id"].as_str(),
        Some(format!("{context}/items?page=true").as_str())
    );
    assert_eq!(page["items"].as_array().map(Vec::len), Some(2));

    let resp = ctx.api.ap_get("/contexts/1-2", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// The actor names what Mastodon's names, and its featured hashtags and
/// profile hashtags are served.
#[tokio::test]
async fn the_actor_names_its_hashtags_and_settings() {
    let ctx = TestContext::new("ap-actor-tags").await;
    let resp = ctx
        .api
        .patch_multipart(
            "/api/v1/accounts/update_credentials",
            &ctx.alice_token,
            &[
                ("note", "I like #Cats and #dogs"),
                ("attribution_domains[]", "https://Blog.example"),
            ],
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = ctx
        .api
        .post_json(
            "/api/v1/featured_tags",
            Some(&ctx.alice_token),
            &json!({"name": "#Cats"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let featured: Value = resp.json().await.unwrap();
    assert_eq!(
        featured["name"].as_str(),
        Some("Cats"),
        "as it was featured"
    );

    let actor = ap(&ctx, "/users/alice").await;
    let base = format!("https://{}/users/alice", ctx.domain);
    assert_eq!(
        actor["webfinger"].as_str(),
        Some(format!("alice@{}", ctx.domain).as_str())
    );
    assert_eq!(
        actor["featuredTags"].as_str(),
        Some(format!("{base}/collections/tags").as_str())
    );
    assert_eq!(actor["memorial"], json!(false));
    assert_eq!(actor["showFeatured"], json!(true));
    assert_eq!(actor["showMedia"], json!(true));
    assert!(actor["showRepliesInMedia"].is_boolean());
    assert!(actor["published"].as_str().unwrap().ends_with("T00:00:00Z"));
    assert!(actor["interactionPolicy"]["canFeature"]["automaticApproval"].is_array());
    assert_eq!(actor["attributionDomains"], json!(["Blog.example"]));
    assert!(actor.get("movedTo").is_none(), "not moved: {actor}");
    let context = actor["@context"].as_array().unwrap();
    assert_eq!(context[2], "https://purl.archive.org/socialweb/webfinger");
    assert_eq!(context.last().unwrap()["Hashtag"], "as:Hashtag");
    let tags: Vec<&str> = actor["tag"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["type"] == "Hashtag")
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(tags, ["#cats", "#dogs"]);

    let collection = ap(&ctx, "/users/alice/collections/tags").await;
    assert_eq!(
        collection,
        json!({
            "@context": ["https://www.w3.org/ns/activitystreams", {"Hashtag": "as:Hashtag"}],
            "id": format!("{base}/collections/tags"),
            "type": "Collection",
            "totalItems": 1,
            "items": [{
                "type": "Hashtag",
                "href": format!("https://{}/@alice/tagged/cats", ctx.domain),
                "name": "#Cats",
            }],
        })
    );

    // A bio without them leaves the account none.
    ctx.api
        .patch_multipart(
            "/api/v1/accounts/update_credentials",
            &ctx.alice_token,
            &[("note", "nothing")],
        )
        .await;
    let actor = ap(&ctx, "/users/alice").await;
    assert!(
        !actor["tag"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["type"] == "Hashtag"),
        "{actor}"
    );
}

/// The instance actor's inbox and outbox are beneath it, as Mastodon's are.
#[tokio::test]
async fn the_instance_actor_has_its_own_outbox() {
    let ctx = TestContext::new("ap-instance-outbox").await;
    let actor = ap(&ctx, "/actor").await;
    let base = format!("https://{}/actor", ctx.domain);
    assert_eq!(
        actor["inbox"].as_str(),
        Some(format!("{base}/inbox").as_str())
    );
    assert_eq!(
        actor["outbox"].as_str(),
        Some(format!("{base}/outbox").as_str())
    );
    let outbox = ap(&ctx, "/actor/outbox").await;
    assert_eq!(
        outbox["id"].as_str(),
        Some(format!("{base}/outbox").as_str())
    );
    assert_eq!(outbox["type"].as_str(), Some("OrderedCollection"));
    assert_eq!(outbox["totalItems"], json!(0));
}

/// A preview card that named alice as its author from a domain she did not
/// list is hers once she lists it.
#[tokio::test]
async fn listing_an_attribution_domain_credits_its_cards() {
    let ctx = TestContext::new("ap-reattribution").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let card: i64 = sqlx::query_scalar(
        r#"INSERT INTO preview_cards (url, title, unverified_author_account_id, created_at, updated_at)
           VALUES ('https://news.blog.example/post', 'A post', $1, now(), now()) RETURNING id"#,
    )
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let resp = ctx
        .api
        .patch_multipart(
            "/api/v1/accounts/update_credentials",
            &ctx.alice_token,
            &[("attribution_domains[]", "blog.example")],
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let (author, unverified): (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT author_account_id, unverified_author_account_id FROM preview_cards WHERE id = $1",
    )
    .bind(card)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!((author, unverified), (Some(alice), None));
}

/// `CreateFeaturedTagService` and `RemoveFeaturedTagService`: an `Add` and a
/// `Remove` of the hashtag to everyone the account reaches, once.
#[tokio::test]
async fn featuring_a_hashtag_is_announced_to_followers() {
    let ctx = TestContext::new("ap-featured-tag-add").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    sqlx::query("UPDATE accounts SET private_key = 'test-private-key' WHERE id = $1")
        .bind(alice_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    let remote_inbox = "https://remote.invalid/users/rob/inbox";
    let remote_id = eunha::snowflake::next_id();
    sqlx::query(
        "INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, shared_inbox_url, created_at, updated_at)
         VALUES ($1, 'rob', 'remote.invalid', 'rob', '', $2, $2, 'k', $3, $2, '', now(), now())",
    )
    .bind(remote_id)
    .bind("https://remote.invalid/users/rob")
    .bind(remote_inbox)
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, now(), now())",
    )
    .bind(remote_id)
    .bind(alice_id)
    .execute(&ctx.db)
    .await
    .unwrap();

    let delivered = |kind: &'static str| {
        let db = ctx.db.clone();
        async move {
            sqlx::query_scalar::<_, Value>(
                "SELECT payload->'activity' FROM eunha.ojak_queue
                 WHERE queue IN ('delivery', 'delivery-priority') AND payload->>'inbox' = $1
                   AND payload->'activity'->>'type' = $2",
            )
            .bind(remote_inbox)
            .bind(kind)
            .fetch_all(&db)
            .await
            .unwrap()
        }
    };

    for _ in 0..2 {
        let resp = ctx
            .api
            .post_json(
                "/api/v1/featured_tags",
                Some(&ctx.alice_token),
                &json!({"name": "Cats"}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
    let adds = delivered("Add").await;
    assert_eq!(adds.len(), 1, "once, when it is new: {adds:?}");
    let base = format!("https://{}/users/alice", ctx.domain);
    assert_eq!(adds[0]["actor"].as_str(), Some(base.as_str()));
    assert_eq!(
        adds[0]["target"].as_str(),
        Some(format!("{base}/collections/featured").as_str())
    );
    assert_eq!(
        adds[0]["object"],
        json!({
            "type": "Hashtag",
            "href": format!("https://{}/@alice/tagged/cats", ctx.domain),
            "name": "#Cats",
        })
    );

    let resp = ctx
        .api
        .post_json(
            "/api/v1/tags/cats/unfeature",
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let removes = delivered("Remove").await;
    assert_eq!(removes.len(), 1, "{removes:?}");
    assert_eq!(removes[0]["object"]["name"].as_str(), Some("#Cats"));
}
