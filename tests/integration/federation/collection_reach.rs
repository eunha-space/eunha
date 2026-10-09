//! Who a local collection's activities go to, as Mastodon sends them: the
//! `Add` and `Update` of a collection, and the `Add` and `Remove` of an item,
//! to the collection's reach (`CollectionRawDistributionWorker`,
//! `CollectionReachFinder`: its owner's reach and the accounts it features);
//! the `Remove` of a deleted collection to its owner's reach
//! (`AccountRawDistributionWorker`, `AccountReachFinder`).

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{make_admin, TestContext};

/// The remote accounts around alice's collection: one following her, one
/// that reported her, both in her reach, and one she features and nothing
/// else.
struct Around {
    follower: String,
    reporter: String,
    member: String,
    member_id: i64,
}

async fn remote(ctx: &TestContext, username: &str, domain: &str) -> (i64, String) {
    let uri = format!("https://{domain}/users/{username}");
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, inbox_url,
                                 outbox_url, protocol, discoverable, feature_approval_policy,
                                 created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, $4 || '/inbox', $4 || '/outbox', 1, true, $5,
                   now(), now())"#,
    )
    .bind(id)
    .bind(username)
    .bind(domain)
    .bind(&uri)
    .bind(eunha::db::models::feature_policy::PUBLIC << 16)
    .execute(&ctx.db)
    .await
    .unwrap();
    (id, format!("{uri}/inbox"))
}

async fn around(ctx: &TestContext) -> Around {
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let (private_pem, public_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    sqlx::query("UPDATE accounts SET private_key = $2, public_key = $3 WHERE id = $1")
        .bind(alice)
        .bind(private_pem)
        .bind(public_pem)
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET discoverable = true WHERE id = $1")
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let (follower_id, follower) = remote(ctx, "fran", "follower.invalid").await;
    let (reporter_id, reporter) = remote(ctx, "rex", "reporter.invalid").await;
    let (member_id, member) = remote(ctx, "mia", "member.invalid").await;
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, now(), now())",
    )
    .bind(follower_id)
    .bind(alice)
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO reports (account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, now(), now())",
    )
    .bind(reporter_id)
    .bind(alice)
    .execute(&ctx.db)
    .await
    .unwrap();
    Around {
        follower,
        reporter,
        member,
        member_id,
    }
}

/// The inboxes an activity of `kind` whose object matches `object` was
/// queued for, sorted.
async fn queued(ctx: &TestContext, kind: &str, object: impl Fn(&Value) -> bool) -> Vec<String> {
    let rows: Vec<(Value, String)> = sqlx::query_as(
        "SELECT payload->'activity', payload->>'inbox' FROM eunha.ojak_queue
         WHERE queue IN ('delivery', 'delivery-priority')
           AND payload->'activity'->>'type' = $1",
    )
    .bind(kind)
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    let mut inboxes: Vec<String> = rows
        .into_iter()
        .filter(|(activity, _)| object(&activity["object"]))
        .map(|(_, inbox)| inbox)
        .collect();
    inboxes.sort();
    inboxes
}

fn sorted(inboxes: &[&String]) -> Vec<String> {
    let mut inboxes: Vec<String> = inboxes.iter().map(|i| (*i).clone()).collect();
    inboxes.sort();
    inboxes
}

fn is_collection(object: &Value) -> bool {
    object["type"] == "FeaturedCollection"
}

