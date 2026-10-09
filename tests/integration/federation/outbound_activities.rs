//! What eunha sends for a boost, a favourite and the undoing of either,
//! named and addressed as Mastodon's serializers write them, so that the
//! servers they reach can match an `Undo` to what it takes back.

use serde_json::{json, Value};

use crate::helpers::TestContext;

const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// A remote account, with a shared inbox besides its own.
async fn seed_remote(ctx: &TestContext, username: &str, domain: &str) -> (i64, String) {
    let uri = format!("https://{domain}/users/{username}");
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key,
                                 inbox_url, shared_inbox_url, outbox_url, protocol, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, '', $4 || '/inbox', 'https://' || $3 || '/inbox',
                   $4 || '/outbox', 1, now(), now())"#,
    )
    .bind(id)
    .bind(username)
    .bind(domain)
    .bind(&uri)
    .execute(&ctx.db)
    .await
    .unwrap();
    (id, uri)
}

/// Alice, of the `numeric_ap_id` scheme Mastodon gives new accounts, with a
/// key and a remote follower; her actor URI.
async fn alice_numeric_with_a_follower(ctx: &TestContext) -> String {
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    sqlx::query(
        "UPDATE accounts SET private_key = $2, public_key = $3, id_scheme = 1 WHERE id = $1",
    )
    .bind(alice_id)
    .bind(&priv_pem)
    .bind(&pub_pem)
    .execute(&ctx.db)
    .await
    .unwrap();
    let (nina_id, _) = seed_remote(ctx, "nina", "nina.invalid").await;
    sqlx::query(
        "INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())",
    )
    .bind(eunha::snowflake::next_id())
    .bind(nina_id)
    .bind(alice_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    format!("https://{}/ap/users/{alice_id}", ctx.domain)
}

/// A public post of rob's, on his server; its id here and its URI.
async fn robs_post(ctx: &TestContext, rob_id: i64, rob: &str) -> (i64, String) {
    let id = eunha::snowflake::next_id();
    let uri = format!("{rob}/statuses/1");
    sqlx::query(
        r#"INSERT INTO statuses (id, account_id, text, spoiler_text, visibility, uri, url, local, created_at, updated_at)
           VALUES ($1, $2, 'robs post', '', 0, $3, $3, false, now(), now())"#,
    )
    .bind(id)
    .bind(rob_id)
    .bind(&uri)
    .execute(&ctx.db)
    .await
    .unwrap();
    (id, uri)
}

/// The activities of a type queued for delivery, each with the inbox it
/// goes to, oldest first.
async fn queued(ctx: &TestContext, kind: &str) -> Vec<(Value, String)> {
    sqlx::query_as(
        "SELECT payload->'activity', payload->>'inbox' FROM eunha.ojak_queue
         WHERE queue IN ('delivery', 'delivery-priority') AND payload->'activity'->>'type' = $1
         ORDER BY id",
    )
    .bind(kind)
    .fetch_all(&ctx.db)
    .await
    .unwrap()
}

fn is_iso8601_seconds(value: &Value) -> bool {
    value.as_str().is_some_and(|time| {
        chrono::NaiveDateTime::parse_from_str(time, "%Y-%m-%dT%H:%M:%SZ").is_ok()
    })
}

/// A boost by an account of the numeric scheme is stored and announced as
/// `TagManager#uri_for` names it, and its `Undo` (`UndoAnnounceSerializer`)
/// names that same `Announce`, to the public, with its audience.
#[tokio::test]
async fn test_a_boost_and_its_undo_name_the_same_announce() {
    let ctx = TestContext::new("outbound-boost").await;
    let alice = alice_numeric_with_a_follower(&ctx).await;
    let (rob_id, rob) = seed_remote(&ctx, "rob", "rob.invalid").await;
    let (post_id, post_uri) = robs_post(&ctx, rob_id, &rob).await;

    let boost = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{post_id}/reblog"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(boost.status(), 200);
    let boost: Value = boost.json().await.unwrap();
    let boost_id = boost["id"].as_str().unwrap().to_owned();
    let activity_uri = format!("{alice}/statuses/{boost_id}/activity");
    assert_eq!(boost["uri"], activity_uri.as_str());
    let stored: Option<String> =
        sqlx::query_scalar("SELECT uri FROM statuses WHERE id = $1::bigint")
            .bind(&boost_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(
        stored.as_deref(),
        Some(activity_uri.as_str()),
        "Status#store_uri"
    );

    let announces = queued(&ctx, "Announce").await;
    let (announce, _) = announces.first().expect("an Announce was queued");
    assert_eq!(announce["id"], activity_uri.as_str());
    assert_eq!(announce["actor"], alice.as_str());
    assert_eq!(announce["object"], post_uri.as_str());
    assert_eq!(announce["to"], json!([PUBLIC]));
    assert_eq!(announce["cc"], json!([rob, format!("{alice}/followers")]));
    assert!(
        is_iso8601_seconds(&announce["published"]),
        "{}",
        announce["published"]
    );

    let undone = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{post_id}/unreblog"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(undone.status(), 200);
    let undos = queued(&ctx, "Undo").await;
    let (undo, _) = undos.first().expect("an Undo was queued");
    assert_eq!(undo["id"], format!("{alice}#announces/{boost_id}/undo"));
    assert_eq!(undo["actor"], alice.as_str());
    assert_eq!(undo["to"], json!([PUBLIC]));
    let undone = &undo["object"];
    assert_eq!(undone["type"], "Announce");
    for key in ["id", "actor", "published", "to", "cc", "object"] {
        assert_eq!(undone[key], announce[key], "{key}");
    }
    assert!(undone.get("@context").is_none());
}

/// `AnnounceNoteSerializer#virtual_object`: an account boosting its own
/// followers-only post sends the post along, since its followers could not
/// fetch it otherwise; the `Undo` names it by its URI.
#[tokio::test]
async fn test_a_self_boost_of_a_private_post_carries_the_note() {
    let ctx = TestContext::new("outbound-self-boost").await;
    let alice = alice_numeric_with_a_follower(&ctx).await;
    let post = ctx
        .api
        .post_status(&ctx.alice_token, "for followers", "private")
        .await;
    let post_id = post["id"].as_str().unwrap();
    let post_uri = post["uri"].as_str().unwrap();
    assert!(post_uri.starts_with(&alice), "{post_uri}");

    let boost = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{post_id}/reblog"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(boost.status(), 200);
    let announces = queued(&ctx, "Announce").await;
    let (announce, _) = announces.first().expect("an Announce was queued");
    assert_eq!(announce["object"]["type"], "Note");
    assert_eq!(announce["object"]["id"], post_uri);
    assert_eq!(announce["to"], json!([format!("{alice}/followers")]));
    assert_eq!(announce["cc"], json!([alice]));

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{post_id}/unreblog"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    let undos = queued(&ctx, "Undo").await;
    let (undo, _) = undos.first().expect("an Undo was queued");
    assert_eq!(undo["object"]["object"], post_uri);
}

/// Migration 036 gives each local boost eunha left without a `uri` the one
/// Mastodon stores, in its account's scheme, on the domain of the local post
/// nearest it; a boost that has one keeps it.
#[tokio::test]
async fn test_migration_036_names_local_boosts() {
    let ctx = TestContext::new("boost-uri-backfill").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    let bob_id: i64 = ctx.bob_id.parse().unwrap();
    sqlx::query("UPDATE accounts SET id_scheme = 1 WHERE id = $1")
        .bind(alice_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET id_scheme = 0 WHERE id = $1")
        .bind(bob_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    let post = ctx
        .api
        .post_status(&ctx.bob_token, "boost me", "public")
        .await;
    let post_id: i64 = post["id"].as_str().unwrap().parse().unwrap();
    let mut boosts = Vec::new();
    for (account_id, uri) in [
        (alice_id, None),
        (bob_id, None),
        (alice_id, Some("https://elsewhere.invalid/kept")),
    ] {
        let id = eunha::snowflake::next_id();
        sqlx::query(
            "INSERT INTO statuses (id, account_id, text, visibility, reblog_of_id, local, uri,
                                   created_at, updated_at)
             VALUES ($1, $2, '', 0, $3, true, $4, now(), now())",
        )
        .bind(id)
        .bind(account_id)
        .bind(post_id)
        .bind(uri)
        .execute(&ctx.db)
        .await
        .unwrap();
        boosts.push(id);
    }

    sqlx::raw_sql(include_str!("../../../migrations/036_local_boost_uris.sql"))
        .execute(&ctx.db)
        .await
        .unwrap();

    let uris: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT uri FROM statuses WHERE id = ANY($1) ORDER BY array_position($1, id)",
    )
    .bind(&boosts)
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    let domain = &ctx.domain;
    assert_eq!(
        uris,
        vec![
            Some(format!(
                "https://{domain}/ap/users/{alice_id}/statuses/{}/activity",
                boosts[0]
            )),
            Some(format!(
                "https://{domain}/users/bob/statuses/{}/activity",
                boosts[1]
            )),
            Some("https://elsewhere.invalid/kept".to_owned()),
        ]
    );
}
