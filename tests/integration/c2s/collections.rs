use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

/// Create a collection and read it back through the account index and show.
#[tokio::test]
async fn test_create_show_and_list_collections() {
    let ctx = TestContext::new("coll-create").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/collections",
            Some(&ctx.alice_token),
            &json!({"name": "Cool people", "description": "a list", "discoverable": true}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let c = &body["collection"];
    assert_eq!(c["name"].as_str(), Some("Cool people"));
    assert_eq!(c["local"].as_bool(), Some(true));
    assert_eq!(c["item_count"].as_i64(), Some(0));
    assert!(c["id"].as_str().is_some(), "id should be a string");
    assert_eq!(c["account_id"].as_str(), Some(ctx.alice_id.as_str()));
    let cid = c["id"].as_str().unwrap().to_string();

    // Account index (root-wrapped under "collections").
    let list: Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/collections", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        list["collections"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"].as_str() == Some(cid.as_str())),
        "created collection missing from account index: {list:?}",
    );

    // Show returns {collection, accounts:[owner, ...]}.
    let show: Value = ctx
        .api
        .get(
            &format!("/api/v1/collections/{cid}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(show["collection"]["id"].as_str(), Some(cid.as_str()));
    let accounts = show["accounts"].as_array().unwrap();
    assert!(
        accounts
            .iter()
            .any(|a| a["id"].as_str() == Some(ctx.alice_id.as_str())),
        "owner account missing from show accounts",
    );
}

/// Creating a collection requires auth.
#[tokio::test]
async fn test_create_collection_requires_auth() {
    let ctx = TestContext::new("coll-auth").await;
    let resp = ctx
        .api
        .post_json("/api/v1/collections", None, &json!({"name": "x"}))
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// A blank name is rejected with 422.
#[tokio::test]
async fn test_create_collection_blank_name() {
    let ctx = TestContext::new("coll-blank").await;
    let resp = ctx
        .api
        .post_json(
            "/api/v1/collections",
            Some(&ctx.alice_token),
            &json!({"name": "   "}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Add a local account to a collection (auto-accepted), see it in items and
/// in the target's in_collections, then revoke and delete it.
#[tokio::test]
async fn test_add_revoke_delete_item() {
    let ctx = TestContext::new("coll-items").await;

    let c: Value = ctx
        .api
        .post_json(
            "/api/v1/collections",
            Some(&ctx.alice_token),
            &json!({"name": "Featured"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let cid = c["collection"]["id"].as_str().unwrap().to_string();

    // Add bob (local) -> accepted.
    let add: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/collections/{cid}/items"),
            Some(&ctx.alice_token),
            &json!({"account_id": ctx.bob_id}),
        )
        .await
        .json()
        .await
        .unwrap();
    let item = &add["collection_item"];
    assert_eq!(item["state"].as_str(), Some("accepted"));
    assert_eq!(item["account_id"].as_str(), Some(ctx.bob_id.as_str()));
    let item_id = item["id"].as_str().unwrap().to_string();

    // bob shows up in alice's collection items + item_count is 1.
    let show: Value = ctx
        .api
        .get(
            &format!("/api/v1/collections/{cid}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(show["collection"]["item_count"].as_i64(), Some(1));

    // bob's in_collections includes this collection.
    let in_colls: Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/in_collections", ctx.bob_id),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        in_colls["collections"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"].as_str() == Some(cid.as_str())),
        "collection missing from bob's in_collections: {in_colls:?}",
    );

    // Only the account it features may revoke it (`CollectionItemPolicy`):
    // not even the collection's owner.
    let revoke = |token: String| {
        let api = &ctx.api;
        let path = format!("/api/v1/collections/{cid}/items/{item_id}/revoke");
        async move { api.post_json(&path, Some(&token), &json!({})).await }
    };
    assert_eq!(
        revoke(ctx.alice_token.clone()).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(revoke(ctx.bob_token.clone()).await.status(), StatusCode::OK);
    // From a local collection, nothing is sent.
    let sent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM eunha.ojak_queue WHERE queue IN ('delivery', 'delivery-priority')",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(sent, 0);

    // Revoked item no longer counts.
    let show2: Value = ctx
        .api
        .get(
            &format!("/api/v1/collections/{cid}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(show2["collection"]["item_count"].as_i64(), Some(0));

    // The `item_count` column is Mastodon's counter cache: the revoked item
    // still counts until it is destroyed.
    let column = || async {
        sqlx::query_scalar::<_, i32>("SELECT item_count FROM collections WHERE id = $1")
            .bind(cid.parse::<i64>().unwrap())
            .fetch_one(&ctx.db)
            .await
            .unwrap()
    };
    assert_eq!(column().await, 1);

    // Delete the item row entirely.
    let del = ctx
        .api
        .delete(
            &format!("/api/v1/collections/{cid}/items/{item_id}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(del.status(), StatusCode::OK);
    assert_eq!(column().await, 0);
}

/// Only the owner may update or delete a collection.
#[tokio::test]
async fn test_update_and_ownership() {
    let ctx = TestContext::new("coll-owner").await;

    let c: Value = ctx
        .api
        .post_json(
            "/api/v1/collections",
            Some(&ctx.alice_token),
            &json!({"name": "Mine"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let cid = c["collection"]["id"].as_str().unwrap().to_string();

    // Bob cannot update alice's collection.
    let bob_update = ctx
        .api
        .put_json(
            &format!("/api/v1/collections/{cid}"),
            Some(&ctx.bob_token),
            &json!({"name": "Hijacked"}),
        )
        .await;
    assert_eq!(bob_update.status(), StatusCode::FORBIDDEN);

    // Alice can.
    let alice_update: Value = ctx
        .api
        .put_json(
            &format!("/api/v1/collections/{cid}"),
            Some(&ctx.alice_token),
            &json!({"name": "Renamed", "discoverable": true}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(alice_update["collection"]["name"].as_str(), Some("Renamed"));
    assert_eq!(
        alice_update["collection"]["discoverable"].as_bool(),
        Some(true)
    );

    // Bob cannot delete it.
    let bob_delete = ctx
        .api
        .delete(&format!("/api/v1/collections/{cid}"), &ctx.bob_token)
        .await;
    assert_eq!(bob_delete.status(), StatusCode::FORBIDDEN);

    // Alice can.
    let alice_delete = ctx
        .api
        .delete(&format!("/api/v1/collections/{cid}"), &ctx.alice_token)
        .await;
    assert_eq!(alice_delete.status(), StatusCode::OK);

    // Gone now.
    let show = ctx
        .api
        .get(
            &format!("/api/v1/collections/{cid}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(show.status(), StatusCode::NOT_FOUND);
}

/// Collections are exposed over ActivityPub: the actor links to its
/// collections, and each collection is fetchable as a FeaturedCollection.
#[tokio::test]
async fn test_collection_activitypub_representation() {
    let ctx = TestContext::new("coll-ap").await;

    let c: Value = ctx
        .api
        .post_json(
            "/api/v1/collections",
            Some(&ctx.alice_token),
            &json!({"name": "AP collection", "discoverable": true}),
        )
        .await
        .json()
        .await
        .unwrap();
    let cid = c["collection"]["id"].as_str().unwrap().to_string();

    // Add bob (local) so the collection has an accepted item.
    ctx.api
        .post_json(
            &format!("/api/v1/collections/{cid}/items"),
            Some(&ctx.alice_token),
            &json!({"account_id": ctx.bob_id}),
        )
        .await;

    // The actor names its collections where Mastodon serves them, under
    // `/ap/users/{id}` whichever scheme it uses.
    let actor: Value = ctx
        .api
        .ap_get("/users/alice", None)
        .await
        .json()
        .await
        .unwrap();
    let base = format!("https://{}/ap/users/{}", ctx.domain, ctx.alice_id);
    let collection_uri = format!("{base}/collections/{cid}");
    assert_eq!(
        actor["featuredCollections"].as_str(),
        Some(format!("{base}/featured_collections").as_str())
    );

    // `ActivityPub::FeaturedCollectionsController`: the count and the first
    // page, and the page embedding the collection.
    let index: Value = ctx
        .api
        .ap_get(
            &format!("/ap/users/{}/featured_collections", ctx.alice_id),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(index["type"].as_str(), Some("Collection"), "{index}");
    assert_eq!(index["totalItems"].as_i64(), Some(1));
    assert_eq!(
        index["first"].as_str(),
        Some(format!("{base}/featured_collections?page=1").as_str())
    );
    let page: Value = ctx
        .api
        .ap_get(
            &format!("/ap/users/{}/featured_collections?page=1", ctx.alice_id),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(page["type"].as_str(), Some("CollectionPage"), "{page}");
    assert_eq!(
        page["partOf"].as_str(),
        Some(format!("{base}/featured_collections").as_str())
    );
    assert_eq!(
        page["items"][0]["id"].as_str(),
        Some(collection_uri.as_str())
    );
    assert!(page.get("next").is_none(), "one page: {page}");

    // Where eunha used to say they were still answers.
    let oc: Value = ctx
        .api
        .ap_get("/users/alice/collections", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        oc["orderedItems"][0].as_str(),
        Some(collection_uri.as_str())
    );

    // The FeaturedCollection object itself, at its page and at its URI.
    for path in [
        format!("/collections/{cid}"),
        format!("/ap/users/{}/collections/{cid}", ctx.alice_id),
    ] {
        let obj: Value = ctx.api.ap_get(&path, None).await.json().await.unwrap();
        assert_eq!(obj["id"].as_str(), Some(collection_uri.as_str()), "{obj}");
        assert_eq!(obj["type"].as_str(), Some("FeaturedCollection"));
        assert_eq!(
            obj["url"].as_str(),
            Some(format!("https://{}/collections/{cid}", ctx.domain).as_str())
        );
        assert_eq!(obj["name"].as_str(), Some("AP collection"));
        assert_eq!(obj["totalItems"].as_i64(), Some(1));
        assert!(
            obj["@context"][1]["FeaturedCollection"].is_string(),
            "{obj}"
        );
        let items = obj["orderedItems"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"].as_str(), Some("FeaturedItem"));
        assert!(
            items[0]["featuredObject"]
                .as_str()
                .is_some_and(|s| s.ends_with("/users/bob")),
            "featuredObject should point at bob's actor: {:?}",
            items[0],
        );
        // `CollectionItemsController`: the item at its own URI.
        let item_uri = items[0]["id"].as_str().unwrap().to_owned();
        assert!(item_uri.starts_with(&format!("{base}/collection_items/")));
        let path = item_uri.trim_start_matches(&format!("https://{}", ctx.domain));
        let item: Value = ctx.api.ap_get(path, None).await.json().await.unwrap();
        assert_eq!(item["id"].as_str(), Some(item_uri.as_str()), "{item}");
        assert_eq!(item["type"].as_str(), Some("FeaturedItem"));
        // A local account's consent is a stamp at its own URI.
        let stamp = item["featureAuthorization"].as_str().unwrap().to_owned();
        let path = stamp.trim_start_matches(&format!("https://{}", ctx.domain));
        let authorization: Value = ctx.api.ap_get(path, None).await.json().await.unwrap();
        assert_eq!(
            authorization["id"].as_str(),
            Some(stamp.as_str()),
            "{authorization}"
        );
        assert_eq!(
            authorization["interactingObject"].as_str(),
            Some(collection_uri.as_str())
        );
    }
    let elsewhere = ctx
        .api
        .ap_get(&format!("/ap/users/{}/collections/{cid}", ctx.bob_id), None)
        .await;
    assert_eq!(
        elsewhere.status(),
        StatusCode::NOT_FOUND,
        "a collection is only beneath its owner"
    );
}

/// Non-discoverable collections are hidden from other users' account index.
#[tokio::test]
async fn test_discoverable_visibility() {
    let ctx = TestContext::new("coll-disc").await;

    let c: Value = ctx
        .api
        .post_json(
            "/api/v1/collections",
            Some(&ctx.alice_token),
            &json!({"name": "Secret", "discoverable": false}),
        )
        .await
        .json()
        .await
        .unwrap();
    let cid = c["collection"]["id"].as_str().unwrap().to_string();

    // Bob (not the owner) should not see a non-discoverable collection.
    let bob_view: Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/collections", ctx.alice_id),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !bob_view["collections"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"].as_str() == Some(cid.as_str())),
        "non-discoverable collection leaked to another user",
    );

    // The owner still sees it.
    let alice_view: Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/collections", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        alice_view["collections"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"].as_str() == Some(cid.as_str())),
        "owner cannot see own non-discoverable collection",
    );
}

/// Mastodon serves collections under `/api/v1_alpha` as well.
#[tokio::test]
async fn test_collections_under_v1_alpha() {
    let ctx = TestContext::new("coll-alpha").await;
    let body: Value = ctx
        .api
        .post_json(
            "/api/v1_alpha/collections",
            Some(&ctx.alice_token),
            &json!({"name": "Alpha", "discoverable": true}),
        )
        .await
        .json()
        .await
        .unwrap();
    let id = body["collection"]["id"].as_str().unwrap().to_owned();
    let shown = ctx
        .api
        .get(&format!("/api/v1_alpha/collections/{id}"), None)
        .await;
    assert_eq!(shown.status(), StatusCode::OK);
    let listed = ctx
        .api
        .get(
            &format!("/api/v1_alpha/accounts/{}/collections", ctx.alice_id),
            None,
        )
        .await;
    assert_eq!(listed.status(), StatusCode::OK);
}

/// `NotifyService` drops a notification when its recipient blocks the
/// sender, and only then: a block the other way does not keep it from them.
#[tokio::test]
async fn test_only_the_recipients_block_drops_a_notification() {
    let ctx = TestContext::new("coll-notify-blocks").await;
    let body: Value = ctx
        .api
        .post_json(
            "/api/v1/collections",
            Some(&ctx.alice_token),
            &json!({"name": "Friends", "account_ids": [ctx.bob_id]}),
        )
        .await
        .json()
        .await
        .unwrap();
    let item_id: i64 = body["collection"]["items"][0]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    let renotify = || async {
        sqlx::query("DELETE FROM notifications WHERE account_id = $1")
            .bind(bob)
            .execute(&ctx.db)
            .await
            .unwrap();
        eunha::push::notify_collection(
            &ctx.state,
            bob,
            "added_to_collection",
            ("CollectionItem", item_id),
            alice,
        )
        .await;
        let (count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM notifications WHERE account_id = $1 AND type = 'added_to_collection'",
        )
        .bind(bob)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
        count
    };
    // The sender blocking the recipient is no reason to drop it.
    block(&ctx.db, alice, bob).await;
    assert_eq!(renotify().await, 1);
    // The recipient blocking the sender is.
    block(&ctx.db, bob, alice).await;
    assert_eq!(renotify().await, 0);
}

async fn block(db: &sqlx::PgPool, from: i64, to: i64) {
    sqlx::query(
        "INSERT INTO blocks (account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, now(), now())",
    )
    .bind(from)
    .bind(to)
    .execute(db)
    .await
    .unwrap();
}

/// bob's notifications of `kind`, from the v1 list.
async fn bobs_notifications(ctx: &TestContext, kind: &str) -> Vec<Value> {
    ctx.api
        .get(
            &format!("/api/v1/notifications?types[]={kind}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap()
}

async fn update_collection(ctx: &TestContext, cid: &str, body: Value) {
    let resp = ctx
        .api
        .put_json(
            &format!("/api/v1/collections/{cid}"),
            Some(&ctx.alice_token),
            &body,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// A local account added to a collection is told so (`added_to_collection`),
/// and told again, replacing the last, when the collection's name,
/// description, sensitivity or topic changes (`collection_update`); both
/// carry the collection, and the update goes with the collection.
#[tokio::test]
async fn test_collection_notifications() {
    let ctx = TestContext::new("coll-notifications").await;
    let resp = ctx
        .api
        .post_json(
            "/api/v1/collections",
            Some(&ctx.alice_token),
            &json!({"name": "Friends", "account_ids": [ctx.bob_id]}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let cid = body["collection"]["id"].as_str().unwrap().to_owned();

    let added = bobs_notifications(&ctx, "added_to_collection").await;
    assert_eq!(added.len(), 1, "{added:?}");
    assert_eq!(added[0]["account"]["id"], ctx.alice_id.as_str());
    assert_eq!(added[0]["collection"]["id"], cid.as_str());
    assert!(added[0].get("status").is_none());
    assert!(bobs_notifications(&ctx, "collection_update")
        .await
        .is_empty());

    // Not significant.
    update_collection(&ctx, &cid, json!({"discoverable": true})).await;
    assert!(bobs_notifications(&ctx, "collection_update")
        .await
        .is_empty());

    update_collection(&ctx, &cid, json!({"name": "Best friends"})).await;
    let first = bobs_notifications(&ctx, "collection_update").await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["collection"]["name"], "Best friends");
    update_collection(&ctx, &cid, json!({"sensitive": true})).await;
    let second = bobs_notifications(&ctx, "collection_update").await;
    assert_eq!(second.len(), 1, "the newer replaces the older");
    assert_ne!(second[0]["id"], first[0]["id"]);

    // Grouped, as `REST::NotificationGroupSerializer` has it.
    let grouped: Value = ctx
        .api
        .get(
            "/api/v2/notifications?types[]=collection_update",
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        grouped["notification_groups"][0]["collection"]["id"],
        cid.as_str(),
        "{grouped}"
    );

    let resp = ctx
        .api
        .delete(&format!("/api/v1/collections/{cid}"), &ctx.alice_token)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(bobs_notifications(&ctx, "collection_update")
        .await
        .is_empty());
    // The item's notification stays, its collection gone.
    let added = bobs_notifications(&ctx, "added_to_collection").await;
    assert_eq!(added.len(), 1);
    assert!(added[0]["collection"].is_null(), "{added:?}");
}
