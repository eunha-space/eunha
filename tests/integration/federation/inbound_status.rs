//! What a remote status's `Create` and `Update` make of its attachments and
//! its poll, as `ActivityPub::Activity::Create` and
//! `ProcessStatusUpdateService` read them.

use serde_json::{json, Value};

use crate::helpers::TestContext;

const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

async fn seed_remote(ctx: &TestContext, username: &str, domain: &str) -> (i64, String, String) {
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    let uri = format!("https://{domain}/users/{username}");
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key,
                                 inbox_url, outbox_url, followers_url, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, $5, $4 || '/inbox', $4 || '/outbox',
                   $4 || '/followers', now(), now())"#,
    )
    .bind(id)
    .bind(username)
    .bind(domain)
    .bind(&uri)
    .bind(&pub_pem)
    .execute(&ctx.db)
    .await
    .unwrap();
    (id, uri, priv_pem)
}

async fn send(ctx: &TestContext, sender: &str, key: &str, activity: &Value) {
    let resp = ctx
        .api
        .post_signed("/inbox", activity, &format!("{sender}#main-key"), key)
        .await;
    assert!(resp.status().is_success(), "{}", resp.status());
}

fn create(actor: &str, note: Value) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/activity", note["id"].as_str().unwrap()),
        "type": "Create",
        "actor": actor,
        "object": note,
    })
}

fn update(actor: &str, note: Value, n: u32) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}#updates/{n}", note["id"].as_str().unwrap()),
        "type": "Update",
        "actor": actor,
        "object": note,
    })
}

fn image(name: &str) -> Value {
    json!({
        "type": "Document",
        "mediaType": "image/png",
        "url": format!("https://remote.invalid/media/{name}.png"),
        "name": name,
    })
}

async fn status_id(ctx: &TestContext, uri: &str) -> Option<i64> {
    sqlx::query_scalar("SELECT id FROM statuses WHERE uri = $1")
        .bind(uri)
        .fetch_optional(&ctx.db)
        .await
        .unwrap()
}

/// The attachments' names as the API shows the status.
async fn shown_media(ctx: &TestContext, id: i64) -> Vec<String> {
    let status: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    status["media_attachments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["description"].as_str().unwrap().to_owned())
        .collect()
}

/// `Create` records the attachments in the order the object lists them,
/// at most four; `Update` keeps an attachment it still lists at its URL,
/// records the new order, and leaves one it no longer lists attached.
#[tokio::test]
async fn test_remote_media_keep_the_order_the_object_gives() {
    let ctx = TestContext::new("inbound-media-order").await;
    let (_, remy, key) = seed_remote(&ctx, "remy", "remote.invalid").await;
    let note_uri = format!("{remy}/statuses/1");
    let published = chrono::Utc::now();
    let note = |names: &[&str], updated: Option<String>| {
        json!({
            "id": note_uri, "type": "Note", "attributedTo": remy,
            "content": "<p>pictures</p>", "to": [PUBLIC],
            "cc": [format!("https://{}/users/alice", ctx.domain)],
            "published": published.to_rfc3339(),
            "updated": updated,
            "attachment": names.iter().map(|n| image(n)).collect::<Vec<_>>(),
        })
    };
    send(
        &ctx,
        &remy,
        &key,
        &create(&remy, note(&["z", "a", "m", "b", "extra"], None)),
    )
    .await;
    let id = status_id(&ctx, &note_uri).await.unwrap();
    assert_eq!(shown_media(&ctx, id).await, ["z", "a", "m", "b"]);
    let before: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, description FROM media_attachments WHERE status_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(before.len(), 4);

    let edited = (published + chrono::Duration::minutes(5)).to_rfc3339();
    send(
        &ctx,
        &remy,
        &key,
        &update(&remy, note(&["b", "z"], Some(edited)), 1),
    )
    .await;
    assert_eq!(shown_media(&ctx, id).await, ["b", "z"]);
    let ordered: Vec<i64> =
        sqlx::query_scalar("SELECT ordered_media_attachment_ids FROM statuses WHERE id = $1")
            .bind(id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    let by_name = |name: &str| before.iter().find(|(_, d)| d == name).unwrap().0;
    // The same rows, at their URLs, not new ones.
    assert_eq!(ordered, [by_name("b"), by_name("z")]);
    let attached: i64 =
        sqlx::query_scalar("SELECT count(*) FROM media_attachments WHERE status_id = $1")
            .bind(id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(attached, 4);
}
