//! Outbound dereferencing surfaces: individual status Notes, the followers /
//! following collections, and the featured (pinned) collection.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

/// Post a status as alice and return its id.
async fn post_status(ctx: &TestContext, body: &Value) -> String {
    let resp = ctx
        .api
        .post_json("/api/v1/statuses", Some(&ctx.alice_token), body)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let status: Value = resp.json().await.unwrap();
    status["id"].as_str().expect("status id").to_string()
}

/// FEP-8967: a status with a preview card advertises it as a `Link`
/// attachment, so receivers do not have to scrape the content for a URL.
#[tokio::test]
async fn test_preview_card_is_served_as_a_link_attachment() {
    let ctx = TestContext::new("ap-fep8967").await;
    let id = post_status(
        &ctx,
        &json!({ "status": "look at https://example.test/article", "visibility": "public" }),
    )
    .await;

    // The card itself is fetched in the background from a URL this test cannot
    // reach, so attach one directly — the serialization is what is under test.
    let status_id: i64 = id.parse().unwrap();
    let card_id: i64 = sqlx::query_scalar!(
        r#"INSERT INTO preview_cards (url, title, description, type, created_at, updated_at)
           VALUES ('https://example.test/article', 'An article', '', 0, now(), now())
           RETURNING id"#,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    sqlx::query!(
        "INSERT INTO preview_cards_statuses (status_id, preview_card_id) VALUES ($1, $2)",
        status_id,
        card_id,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let note: Value = ctx
        .api
        .ap_get(&format!("/users/alice/statuses/{id}"), None)
        .await
        .json()
        .await
        .unwrap();

    let attachments = note["attachment"].as_array().expect("attachment array");
    let link = attachments
        .iter()
        .find(|a| a["type"].as_str() == Some("Link"))
        .expect("no Link attachment for the preview card");
    assert_eq!(link["href"].as_str(), Some("https://example.test/article"));
}

/// GET /users/{u}/statuses/{id} serves a Note; /activity serves the Create.
#[tokio::test]
async fn test_status_served_as_ap_note() {
    let ctx = TestContext::new("ap-note").await;
    let id = post_status(
        &ctx,
        &json!({ "status": "hello #world", "visibility": "public" }),
    )
    .await;

    let actor_url = format!("https://{}/users/alice", ctx.domain);
    let note_uri = format!("{actor_url}/statuses/{id}");

    let resp = ctx
        .api
        .ap_get(&format!("/users/alice/statuses/{id}"), None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let note: Value = resp.json().await.unwrap();
    assert_eq!(note["type"].as_str(), Some("Note"));
    assert_eq!(note["id"].as_str(), Some(note_uri.as_str()));
    assert_eq!(note["attributedTo"].as_str(), Some(actor_url.as_str()));
    assert!(note["content"].as_str().unwrap_or("").contains("world"));
    assert!(
        note["to"].as_array().is_some_and(|a| a
            .iter()
            .any(|v| v.as_str() == Some("https://www.w3.org/ns/activitystreams#Public"))),
        "public note should address the Public collection: {note}"
    );
    // The hashtag is carried in the tag array for remote indexing.
    let has_hashtag = note["tag"].as_array().is_some_and(|a| {
        a.iter()
            .any(|t| t["type"].as_str() == Some("Hashtag") && t["name"].as_str() == Some("#world"))
    });
    assert!(has_hashtag, "expected a Hashtag tag: {note}");

    // The /activity wrapper is a Create around the same Note.
    let resp = ctx
        .api
        .ap_get(&format!("/users/alice/statuses/{id}/activity"), None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let create: Value = resp.json().await.unwrap();
    assert_eq!(create["type"].as_str(), Some("Create"));
    assert_eq!(
        create["id"].as_str(),
        Some(format!("{note_uri}/activity").as_str())
    );
    assert_eq!(create["object"]["id"].as_str(), Some(note_uri.as_str()));
}

/// Private statuses are not served over unauthenticated AP GET.
#[tokio::test]
async fn test_private_status_not_dereferenceable() {
    let ctx = TestContext::new("ap-note-priv").await;
    let id = post_status(
        &ctx,
        &json!({ "status": "secret", "visibility": "private" }),
    )
    .await;

    let resp = ctx
        .api
        .ap_get(&format!("/users/alice/statuses/{id}"), None)
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// followers / following collections reflect a local follow.
#[tokio::test]
async fn test_followers_following_collections() {
    let ctx = TestContext::new("ap-rel").await;

    // alice follows bob.
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // bob's followers collection counts alice.
    let followers: Value = ctx
        .api
        .ap_get("/users/bob/followers", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(followers["type"].as_str(), Some("OrderedCollection"));
    assert_eq!(followers["totalItems"].as_i64(), Some(1));

    // The first page, where the collection says it is.
    let first = url::Url::parse(followers["first"].as_str().expect("a first page")).unwrap();
    let first = format!("{}?{}", first.path(), first.query().unwrap_or_default());
    let page: Value = ctx.api.ap_get(&first, None).await.json().await.unwrap();
    let alice_uri = format!("https://{}/users/alice", ctx.domain);
    assert!(
        page["orderedItems"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(alice_uri.as_str()))),
        "alice should appear in bob's followers page: {page}"
    );

    // alice's following collection counts bob.
    let following: Value = ctx
        .api
        .ap_get("/users/alice/following", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(following["totalItems"].as_i64(), Some(1));
}

/// The featured collection lists pinned statuses by URI.
#[tokio::test]
async fn test_featured_collection_lists_pins() {
    let ctx = TestContext::new("ap-featured").await;
    let id = post_status(&ctx, &json!({ "status": "pin me", "visibility": "public" })).await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/pin"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let featured: Value = ctx
        .api
        .ap_get("/users/alice/collections/featured", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(featured["type"].as_str(), Some("OrderedCollection"));
    let note_uri = format!("https://{}/users/alice/statuses/{id}", ctx.domain);
    assert!(
        featured["orderedItems"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(note_uri.as_str()))),
        "pinned status should appear in featured collection: {featured}"
    );
}

/// An account on the numeric scheme advertises its collections beneath
/// `/ap/users/{id}`, and those URLs have to answer with collections.
///
/// They did not. Only the username scheme had routes for `collections/featured`
/// and `collections`, so a peer following the actor's own `featured` link fell
/// through to the SPA fallback and got an HTML page — or a 503 where the
/// frontend was not built. Mastodon fetches both on every federation handshake.
#[tokio::test]
async fn test_numeric_scheme_serves_its_advertised_collections() {
    let ctx = TestContext::new("ap-numeric-collections").await;

    sqlx::query("UPDATE accounts SET id_scheme = 1 WHERE id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();

    let id = post_status(&ctx, &json!({ "status": "pin me", "visibility": "public" })).await;
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/pin"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Whatever the actor says it has, fetched exactly as a peer would.
    let actor: Value = ctx
        .api
        .ap_get(&format!("/ap/users/{}", ctx.alice_id), None)
        .await
        .json()
        .await
        .unwrap();
    let base = format!("https://{}/ap/users/{}", ctx.domain, ctx.alice_id);
    assert_eq!(
        actor["featured"].as_str(),
        Some(format!("{base}/collections/featured").as_str())
    );
    assert_eq!(
        actor["featuredCollections"].as_str(),
        Some(format!("{base}/collections").as_str())
    );

    for path in [
        format!("/ap/users/{}/collections/featured", ctx.alice_id),
        format!("/ap/users/{}/collections", ctx.alice_id),
    ] {
        let resp = ctx.api.ap_get(&path, None).await;
        let status = resp.status();
        // Read as text first: without the route this is the SPA's index.html,
        // and `json()` would panic on the decode rather than say what arrived.
        let text = resp.text().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{path} should answer: {text}");
        let body: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
            panic!("{path} should be a collection, not the web frontend: {text:.120}")
        });
        assert_eq!(
            body["type"].as_str(),
            Some("OrderedCollection"),
            "{path} should be a collection: {body}"
        );
        assert_eq!(
            body["id"].as_str(),
            Some(format!("https://{}{path}", ctx.domain).as_str()),
            "a collection's id is the URL it was advertised at: {body}"
        );
    }

    let featured: Value = ctx
        .api
        .ap_get(
            &format!("/ap/users/{}/collections/featured", ctx.alice_id),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    let note_uri = format!("{base}/statuses/{id}");
    assert!(
        featured["orderedItems"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(note_uri.as_str()))),
        "pinned status should appear, under the numeric scheme: {featured}"
    );
}

/// The actor document exposes profile metadata fields as `PropertyValue`
/// attachments and uses the human `/@username` url, so profile edits federate.
#[tokio::test]
async fn test_actor_serializes_profile_fields() {
    let ctx = TestContext::new("ap-actor-fields").await;

    // A local custom emoji referenced in the display name should federate as a tag.
    sqlx::query(
        "INSERT INTO custom_emojis (id, shortcode, domain, disabled, uri, image_remote_url, created_at, updated_at)
         VALUES (nextval('custom_emojis_id_seq'), 'party', NULL, false, $1, $2, now(), now())",
    )
    .bind(format!("https://{}/emojis/party", ctx.domain))
    .bind(format!("https://{}/party.png", ctx.domain))
    .execute(&ctx.db)
    .await
    .unwrap();

    let resp = ctx
        .api
        .patch_multipart(
            "/api/v1/accounts/update_credentials",
            &ctx.alice_token,
            &[
                ("display_name", "Alice :party:"),
                ("fields_attributes[0][name]", "Website"),
                ("fields_attributes[0][value]", "https://alice.example"),
            ],
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let actor: Value = ctx
        .api
        .ap_get("/users/alice", None)
        .await
        .json()
        .await
        .unwrap();

    assert_eq!(actor["type"].as_str(), Some("Person"));
    assert_eq!(
        actor["url"].as_str(),
        Some(format!("https://{}/@alice", ctx.domain).as_str())
    );

    let attachment = actor["attachment"]
        .as_array()
        .expect("actor should have an attachment array");
    let website = attachment
        .iter()
        .find(|f| f["name"].as_str() == Some("Website"))
        .expect("Website field should be serialized as an attachment");
    assert_eq!(website["type"].as_str(), Some("PropertyValue"));
    // The value is HTML with the URL linkified, mirroring Mastodon.
    assert!(
        website["value"]
            .as_str()
            .is_some_and(|v| v.contains("https://alice.example") && v.contains("<a ")),
        "field value should be linkified HTML: {website}"
    );

    let tag = actor["tag"]
        .as_array()
        .expect("actor should have a tag array");
    let emoji = tag
        .iter()
        .find(|t| t["name"].as_str() == Some(":party:"))
        .expect("custom emoji in display name should be serialized as a tag");
    assert_eq!(emoji["type"].as_str(), Some("Emoji"));
    assert!(emoji["icon"]["url"].as_str().is_some_and(|u| !u.is_empty()));
}

/// A profile update reaches accounts the actor recently followed, not just its
/// own followers — matching Mastodon's `AccountReachFinder`.
#[tokio::test]
async fn test_profile_update_reaches_recently_followed() {
    let ctx = TestContext::new("ap-actor-reach").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();

    // The fanout only runs for accounts that can sign (have a private key).
    sqlx::query("UPDATE accounts SET private_key = 'test-private-key' WHERE id = $1")
        .bind(alice_id)
        .execute(&ctx.db)
        .await
        .unwrap();

    // A remote account alice follows, but which does NOT follow alice back.
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
    .bind(alice_id)
    .bind(remote_id)
    .execute(&ctx.db)
    .await
    .unwrap();

    let resp = ctx
        .api
        .patch_multipart(
            "/api/v1/accounts/update_credentials",
            &ctx.alice_token,
            &[("display_name", "Alice Updated")],
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let delivered: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM eunha.feder_queue
         WHERE queue IN ('delivery', 'delivery-priority') AND payload->>'inbox' = $1
           AND payload->'activity'->'object'->>'type' = 'Person'",
    )
    .bind(remote_inbox)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        delivered, 1,
        "recently-followed remote account should receive the profile Update"
    );
}
