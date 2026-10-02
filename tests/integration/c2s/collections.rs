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

    // Revoke the item.
    let revoke = ctx
        .api
        .post_json(
            &format!("/api/v1/collections/{cid}/items/{item_id}/revoke"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(revoke.status(), StatusCode::OK);

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

    // Delete the item row entirely.
    let del = ctx
        .api
        .delete(
            &format!("/api/v1/collections/{cid}/items/{item_id}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(del.status(), StatusCode::OK);
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

    // Actor advertises its collections endpoint.
    let actor: Value = ctx
        .api
        .ap_get("/users/alice", None)
        .await
        .json()
        .await
        .unwrap();
    let featured = actor["featuredCollections"]
        .as_str()
        .expect("featuredCollections link");
    assert!(
        featured.ends_with("/users/alice/collections"),
        "got {featured}"
    );

    // The account collections OrderedCollection lists the collection URI.
    let oc: Value = ctx
        .api
        .ap_get("/users/alice/collections", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(oc["type"].as_str(), Some("OrderedCollection"));
    let uris = oc["orderedItems"].as_array().unwrap();
    assert!(
        uris.iter().any(|u| u
            .as_str()
            .is_some_and(|s| s.ends_with(&format!("/collections/{cid}")))),
        "collection URI missing from account collections: {oc:?}",
    );

    // The FeaturedCollection object itself.
    let obj: Value = ctx
        .api
        .ap_get(&format!("/collections/{cid}"), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(obj["type"].as_str(), Some("FeaturedCollection"));
    assert_eq!(obj["name"].as_str(), Some("AP collection"));
    assert_eq!(obj["totalItems"].as_i64(), Some(1));
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
