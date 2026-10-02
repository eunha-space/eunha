//! A member's data export, imports and archive takeout, which Mastodon serves
//! as settings pages (`Export`, `Form::Import`, `BulkImportService`,
//! `BackupService`) and eunha under `/api/eunha/v1/`.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

async fn remote(ctx: &TestContext, username: &str, domain: &str) -> i64 {
    let uri = format!("https://{domain}/users/{username}");
    sqlx::query_scalar::<_, i64>(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, shared_inbox_url, discoverable, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, 'remote-key', $4 || '/inbox', $4 || '/outbox', '', true, now(), now())
           RETURNING id"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(username)
    .bind(domain)
    .bind(uri)
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

async fn remote_status(ctx: &TestContext, account_id: i64, uri: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"INSERT INTO statuses (id, account_id, text, visibility, uri, created_at, updated_at)
           VALUES ($1, $2, 'remote post', 0, $3, now(), now()) RETURNING id"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(account_id)
    .bind(uri)
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

async fn exec(ctx: &TestContext, sql: &str, binds: &[i64]) {
    let mut query = sqlx::query(sql);
    for bind in binds {
        query = query.bind(*bind);
    }
    query.execute(&ctx.db).await.unwrap();
}

fn id(s: &str) -> i64 {
    s.parse().unwrap()
}

async fn export(ctx: &TestContext, file: &str) -> (String, String) {
    let resp = ctx
        .api
        .get(
            &format!("/api/eunha/v1/exports/{file}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "{file}");
    let disposition = resp
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    (disposition, resp.text().await.unwrap())
}

#[tokio::test]
async fn exports_are_byte_for_byte_mastodons() {
    let ctx = TestContext::new("export").await;
    let alice = id(&ctx.alice_id);
    let bob = id(&ctx.bob_id);
    let carol = remote(&ctx, "carol", "remote.example").await;
    let dave = remote(&ctx, "dave", "other.example").await;
    let d = &ctx.domain;

    exec(
        &ctx,
        "INSERT INTO follows (account_id, target_account_id, show_reblogs, notify, created_at, updated_at)
         VALUES ($1, $2, true, false, now(), now())",
        &[alice, bob],
    )
    .await;
    exec(
        &ctx,
        "INSERT INTO follows (account_id, target_account_id, show_reblogs, notify, languages, created_at, updated_at)
         VALUES ($1, $2, false, true, '{en,fr}', now(), now())",
        &[alice, carol],
    )
    .await;
    let (disposition, body) = export(&ctx, "follows.csv").await;
    assert_eq!(
        disposition,
        "attachment; filename=\"following_accounts.csv\"; filename*=UTF-8''following_accounts.csv"
    );
    assert_eq!(
        body,
        format!(
            "Account address,Show boosts,Notify on new posts,Languages\n\
             carol@remote.example,false,true,\"en, fr\"\n\
             bob@{d},true,false,\n"
        )
    );

    for target in [dave, carol] {
        exec(
            &ctx,
            "INSERT INTO blocks (account_id, target_account_id, created_at, updated_at) VALUES ($1, $2, now(), now())",
            &[alice, target],
        )
        .await;
    }
    let (disposition, body) = export(&ctx, "blocks.csv").await;
    assert!(disposition.contains("filename=\"blocked_accounts.csv\""));
    assert_eq!(body, "carol@remote.example\ndave@other.example\n");

    exec(
        &ctx,
        "INSERT INTO mutes (account_id, target_account_id, hide_notifications, created_at, updated_at)
         VALUES ($1, $2, true, now(), now())",
        &[alice, bob],
    )
    .await;
    exec(
        &ctx,
        "INSERT INTO mutes (account_id, target_account_id, hide_notifications, created_at, updated_at)
         VALUES ($1, $2, false, now(), now())",
        &[alice, dave],
    )
    .await;
    let (disposition, body) = export(&ctx, "mutes.csv").await;
    assert!(disposition.contains("filename=\"muted_accounts.csv\""));
    assert_eq!(
        body,
        format!("Account address,Hide notifications\ndave@other.example,false\nbob@{d},true\n")
    );

    for domain in ["b.example", "a.example"] {
        sqlx::query(
            "INSERT INTO account_domain_blocks (account_id, domain, created_at, updated_at) VALUES ($1, $2, now(), now())",
        )
        .bind(alice)
        .bind(domain)
        .execute(&ctx.db)
        .await
        .unwrap();
    }
    let (disposition, body) = export(&ctx, "domain_blocks.csv").await;
    assert!(disposition.contains("filename=\"blocked_domains.csv\""));
    assert_eq!(body, "b.example\na.example\n");

    let friends = sqlx::query_scalar::<_, i64>(
        "INSERT INTO lists (account_id, title, created_at, updated_at) VALUES ($1, 'Friends', now(), now()) RETURNING id",
    )
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let work = sqlx::query_scalar::<_, i64>(
        "INSERT INTO lists (account_id, title, created_at, updated_at) VALUES ($1, 'Work, \"stuff\"', now(), now()) RETURNING id",
    )
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    for (list, member) in [(friends, bob), (friends, carol), (work, dave)] {
        exec(
            &ctx,
            "INSERT INTO list_accounts (list_id, account_id) VALUES ($1, $2)",
            &[list, member],
        )
        .await;
    }
    let (_, body) = export(&ctx, "lists.csv").await;
    assert_eq!(
        body,
        format!(
            "Friends,bob@{d}\nFriends,carol@remote.example\n\"Work, \"\"stuff\"\"\",dave@other.example\n"
        )
    );

    let remote_post =
        remote_status(&ctx, carol, "https://remote.example/users/carol/statuses/1").await;
    exec(
        &ctx,
        "INSERT INTO bookmarks (account_id, status_id, created_at, updated_at) VALUES ($1, $2, now(), now())",
        &[alice, remote_post],
    )
    .await;
    let local_post = ctx
        .api
        .post_status(&ctx.bob_token, "bookmark me", "public")
        .await;
    let local_post_id = local_post["id"].as_str().unwrap();
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{local_post_id}/bookmark"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let (disposition, body) = export(&ctx, "bookmarks.csv").await;
    assert!(disposition.contains("filename=\"bookmarks.csv\""));
    assert_eq!(
        body,
        format!(
            "https://{d}/users/bob/statuses/{local_post_id}\nhttps://remote.example/users/carol/statuses/1\n"
        )
    );

    let zeta: Value = ctx
        .api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.alice_token),
            &json!({
                "title": "zeta",
                "context": ["home", "public"],
                "filter_action": "hide",
                "keywords_attributes": [
                    { "keyword": "foo", "whole_word": false },
                    { "keyword": "bar baz", "whole_word": true },
                ],
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let zeta_id = zeta["id"].as_str().unwrap();
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v2/filters/{zeta_id}/statuses"),
            Some(&ctx.alice_token),
            &json!({ "status_id": local_post_id }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let alpha: Value = ctx
        .api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.alice_token),
            &json!({ "title": "alpha", "context": ["notifications"] }),
        )
        .await
        .json()
        .await
        .unwrap();
    sqlx::query("UPDATE custom_filters SET expires_at = '2030-01-02 03:04:05' WHERE id = $1")
        .bind(id(alpha["id"].as_str().unwrap()))
        .execute(&ctx.db)
        .await
        .unwrap();
    let (disposition, body) = export(&ctx, "custom_filters.json").await;
    assert!(disposition.contains("filename=\"custom_filters.json\""));
    assert_eq!(
        body,
        format!(
            "{{\"custom_filters\":[\
             {{\"title\":\"alpha\",\"expires_at\":\"2030-01-02 03:04:05 UTC\",\"context\":[\"notifications\"],\
             \"action\":\"warn\",\"keywords_attributes\":[],\"statuses\":[]}},\
             {{\"title\":\"zeta\",\"expires_at\":null,\"context\":[\"home\",\"public\"],\"action\":\"hide\",\
             \"keywords_attributes\":[{{\"keyword\":\"foo\",\"whole_word\":false}},{{\"keyword\":\"bar baz\",\"whole_word\":true}}],\
             \"statuses\":[\"https://{d}/users/bob/statuses/{local_post_id}\"]}}]}}"
        )
    );

    let summary: Value = ctx
        .api
        .get("/api/eunha/v1/exports", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(summary["blocks"], 2);
    assert_eq!(summary["mutes"], 2);
    assert_eq!(summary["lists"], 2);
    assert_eq!(summary["domain_blocks"], 2);
    assert_eq!(summary["bookmarks"], 2);
    assert_eq!(summary["custom_filters"], 2);

    let resp = ctx
        .api
        .get("/api/eunha/v1/exports/statuses.csv", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = ctx.api.get("/api/eunha/v1/exports/follows.csv", None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