async fn create(ctx: &TestContext, account_ids: Value) -> String {
    let resp = ctx
        .api
        .post_json(
            "/api/v1/collections",
            Some(&ctx.alice_token),
            &json!({"name": "Featured", "account_ids": account_ids}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    body["collection"]["id"].as_str().unwrap().to_owned()
}

/// `CreateCollectionService` and `UpdateCollectionService`: the `Add` and the
/// `Update` go to the owner's reach and the accounts featured, pending ones
/// too; an update that changes nothing is not sent.
#[tokio::test]
async fn test_collection_add_and_update_go_to_the_collections_reach() {
    let ctx = TestContext::new("coll-reach-add").await;
    let a = around(&ctx).await;
    let cid = create(&ctx, json!([a.member_id.to_string(), ctx.bob_id])).await;
    let reach = sorted(&[&a.follower, &a.member, &a.reporter]);
    assert_eq!(queued(&ctx, "Add", is_collection).await, reach);

    let update = |body: Value| {
        let api = &ctx.api;
        let path = format!("/api/v1/collections/{cid}");
        let token = ctx.alice_token.clone();
        async move {
            let resp = api.put_json(&path, Some(&token), &body).await;
            assert_eq!(resp.status(), StatusCode::OK);
        }
    };
    // `relevant_attributes_changed?`: nothing did.
    update(json!({"name": "Featured"})).await;
    assert!(queued(&ctx, "Update", is_collection).await.is_empty());
    update(json!({"name": "Renamed"})).await;
    assert_eq!(queued(&ctx, "Update", is_collection).await, reach);
}

/// `AddAccountToCollectionService` and `DeleteCollectionItemService`: a
/// local account's item comes and goes to the collection's reach, a
/// suspended featured account included.
#[tokio::test]
async fn test_item_add_and_remove_go_to_the_collections_reach() {
    let ctx = TestContext::new("coll-reach-items").await;
    let a = around(&ctx).await;
    let cid = create(&ctx, json!([a.member_id.to_string()])).await;
    // `@collection.accounts.inboxes` asks nothing of a suspension.
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(a.member_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/collections/{cid}/items"),
            Some(&ctx.alice_token),
            &json!({"account_id": ctx.bob_id}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let item_id = body["collection_item"]["id"].as_str().unwrap().to_owned();
    let reach = sorted(&[&a.follower, &a.member, &a.reporter]);
    let is_item = |object: &Value| object["type"] == "FeaturedItem";
    assert_eq!(queued(&ctx, "Add", is_item).await, reach);

    let resp = ctx
        .api
        .delete(
            &format!("/api/v1/collections/{cid}/items/{item_id}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let item_uri = |object: &Value| {
        object
            .as_str()
            .is_some_and(|uri| uri.ends_with(&format!("/collection_items/{item_id}")))
    };
    assert_eq!(queued(&ctx, "Remove", item_uri).await, reach);
}

/// `DeleteCollectionService`: the `Remove` goes to the owner's reach
/// (`AccountRawDistributionWorker`), not to the accounts it featured.
#[tokio::test]
async fn test_collection_removal_goes_to_the_owners_reach() {
    let ctx = TestContext::new("coll-reach-remove").await;
    let a = around(&ctx).await;
    let cid = create(&ctx, json!([a.member_id.to_string()])).await;
    let resp = ctx
        .api
        .delete(&format!("/api/v1/collections/{cid}"), &ctx.alice_token)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let is_this = |object: &Value| {
        object
            .as_str()
            .is_some_and(|uri| uri.ends_with(&format!("/collections/{cid}")))
    };
    assert_eq!(
        queued(&ctx, "Remove", is_this).await,
        sorted(&[&a.follower, &a.reporter])
    );
}

/// `Admin::ModerationAction`: a reported collection marked sensitive is
/// updated as its owner's update would be, once; one deleted is removed
/// from its owner's reach, which upstream does not send
/// (`moderated-collection-removal`).
#[tokio::test]
async fn test_moderated_collections_reach() {
    let ctx = TestContext::new("coll-reach-moderated").await;
    let a = around(&ctx).await;
    let bob: i64 = ctx.bob_id.parse().unwrap();
    make_admin(&ctx.db, bob).await;
    let cid = create(&ctx, json!([a.member_id.to_string()])).await;
    let report = || async {
        let report_id: i64 = sqlx::query_scalar(
            "INSERT INTO reports (account_id, target_account_id, created_at, updated_at)
             VALUES ($1, $2, now(), now()) RETURNING id",
        )
        .bind(bob)
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO collection_reports (collection_id, report_id, created_at, updated_at)
             VALUES ($1, $2, now(), now())",
        )
        .bind(cid.parse::<i64>().unwrap())
        .bind(report_id)
        .execute(&ctx.db)
        .await
        .unwrap();
        report_id
    };
    let act = |report_id: i64, kind: &'static str| {
        let state = ctx.state.clone();
        async move {
            eunha::moderation::moderation_action::save(&state, bob, report_id, kind, None, false)
                .await
                .unwrap();
        }
    };

    act(report().await, "mark_as_sensitive").await;
    let reach = sorted(&[&a.follower, &a.member, &a.reporter]);
    assert_eq!(queued(&ctx, "Update", is_collection).await, reach);
    // Already sensitive: `relevant_attributes_changed?` says no.
    act(report().await, "mark_as_sensitive").await;
    assert_eq!(queued(&ctx, "Update", is_collection).await, reach);

    act(report().await, "delete").await;
    let is_this = |object: &Value| {
        object
            .as_str()
            .is_some_and(|uri| uri.ends_with(&format!("/collections/{cid}")))
    };
    assert_eq!(
        queued(&ctx, "Remove", is_this).await,
        sorted(&[&a.follower, &a.reporter])
    );
}
