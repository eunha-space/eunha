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

fn question(ctx: &TestContext, uri: &str, actor: &str, options: &[&str], extra: Value) -> Value {
    let mut note = json!({
        "id": uri, "type": "Question", "attributedTo": actor,
        "content": "<p>which?</p>", "to": [PUBLIC],
        "cc": [format!("https://{}/users/alice", ctx.domain)],
        "published": "2026-01-01T00:00:00Z",
        "oneOf": options.iter().map(|o| json!({"type": "Note", "name": o})).collect::<Vec<_>>(),
    });
    if let (Some(note), Some(extra)) = (note.as_object_mut(), extra.as_object()) {
        for (k, v) in extra {
            note.insert(k.clone(), v.clone());
        }
    }
    note
}

async fn poll_options(ctx: &TestContext, status_id: i64) -> Option<Vec<String>> {
    sqlx::query_scalar("SELECT options FROM polls WHERE status_id = $1")
        .bind(status_id)
        .fetch_optional(&ctx.db)
        .await
        .unwrap()
}

/// A `Question` with no option makes a poll that is not valid, which the
/// status saves with it: `Status.create!` raises and the status is not
/// taken, and an edit to one is not kept. An update that is not an edit
/// leaves a poll's options as they were.
#[tokio::test]
async fn test_a_poll_with_no_options_is_refused_with_its_status() {
    let ctx = TestContext::new("inbound-empty-poll").await;
    let (_, remy, key) = seed_remote(&ctx, "remy", "remote.invalid").await;

    let empty = format!("{remy}/statuses/empty");
    send(
        &ctx,
        &remy,
        &key,
        &create(&remy, question(&ctx, &empty, &remy, &[], json!({}))),
    )
    .await;
    assert_eq!(status_id(&ctx, &empty).await, None);

    let poll = format!("{remy}/statuses/poll");
    send(
        &ctx,
        &remy,
        &key,
        &create(&remy, question(&ctx, &poll, &remy, &["a", "b"], json!({}))),
    )
    .await;
    let id = status_id(&ctx, &poll).await.unwrap();

    // An edit to no options is refused whole: its text is not taken either.
    send(
        &ctx,
        &remy,
        &key,
        &update(
            &remy,
            question(
                &ctx,
                &poll,
                &remy,
                &[],
                json!({"content": "<p>edited</p>", "updated": "2026-01-02T00:00:00Z"}),
            ),
            1,
        ),
    )
    .await;
    let text: String = sqlx::query_scalar("SELECT text FROM statuses WHERE id = $1")
        .bind(id)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(text, "<p>which?</p>");
    assert_eq!(
        poll_options(&ctx, id).await,
        Some(vec!["a".into(), "b".into()])
    );

    // An update that is not an edit does not change the options.
    send(
        &ctx,
        &remy,
        &key,
        &update(
            &remy,
            question(&ctx, &poll, &remy, &["c", "d"], json!({})),
            2,
        ),
    )
    .await;
    assert_eq!(
        poll_options(&ctx, id).await,
        Some(vec!["a".into(), "b".into()])
    );

    // An edit that does replaces them, and its votes go.
    send(
        &ctx,
        &remy,
        &key,
        &update(
            &remy,
            question(
                &ctx,
                &poll,
                &remy,
                &["c", "d"],
                json!({"updated": "2026-01-03T00:00:00Z"}),
            ),
            3,
        ),
    )
    .await;
    assert_eq!(
        poll_options(&ctx, id).await,
        Some(vec!["c".into(), "d".into()])
    );
}

/// `ProcessStatusUpdateService#update_poll!`: an edit that is no longer a
/// `Question` destroys the poll the status had, its votes with it; an update
/// that is not an edit leaves it.
#[tokio::test]
async fn test_an_edit_that_is_no_longer_a_poll_destroys_it() {
    let ctx = TestContext::new("inbound-poll-gone").await;
    let (_, remy, key) = seed_remote(&ctx, "remy", "remote.invalid").await;
    let poll = format!("{remy}/statuses/poll");
    send(
        &ctx,
        &remy,
        &key,
        &create(&remy, question(&ctx, &poll, &remy, &["a", "b"], json!({}))),
    )
    .await;
    let id = status_id(&ctx, &poll).await.unwrap();
    let poll_id: i64 = sqlx::query_scalar("SELECT id FROM polls WHERE status_id = $1")
        .bind(id)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO poll_votes (account_id, poll_id, choice, created_at, updated_at)
         VALUES ($1, $2, 0, now(), now())",
    )
    .bind(ctx.alice_id.parse::<i64>().unwrap())
    .bind(poll_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    let note = |updated: Option<&str>| {
        json!({
            "id": poll, "type": "Note", "attributedTo": remy,
            "content": "<p>no longer a poll</p>", "to": [PUBLIC],
            "cc": [format!("https://{}/users/alice", ctx.domain)],
            "published": "2026-01-01T00:00:00Z",
            "updated": updated,
        })
    };

    // Not an edit: the poll stays.
    send(&ctx, &remy, &key, &update(&remy, note(None), 1)).await;
    assert!(poll_options(&ctx, id).await.is_some());

    send(
        &ctx,
        &remy,
        &key,
        &update(&remy, note(Some("2026-01-02T00:00:00Z")), 2),
    )
    .await;
    assert_eq!(poll_options(&ctx, id).await, None);
    let (status_poll, votes): (Option<i64>, i64) = sqlx::query_as(
        "SELECT poll_id, (SELECT count(*) FROM poll_votes WHERE poll_id = $2) FROM statuses WHERE id = $1",
    )
    .bind(id)
    .bind(poll_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(status_poll, None);
    assert_eq!(votes, 0);
}

/// From a domain blocked with `reject_media`, an attachment is recorded as
/// Mastodon records one it does not download (`skip_download`): its URLs,
/// description and blurhash, no file and so `unknown`, with no content type,
/// and the focus alone for its meta. The API points its `url` and
/// `preview_url` at the media proxy, which has nothing to give for it.
#[tokio::test]
async fn test_media_from_a_reject_media_domain_is_recorded_without_its_file() {
    let ctx = TestContext::new("inbound-reject-media").await;
    let (_, remy, key) = seed_remote(&ctx, "remy", "remote.invalid").await;
    sqlx::query(
        "INSERT INTO domain_blocks (domain, severity, reject_media, created_at, updated_at)
         VALUES ('remote.invalid', 2, true, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let note_uri = format!("{remy}/statuses/rejected");
    let attachment = |name: &str| {
        json!({
            "type": "Document", "mediaType": "image/png",
            "url": format!("https://remote.invalid/media/{name}.png"),
            "name": name, "blurhash": "LEHV6nWB2yk8pyo0adR*.7kCMdnj",
            "width": 640, "height": 480, "focalPoint": [0.5, -0.25],
        })
    };
    let note = |names: &[&str], updated: Option<&str>| {
        json!({
            "id": note_uri, "type": "Note", "attributedTo": remy,
            "content": "<p>pictures</p>", "to": [PUBLIC],
            "cc": [format!("https://{}/users/alice", ctx.domain)],
            "published": "2026-01-01T00:00:00Z",
            "updated": updated,
            "attachment": names.iter().map(|n| attachment(n)).collect::<Vec<_>>(),
        })
    };
    send(&ctx, &remy, &key, &create(&remy, note(&["one"], None))).await;
    let id = status_id(&ctx, &note_uri).await.unwrap();

    // An edit adding one records it the same way.
    send(
        &ctx,
        &remy,
        &key,
        &update(
            &remy,
            note(&["one", "two"], Some("2026-01-02T00:00:00Z")),
            1,
        ),
    )
    .await;

    type Row = (
        i64,
        i32,
        Option<String>,
        Option<String>,
        Option<Value>,
        Option<String>,
        Option<i32>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, type, remote_url, file_content_type, file_meta::jsonb, file_file_name, processing
         FROM media_attachments WHERE status_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    for (_, kind, remote_url, content_type, meta, file, processing) in &rows {
        assert_eq!(*kind, 4);
        assert!(remote_url
            .as_deref()
            .unwrap()
            .starts_with("https://remote.invalid/media/"));
        assert_eq!(*content_type, None);
        assert_eq!(*meta, Some(json!({"focus": {"x": 0.5, "y": -0.25}})));
        assert_eq!(*file, None);
        assert_eq!(*processing, Some(2));
    }

    let status: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let media = &status["media_attachments"][0];
    let media_id = rows[0].0;
    assert_eq!(media["type"], "unknown");
    assert_eq!(
        media["url"],
        format!("https://{}/media_proxy/{media_id}/original", ctx.domain)
    );
    assert_eq!(
        media["preview_url"],
        format!("https://{}/media_proxy/{media_id}/small", ctx.domain)
    );
    assert_eq!(media["remote_url"], "https://remote.invalid/media/one.png");
    assert_eq!(media["description"], "one");
    assert_eq!(media["blurhash"], "LEHV6nWB2yk8pyo0adR*.7kCMdnj");
    assert_eq!(media["meta"], json!({"focus": {"x": 0.5, "y": -0.25}}));

    let proxied = ctx
        .api
        .get(&format!("/media_proxy/{media_id}/original"), None)
        .await;
    // Nothing is fetched for it (`!reject_media?`), so the proxy sends the
    // viewer to Paperclip's `missing.png`.
    assert_eq!(proxied.status(), reqwest::StatusCode::FOUND);
    let location = proxied.headers()["location"].to_str().unwrap().to_owned();
    assert!(
        location.ends_with("/files/original/missing.png"),
        "{location}"
    );
    let proxied = ctx
        .api
        .get(&format!("/media_proxy/{media_id}/small"), None)
        .await;
    let location = proxied.headers()["location"].to_str().unwrap().to_owned();
    assert!(location.ends_with("/files/small/missing.png"), "{location}");
}

/// `MediaProxyController#show` authorizes with `MediaAttachmentPolicy
/// #download?`, `StatusPolicy#show?` for the viewer, and sends the viewer to
/// the file; for a remote attachment, which eunha keeps no copy of, to where
/// the copy would come from, and for one of a type Mastodon does not take,
/// whose download fails, nowhere.
#[tokio::test]
async fn test_the_media_proxy_shows_what_the_viewer_may_see() {
    let ctx = TestContext::new("media-proxy-auth").await;
    let (_, remy, key) = seed_remote(&ctx, "remy", "remote.invalid").await;
    // Alice follows remy, so his followers-only post reaches her.
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, created_at, updated_at)
         SELECT $1, id, now(), now() FROM accounts WHERE uri = $2",
    )
    .bind(ctx.alice_id.parse::<i64>().unwrap())
    .bind(&remy)
    .execute(&ctx.db)
    .await
    .unwrap();
    let note_uri = format!("{remy}/statuses/private");
    send(
        &ctx,
        &remy,
        &key,
        &create(
            &remy,
            json!({
                "id": note_uri, "type": "Note", "attributedTo": remy,
                "content": "<p>for followers</p>",
                "to": [format!("{remy}/followers")],
                "published": "2026-01-01T00:00:00Z",
                "attachment": [
                    {"type": "Document", "mediaType": "image/png",
                     "url": "https://remote.invalid/media/a.png",
                     "icon": {"type": "Image", "url": "https://remote.invalid/media/a-small.png"}},
                    {"type": "Document", "mediaType": "application/x-thing",
                     "url": "https://remote.invalid/media/b.thing"},
                ],
            }),
        ),
    )
    .await;
    let id = status_id(&ctx, &note_uri).await.unwrap();
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT unnest(ordered_media_attachment_ids) FROM statuses WHERE id = $1",
    )
    .bind(id)
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    let get = |path: String, token: Option<String>| {
        let api = &ctx.api;
        async move { api.get(&path, token.as_deref()).await }
    };

    // No one signed in, and bob, who does not follow remy: not found.
    for token in [None, Some(ctx.bob_token.clone())] {
        let resp = get(format!("/media_proxy/{}/original", ids[0]), token).await;
        assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    }
    // Alice, who follows him, is sent to it, and to its preview.
    let alice = Some(ctx.alice_token.clone());
    let resp = get(format!("/media_proxy/{}/original", ids[0]), alice.clone()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::FOUND);
    assert_eq!(
        resp.headers()["location"],
        "https://remote.invalid/media/a.png"
    );
    let resp = get(format!("/media_proxy/{}/small", ids[0]), alice.clone()).await;
    assert_eq!(
        resp.headers()["location"],
        "https://remote.invalid/media/a-small.png"
    );
    // One of a type Mastodon does not take: its download would fail.
    let resp = get(format!("/media_proxy/{}/original", ids[1]), alice.clone()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    // Its post discarded, only a moderator who handles reports may have it.
    sqlx::query("UPDATE statuses SET deleted_at = now() WHERE id = $1")
        .bind(id)
        .execute(&ctx.db)
        .await
        .unwrap();
    let resp = get(format!("/media_proxy/{}/original", ids[0]), alice.clone()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    let role: i64 = sqlx::query_scalar(
        "INSERT INTO user_roles (name, permissions, position, created_at, updated_at)
         VALUES ('Moderator', $1, 10, now(), now()) RETURNING id",
    )
    .bind(1_i64 << 4)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    sqlx::query("UPDATE users SET role_id = $1 WHERE account_id = $2")
        .bind(role)
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let resp = get(
        format!("/media_proxy/{}/original", ids[0]),
        Some(ctx.bob_token.clone()),
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::FOUND);
}
