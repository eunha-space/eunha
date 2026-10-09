//! A member's data export, imports and archive takeout, which Mastodon serves
//! as settings pages (`Export`, `Form::Import`, `BulkImportService`,
//! `BackupService`) and eunha under `/api/eunha/v1/`.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{account_session_cookie, seed_user, tiny_png, TestContext};

async fn remote(ctx: &TestContext, username: &str, domain: &str) -> i64 {
    let uri = format!("https://{domain}/users/{username}");
    sqlx::query_scalar::<_, i64>(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, shared_inbox_url, discoverable, protocol, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, 'remote-key', $4 || '/inbox', $4 || '/outbox', '', true, 1, now(), now())
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

async fn upload(
    ctx: &TestContext,
    token: &str,
    kind: &str,
    mode: &str,
    filename: &str,
    content_type: &str,
    data: impl Into<Vec<u8>>,
) -> reqwest::Response {
    let part = reqwest::multipart::Part::bytes(data.into())
        .file_name(filename.to_owned())
        .mime_str(content_type)
        .unwrap();
    let form = reqwest::multipart::Form::new()
        .text("type", kind.to_owned())
        .text("mode", mode.to_owned())
        .part("data", part);
    ctx.api
        .http
        .post(ctx.api.url("/api/eunha/v1/imports"))
        .header("host", &ctx.api.host)
        .bearer_auth(token)
        .multipart(form)
        .send()
        .await
        .unwrap()
}

/// Run every job to the end: `BulkImportWorker`, the row jobs it queues, and
/// each retry of a row that raised, made due at once, until none is left.
async fn run_jobs(ctx: &TestContext) {
    loop {
        ctx.state.jobs.settle().await;
        eunha::jobs::make_due(&ctx.state).await.unwrap();
        eunha::jobs::drain(&ctx.state).await.unwrap();
        ctx.state.jobs.settle().await;
        let left = count(
            ctx,
            "SELECT count(*) FROM eunha.jobs WHERE dead_at IS NULL",
            &[],
        )
        .await;
        if left == 0 {
            return;
        }
    }
}

/// Confirm an uploaded import, run its jobs to the end, and return it.
async fn confirm(ctx: &TestContext, created: &Value) -> Value {
    let path = format!("/api/eunha/v1/imports/{}", created["id"].as_str().unwrap());
    let resp = ctx
        .api
        .post_json(
            &format!("{path}/confirm"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    run_jobs(ctx).await;
    let resp = ctx.api.get(&path, Some(&ctx.alice_token)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    resp.json().await.unwrap()
}

/// Upload a CSV, confirm it and run it to the end. Returns the finished
/// import.
async fn import(ctx: &TestContext, kind: &str, mode: &str, filename: &str, csv: &str) -> Value {
    let resp = upload(ctx, &ctx.alice_token, kind, mode, filename, "text/csv", csv).await;
    assert_eq!(resp.status(), StatusCode::OK, "{csv}");
    let created: Value = resp.json().await.unwrap();
    assert_eq!(created["state"], "unconfirmed");
    let finished = confirm(ctx, &created).await;
    assert_eq!(finished["state"], "finished", "{finished}");
    finished
}

async fn failures(ctx: &TestContext, import: &Value) -> String {
    let resp = ctx
        .api
        .get(
            &format!(
                "/api/eunha/v1/imports/{}/failures",
                import["id"].as_str().unwrap()
            ),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    resp.text().await.unwrap()
}

async fn error_of(resp: reqwest::Response) -> String {
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    body["error"].as_str().unwrap_or_default().to_owned()
}

async fn count(ctx: &TestContext, sql: &str, binds: &[i64]) -> i64 {
    let mut query = sqlx::query_scalar::<_, i64>(sql);
    for bind in binds {
        query = query.bind(*bind);
    }
    query.fetch_one(&ctx.db).await.unwrap()
}

async fn follows(ctx: &TestContext, follower: i64, target: i64) -> bool {
    count(
        ctx,
        "SELECT count(*) FROM follows WHERE account_id = $1 AND target_account_id = $2",
        &[follower, target],
    )
    .await
        > 0
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
    assert_eq!(summary["can_request_backup"], true);

    let resp = ctx
        .api
        .get("/api/eunha/v1/exports/statuses.csv", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = ctx.api.get("/api/eunha/v1/exports/follows.csv", None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn following_imports_merge_and_overwrite() {
    let ctx = TestContext::new("import-follow").await;
    let alice = id(&ctx.alice_id);
    let bob = id(&ctx.bob_id);
    let (carol, _) = seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let (erin, _) = seed_user(&ctx.db, &ctx.domain, "erin", "erin@test.invalid").await;
    let d = &ctx.domain;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    // Mastodon's own export, with its header: options are carried, a handle
    // with a leading `@` and no domain is ours, and an unknown one fails.
    let csv = format!(
        "Account address,Show boosts,Notify on new posts,Languages\n\
         bob@{d},false,TRUE,\"en, ko\"\n\
         @carol,,,\n\
         nobody@{d},true,false,\n"
    );
    let done = import(&ctx, "following", "merge", "following_accounts.csv", &csv).await;
    assert_eq!(done["total_items"], 3);
    assert_eq!(done["processed_items"], 3);
    assert_eq!(done["imported_items"], 2);
    assert_eq!(done["failure_count"], 1);
    assert_eq!(done["likely_mismatched"], false);
    let row = sqlx::query_as::<_, (bool, bool, Option<Vec<String>>)>(
        "SELECT show_reblogs, notify, languages FROM follows WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(alice)
    .bind(bob)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(row, (false, true, Some(vec!["en".into(), "ko".into()])));
    assert!(follows(&ctx, alice, carol).await);
    assert_eq!(
        failures(&ctx, &done).await,
        format!(
            "Account address,Show boosts,Notify on new posts,Languages\nnobody@{d},true,false,\n"
        )
    );

    // Overwrite, from a file without a header row: what it does not list is
    // unfollowed.
    let done = import(
        &ctx,
        "following",
        "overwrite",
        "follows.csv",
        &format!("erin@{d}\ncarol\n"),
    )
    .await;
    assert_eq!(done["processed_items"], 2);
    assert_eq!(done["imported_items"], 2);
    assert!(!follows(&ctx, alice, bob).await);
    assert!(follows(&ctx, alice, carol).await);
    assert!(follows(&ctx, alice, erin).await);

    // `BulkImportService` indexes the rows by handle, so a handle listed
    // twice is queued once, for its last row; the first is never run, and
    // the import never finishes.
    let resp = upload(
        &ctx,
        &ctx.alice_token,
        "following",
        "merge",
        "follows.csv",
        "text/csv",
        format!("bob@{d}\nbob\n"),
    )
    .await;
    let created: Value = resp.json().await.unwrap();
    let stuck = confirm(&ctx, &created).await;
    assert_eq!(stuck["state"], "in_progress");
    assert_eq!(stuck["total_items"], 2);
    assert_eq!(stuck["processed_items"], 1);
    assert_eq!(stuck["imported_items"], 1);
    assert!(follows(&ctx, alice, bob).await);
    let left = sqlx::query_scalar::<_, Value>(
        "SELECT data FROM bulk_import_rows WHERE bulk_import_id = $1",
    )
    .bind(id(created["id"].as_str().unwrap()))
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(left, vec![json!({ "acct": format!("bob@{d}") })]);
}

#[tokio::test]
async fn block_and_mute_imports_merge_and_overwrite() {
    let ctx = TestContext::new("import-block").await;
    let alice = id(&ctx.alice_id);
    let bob = id(&ctx.bob_id);
    let (carol, _) = seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let blocks = |target: i64| {
        let ctx = &ctx;
        async move {
            count(
                ctx,
                "SELECT count(*) FROM blocks WHERE account_id = $1 AND target_account_id = $2",
                &[alice, target],
            )
            .await
        }
    };

    let done = import(&ctx, "blocking", "merge", "blocked_accounts.csv", "bob\n").await;
    assert_eq!(done["imported_items"], 1);
    assert_eq!(blocks(bob).await, 1);
    let done = import(&ctx, "blocking", "merge", "blocks.csv", "carol\n").await;
    assert_eq!(done["imported_items"], 1);
    let done = import(
        &ctx,
        "blocking",
        "overwrite",
        "blocks.csv",
        "carol\nghost\n",
    )
    .await;
    assert_eq!(done["processed_items"], 2);
    assert_eq!(done["imported_items"], 1);
    assert_eq!(blocks(bob).await, 0);
    assert_eq!(blocks(carol).await, 1);
    assert_eq!(failures(&ctx, &done).await, "ghost\n");

    let hidden = |target: i64| {
        let ctx = &ctx;
        async move {
            sqlx::query_scalar::<_, bool>(
                "SELECT hide_notifications FROM mutes WHERE account_id = $1 AND target_account_id = $2",
            )
            .bind(alice)
            .bind(target)
            .fetch_optional(&ctx.db)
            .await
            .unwrap()
        }
    };
    let csv = "Account address,Hide notifications\nbob,false\ncarol,\nghost,true\n";
    let done = import(&ctx, "muting", "merge", "muted_accounts.csv", csv).await;
    assert_eq!(done["imported_items"], 2);
    assert_eq!(hidden(bob).await, Some(false));
    // A blank choice is `Account#mute!`'s default.
    assert_eq!(hidden(carol).await, Some(true));
    assert_eq!(
        failures(&ctx, &done).await,
        "Account address,Hide notifications\nghost,true\n"
    );
    let done = import(&ctx, "muting", "overwrite", "mutes.csv", "carol\n").await;
    assert_eq!(done["imported_items"], 1);
    assert_eq!(hidden(bob).await, None);
    assert_eq!(hidden(carol).await, Some(true));
}

#[tokio::test]
async fn domain_block_bookmark_and_list_imports() {
    let ctx = TestContext::new("import-misc").await;
    let alice = id(&ctx.alice_id);
    let bob = id(&ctx.bob_id);
    let (carol, _) = seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let d = &ctx.domain;

    let domains = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT domain FROM account_domain_blocks WHERE account_id = $1 ORDER BY domain",
        )
        .bind(alice)
        .fetch_all(&ctx.db)
        .await
        .unwrap()
    };
    let done = import(
        &ctx,
        "domain_blocking",
        "merge",
        "x.csv",
        "#domain\n A.Example \nb.example\n",
    )
    .await;
    assert_eq!(done["imported_items"], 2);
    assert_eq!(domains().await, vec!["a.example", "b.example"]);
    let done = import(&ctx, "domain_blocking", "overwrite", "x.csv", "c.example\n").await;
    assert_eq!(done["imported_items"], 1);
    assert_eq!(domains().await, vec!["c.example"]);

    let bobs = ctx
        .api
        .post_status(&ctx.bob_token, "bob's post", "public")
        .await;
    let alices = ctx
        .api
        .post_status(&ctx.alice_token, "my post", "public")
        .await;
    for status in [&alices] {
        ctx.api
            .post_json(
                &format!(
                    "/api/v1/statuses/{}/bookmark",
                    status["id"].as_str().unwrap()
                ),
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await;
    }
    let bob_uri = format!(
        "https://{d}/users/bob/statuses/{}",
        bobs["id"].as_str().unwrap()
    );
    let missing = format!("https://{d}/users/bob/statuses/1");
    let done = import(
        &ctx,
        "bookmarks",
        "merge",
        "bookmarks.csv",
        &format!("{bob_uri}\n{missing}\n"),
    )
    .await;
    assert_eq!(done["imported_items"], 1);
    assert_eq!(failures(&ctx, &done).await, format!("{missing}\n"));
    assert_eq!(
        count(
            &ctx,
            "SELECT count(*) FROM bookmarks WHERE account_id = $1",
            &[alice]
        )
        .await,
        2
    );
    let done = import(
        &ctx,
        "bookmarks",
        "overwrite",
        "bookmarks.csv",
        &format!("#uri\n{bob_uri}\n"),
    )
    .await;
    assert_eq!(done["imported_items"], 1);
    let kept =
        sqlx::query_scalar::<_, i64>("SELECT status_id FROM bookmarks WHERE account_id = $1")
            .bind(alice)
            .fetch_all(&ctx.db)
            .await
            .unwrap();
    assert_eq!(kept, vec![id(bobs["id"].as_str().unwrap())]);

    let members = |title: &'static str| {
        let ctx = &ctx;
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT la.account_id FROM list_accounts la JOIN lists l ON l.id = la.list_id
                 WHERE l.account_id = $1 AND l.title = $2 ORDER BY la.account_id",
            )
            .bind(alice)
            .bind(title)
            .fetch_all(&ctx.db)
            .await
            .unwrap()
        }
    };
    let csv = format!("Friends,bob@{d}\nFriends,carol\n");
    let done = import(&ctx, "lists", "merge", "lists.csv", &csv).await;
    assert_eq!(done["imported_items"], 2);
    let mut expected = vec![bob, carol];
    expected.sort();
    assert_eq!(members("Friends").await, expected);
    // Members are followed so that they can be listed.
    assert!(follows(&ctx, alice, bob).await && follows(&ctx, alice, carol).await);
    // Upstream's `list.accounts <<` refuses an account already on the list,
    // so merging the same file again fails every row.
    let done = import(&ctx, "lists", "merge", "lists.csv", &csv).await;
    assert_eq!(done["imported_items"], 0);
    assert_eq!(failures(&ctx, &done).await, csv);
    let done = import(&ctx, "lists", "overwrite", "lists.csv", "Work,carol\n").await;
    assert_eq!(done["imported_items"], 1);
    assert_eq!(members("Friends").await, Vec::<i64>::new());
    assert_eq!(
        count(
            &ctx,
            "SELECT count(*) FROM lists WHERE account_id = $1",
            &[alice]
        )
        .await,
        1
    );
    assert_eq!(members("Work").await, vec![carol]);
}

#[tokio::test]
async fn custom_filter_imports_read_the_export() {
    let ctx = TestContext::new("import-filters").await;
    let alice = id(&ctx.alice_id);
    let post = ctx
        .api
        .post_status(&ctx.bob_token, "filtered", "public")
        .await;
    let uri = format!(
        "https://{}/users/bob/statuses/{}",
        ctx.domain,
        post["id"].as_str().unwrap()
    );
    ctx.api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.alice_token),
            &json!({ "title": "old", "context": ["home"] }),
        )
        .await;
    let file = json!({ "custom_filters": [
        {
            "title": "spoilers",
            "expires_at": "2030-01-02 03:04:05 UTC",
            "context": ["home", " public "],
            "action": "blur",
            "keywords_attributes": [{ "keyword": "finale", "whole_word": false }],
            "statuses": [uri, "https://gone.example/1"],
        },
        { "title": "bad", "context": ["nowhere"], "action": "warn", "keywords_attributes": [], "statuses": [] },
    ]})
    .to_string();
    let resp = upload(
        &ctx,
        &ctx.alice_token,
        "custom_filters",
        "overwrite",
        "custom_filters.json",
        "application/json",
        file.clone(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let created: Value = resp.json().await.unwrap();
    assert_eq!(created["total_items"], 2);
    assert_eq!(created["missing_status"], true);
    // The bad filter's row raises on every run, and counts as failed once
    // its retries are spent.
    let done = confirm(&ctx, &created).await;
    assert_eq!(done["state"], "finished");
    assert_eq!(done["processed_items"], 2);
    assert_eq!(done["imported_items"], 1);
    let filters = sqlx::query_as::<_, (String, Vec<String>, i32, Option<chrono::NaiveDateTime>)>(
        "SELECT phrase, context, action, expires_at FROM custom_filters WHERE account_id = $1",
    )
    .bind(alice)
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(filters.len(), 1, "overwrite drops the old filter");
    assert_eq!(filters[0].0, "spoilers");
    assert_eq!(filters[0].1, vec!["home", "public"]);
    assert_eq!(filters[0].2, 2);
    assert!(filters[0].3.is_some());
    assert_eq!(
        count(&ctx, "SELECT count(*) FROM custom_filter_statuses", &[]).await,
        1
    );
    // The failures file is JSON, the rows as they were stored.
    let failed = failures(&ctx, &done).await;
    assert_eq!(
        failed,
        r#"{"custom_filters":[{"title":"bad","action":"warn","context":["nowhere"],"statuses":[],"keywords_attributes":[]}]}"#
    );

    // Upstream saves the filter, and its keywords, before the action is
    // checked: a bad action leaves a filter behind on each of the row's
    // seven runs, and the row still fails.
    let oops = json!({ "custom_filters": [{
        "title": "oops",
        "context": ["home"],
        "action": "nuke",
        "keywords_attributes": [{ "keyword": "x", "whole_word": true }],
        "statuses": [],
    }]})
    .to_string();
    let resp = upload(
        &ctx,
        &ctx.alice_token,
        "custom_filters",
        "merge",
        "custom_filters.json",
        "application/json",
        oops,
    )
    .await;
    let created: Value = resp.json().await.unwrap();
    let done = confirm(&ctx, &created).await;
    assert_eq!(done["state"], "finished");
    assert_eq!(done["processed_items"], 1);
    assert_eq!(done["imported_items"], 0);
    let left = sqlx::query_as::<_, (i32, i64)>(
        "SELECT f.action, count(k.id) FROM custom_filters f
         JOIN custom_filter_keywords k ON k.custom_filter_id = f.id
         WHERE f.account_id = $1 AND f.phrase = 'oops' GROUP BY f.id",
    )
    .bind(alice)
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(left, vec![(0, 1); 7]);

    // A filters file is refused for any other type.
    let resp = upload(
        &ctx,
        &ctx.alice_token,
        "blocking",
        "merge",
        "custom_filters.json",
        "application/json",
        file,
    )
    .await;
    assert_eq!(
        error_of(resp).await,
        "Validation failed: Data Incompatible with the selected import type"
    );
}

#[tokio::test]
async fn uploads_are_validated_as_mastodon_validates_them() {
    let ctx = TestContext::new("import-invalid").await;
    let t = &ctx.alice_token;

    let resp = upload(&ctx, t, "following", "merge", "empty.csv", "text/csv", "").await;
    assert_eq!(
        error_of(resp).await,
        "Validation failed: Data Empty CSV file"
    );
    let resp = upload(&ctx, t, "blocking", "merge", "x.csv", "text/csv", "a\"b\n").await;
    assert_eq!(
        error_of(resp).await,
        "Validation failed: Data Invalid CSV file. Error: Illegal quoting in line 1."
    );
    // A lists import needs a list name; a following export has none.
    let resp = upload(
        &ctx,
        t,
        "lists",
        "merge",
        "x.csv",
        "text/csv",
        "Account address,Show boosts\nbob,true\n",
    )
    .await;
    assert_eq!(
        error_of(resp).await,
        "Validation failed: Data Incompatible with the selected import type"
    );
    let resp = upload(&ctx, t, "", "merge", "x.csv", "text/csv", "bob\n").await;
    assert_eq!(
        error_of(resp).await,
        "Validation failed: Type can't be blank"
    );
    let many: String = (0..20_001)
        .map(|i| format!("user{i}@example.com\n"))
        .collect();
    let resp = upload(
        &ctx,
        t,
        "blocking",
        "merge",
        "x.csv",
        "text/csv",
        many.clone(),
    )
    .await;
    assert_eq!(
        error_of(resp).await,
        "Validation failed: Data contains more than 20000 rows"
    );
    let follows: String = (0..7_501)
        .map(|i| format!("user{i}@example.com\n"))
        .collect();
    let resp = upload(&ctx, t, "following", "merge", "x.csv", "text/csv", follows).await;
    assert_eq!(
        error_of(resp).await,
        "Validation failed: Data You cannot follow more than 7500 people"
    );
    let big = vec![b'a'; 20 * 1024 * 1024 + 1];
    let resp = upload(&ctx, t, "blocking", "merge", "x.csv", "text/csv", big).await;
    assert_eq!(
        error_of(resp).await,
        "Validation failed: Data File is too large"
    );

    // The file's name says it is a mute list.
    let resp = upload(
        &ctx,
        t,
        "blocking",
        "merge",
        "muted_accounts.csv",
        "text/csv",
        "bob\n",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let created: Value = resp.json().await.unwrap();
    assert_eq!(created["likely_mismatched"], true);
    let path = format!("/api/eunha/v1/imports/{}", created["id"].as_str().unwrap());

    // Only an unconfirmed import can be dropped or confirmed, and only a
    // finished one has failures.
    let resp = ctx.api.get(&format!("{path}/failures"), Some(t)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = ctx.api.get(&path, Some(&ctx.bob_token)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = ctx.api.delete(&path, t).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = ctx
        .api
        .post_json(&format!("{path}/confirm"), Some(t), &json!({}))
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let recent: Value = ctx
        .api
        .get("/api/eunha/v1/imports", Some(t))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(recent.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn imports_run_on_the_job_queue_and_are_vacuumed() {
    let ctx = TestContext::new("import-jobs").await;
    ctx.state.jobs.set_mode(eunha::jobs::Mode::Durable);
    let alice = id(&ctx.alice_id);
    seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;

    // Confirming queues `BulkImportWorker` on the `pull` queue, never
    // retried.
    let resp = upload(
        &ctx,
        &ctx.alice_token,
        "blocking",
        "merge",
        "x.csv",
        "text/csv",
        "bob\ncarol\n",
    )
    .await;
    let created: Value = resp.json().await.unwrap();
    let import_id = id(created["id"].as_str().unwrap());
    let resp = ctx
        .api
        .post_json(
            &format!("/api/eunha/v1/imports/{import_id}/confirm"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    let scheduled: Value = resp.json().await.unwrap();
    assert_eq!(scheduled["state"], "scheduled");
    let queued = sqlx::query_as::<_, (String, String, Value, i32, bool)>(
        "SELECT queue, kind, args, max_retries, keep_dead FROM eunha.jobs ORDER BY id",
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        queued,
        vec![(
            "pull".into(),
            "BulkImportWorker".into(),
            json!({ "bulk_import_id": import_id }),
            -1,
            false
        )]
    );
    // It queues a row job for each row, which runs them.
    assert_eq!(eunha::jobs::drain(&ctx.state).await.unwrap(), 3);
    let states = sqlx::query_as::<_, (i32, i32, i32)>(
        "SELECT state, processed_items, imported_items FROM bulk_imports WHERE id = $1",
    )
    .bind(import_id)
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(states, vec![(3, 2, 2)]);
    assert_eq!(
        count(
            &ctx,
            "SELECT count(*) FROM blocks WHERE account_id = $1",
            &[alice]
        )
        .await,
        2
    );

    // A row that raises is retried six times, `Import::RowWorker`'s
    // `retry: 6`, and then counted as processed and not imported; its job
    // is not kept (`dead: false`).
    let file = json!({ "custom_filters": [
        { "title": "bad", "context": ["nowhere"], "action": "warn", "keywords_attributes": [], "statuses": [] },
    ]})
    .to_string();
    let resp = upload(
        &ctx,
        &ctx.alice_token,
        "custom_filters",
        "merge",
        "custom_filters.json",
        "application/json",
        file,
    )
    .await;
    let created: Value = resp.json().await.unwrap();
    let scheduled = id(created["id"].as_str().unwrap());
    ctx.api
        .post_json(
            &format!("/api/eunha/v1/imports/{scheduled}/confirm"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(eunha::jobs::drain(&ctx.state).await.unwrap(), 2);
    let progress = || async {
        sqlx::query_as::<_, (i32, i32, i32)>(
            "SELECT state, processed_items, imported_items FROM bulk_imports WHERE id = $1",
        )
        .bind(scheduled)
        .fetch_one(&ctx.db)
        .await
        .unwrap()
    };
    assert_eq!(progress().await, (2, 0, 0));
    for retry in 1..=6 {
        let job = sqlx::query_as::<_, (String, String, i32, i32, bool)>(
            "SELECT queue, kind, attempts, max_retries, keep_dead FROM eunha.jobs",
        )
        .fetch_one(&ctx.db)
        .await
        .unwrap();
        assert_eq!(
            job,
            ("pull".into(), "Import::RowWorker".into(), retry, 6, false)
        );
        eunha::jobs::make_due(&ctx.state).await.unwrap();
        assert_eq!(eunha::jobs::drain(&ctx.state).await.unwrap(), 1);
    }
    assert_eq!(progress().await, (3, 1, 0));
    assert_eq!(count(&ctx, "SELECT count(*) FROM eunha.jobs", &[]).await, 0);

    // `Vacuum::ImportsVacuum`: unconfirmed ones after ten minutes, the rest
    // after a week.
    let resp = upload(
        &ctx,
        &ctx.alice_token,
        "blocking",
        "merge",
        "x.csv",
        "text/csv",
        "bob\n",
    )
    .await;
    let stale: Value = resp.json().await.unwrap();
    let resp = upload(
        &ctx,
        &ctx.alice_token,
        "blocking",
        "merge",
        "x.csv",
        "text/csv",
        "bob\n",
    )
    .await;
    let fresh: Value = resp.json().await.unwrap();
    exec(
        &ctx,
        "UPDATE bulk_imports SET created_at = now() - interval '11 minutes' WHERE id = $1",
        &[id(stale["id"].as_str().unwrap())],
    )
    .await;
    exec(
        &ctx,
        "UPDATE bulk_imports SET created_at = now() - interval '8 days' WHERE id = $1",
        &[import_id],
    )
    .await;
    assert_eq!(
        eunha::portability::import::vacuum(&ctx.state)
            .await
            .unwrap(),
        2
    );
    let left = sqlx::query_scalar::<_, i64>("SELECT id FROM bulk_imports ORDER BY id")
        .fetch_all(&ctx.db)
        .await
        .unwrap();
    assert_eq!(left, vec![scheduled, id(fresh["id"].as_str().unwrap())]);
}

fn zip_entries(bytes: &[u8]) -> (Vec<String>, zip::ZipArchive<std::io::Cursor<Vec<u8>>>) {
    let archive = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).unwrap();
    let names = (0..archive.len())
        .map(|i| archive.name_for_index(i).unwrap().to_owned())
        .collect();
    (names, archive)
}

fn zip_json(archive: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>, name: &str) -> Value {
    let file = archive.by_name(name).unwrap();
    serde_json::from_reader(file).unwrap()
}

/// An object's keys, sorted.
fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

/// A timestamp as `Time#iso8601` writes one: whole seconds, `Z`.
fn assert_iso8601(value: &Value) {
    let text = value.as_str().unwrap();
    assert!(
        chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%SZ").is_ok(),
        "{text}"
    );
}

/// `ContextHelper#full_context` as Mastodon 4.7 serializes it.
fn mastodon_full_context() -> Value {
    serde_json::from_str(
        r#"
    [
        "https://www.w3.org/ns/activitystreams",
        "https://w3id.org/security/v1",
        "https://www.w3.org/ns/cid/v1",
        "https://purl.archive.org/socialweb/webfinger",
        {
            "manuallyApprovesFollowers": "as:manuallyApprovesFollowers",
            "sensitive": "as:sensitive",
            "Hashtag": "as:Hashtag",
            "movedTo": {
                "@id": "as:movedTo",
                "@type": "@id"
            },
            "alsoKnownAs": {
                "@id": "as:alsoKnownAs",
                "@type": "@id"
            },
            "toot": "http://joinmastodon.org/ns#",
            "Emoji": "toot:Emoji",
            "featured": {
                "@id": "toot:featured",
                "@type": "@id"
            },
            "featuredTags": {
                "@id": "toot:featuredTags",
                "@type": "@id"
            },
            "schema": "http://schema.org#",
            "PropertyValue": "schema:PropertyValue",
            "value": "schema:value",
            "ostatus": "http://ostatus.org#",
            "atomUri": "ostatus:atomUri",
            "inReplyToAtomUri": "ostatus:inReplyToAtomUri",
            "conversation": "ostatus:conversation",
            "focalPoint": {
                "@container": "@list",
                "@id": "toot:focalPoint"
            },
            "blurhash": "toot:blurhash",
            "discoverable": "toot:discoverable",
            "indexable": "toot:indexable",
            "memorial": "toot:memorial",
            "votersCount": "toot:votersCount",
            "suspended": "toot:suspended",
            "attributionDomains": {
                "@id": "toot:attributionDomains",
                "@container": "@set"
            },
            "showFeatured": "toot:showFeatured",
            "showMedia": "toot:showMedia",
            "showRepliesInMedia": "toot:showRepliesInMedia",
            "QuoteRequest": "https://w3id.org/fep/044f#QuoteRequest",
            "quote": {
                "@id": "https://w3id.org/fep/044f#quote",
                "@type": "@id"
            },
            "quoteUri": "http://fedibird.com/ns#quoteUri",
            "_misskey_quote": "https://misskey-hub.net/ns#_misskey_quote",
            "quoteAuthorization": {
                "@id": "https://w3id.org/fep/044f#quoteAuthorization",
                "@type": "@id"
            },
            "gts": "https://gotosocial.org/ns#",
            "interactionPolicy": {
                "@id": "gts:interactionPolicy",
                "@type": "@id"
            },
            "canFeature": {
                "@id": "https://w3id.org/fep/7aa9#canFeature",
                "@type": "@id"
            },
            "canQuote": {
                "@id": "gts:canQuote",
                "@type": "@id"
            },
            "automaticApproval": {
                "@id": "gts:automaticApproval",
                "@type": "@id"
            },
            "manualApproval": {
                "@id": "gts:manualApproval",
                "@type": "@id"
            },
            "QuoteAuthorization": "https://w3id.org/fep/044f#QuoteAuthorization",
            "interactingObject": {
                "@id": "gts:interactingObject",
                "@type": "@id"
            },
            "interactionTarget": {
                "@id": "gts:interactionTarget",
                "@type": "@id"
            },
            "FeatureRequest": "https://w3id.org/fep/7aa9#FeatureRequest",
            "FeaturedCollection": "https://w3id.org/fep/7aa9#FeaturedCollection",
            "FeaturedItem": "https://w3id.org/fep/7aa9#FeaturedItem",
            "FeatureAuthorization": "https://w3id.org/fep/7aa9#FeatureAuthorization",
            "topic": {
                "@id": "https://w3id.org/fep/7aa9#topic",
                "@type": "@id"
            },
            "featuredObject": {
                "@id": "https://w3id.org/fep/7aa9#featuredObject",
                "@type": "@id"
            },
            "featureAuthorization": {
                "@id": "https://w3id.org/fep/7aa9#featureAuthorization",
                "@type": "@id"
            }
        }
    ]
"#,
    )
    .unwrap()
}

/// `GET /backups/{id}/download` with `cookie`, if any.
async fn download(ctx: &TestContext, backup_id: &str, cookie: Option<&str>) -> reqwest::Response {
    let mut request = ctx
        .api
        .http
        .get(ctx.api.url(&format!("/backups/{backup_id}/download")))
        .header("host", &ctx.api.host);
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn archive_takeout_zips_posts_media_and_actor() {
    let ctx = TestContext::new("backup").await;
    let alice = id(&ctx.alice_id);
    let d = &ctx.domain;

    let media: Value = ctx
        .api
        .post_multipart_file(
            "/api/v1/media",
            &ctx.alice_token,
            "pic.png",
            "image/png",
            tiny_png(),
            &[],
        )
        .await
        .json()
        .await
        .unwrap();
    let post: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({ "status": "with a picture", "media_ids": [media["id"]] }),
        )
        .await
        .json()
        .await
        .unwrap();
    let bobs = ctx
        .api
        .post_status(&ctx.bob_token, "likeable", "public")
        .await;
    let bob_id = bobs["id"].as_str().unwrap();
    for action in ["favourite", "bookmark", "reblog"] {
        let resp = ctx
            .api
            .post_json(
                &format!("/api/v1/statuses/{bob_id}/{action}"),
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK, "{action}");
    }
    // A followers-only post of her own, boosted: the `Announce` carries it.
    let private = ctx
        .api
        .post_status(&ctx.alice_token, "for my followers", "private")
        .await;
    let private_id = private["id"].as_str().unwrap();
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{private_id}/reblog"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "self-boost");
    // An avatar, stored where Paperclip keeps one.
    let avatar_key = format!(
        "accounts/avatars/{}/original/face.png",
        eunha::media::int_to_path(alice)
    );
    ctx.state
        .storage
        .store(&tiny_png(), &avatar_key, "image/png")
        .await
        .unwrap();
    exec(
        &ctx,
        "UPDATE accounts SET avatar_file_name = 'face.png', avatar_content_type = 'image/png' WHERE id = $1",
        &[alice],
    )
    .await;

    let resp = ctx
        .api
        .post_json("/api/eunha/v1/backups", Some(&ctx.alice_token), &json!({}))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let backup: Value = resp.json().await.unwrap();
    assert_eq!(backup["processed"], true);
    let backup_id = backup["id"].as_str().unwrap();

    let link: Value = ctx
        .api
        .get(
            &format!("/api/eunha/v1/backups/{backup_id}/download"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let file_name =
        sqlx::query_scalar::<_, String>("SELECT dump_file_name FROM backups WHERE id = $1")
            .bind(id(backup_id))
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    let key = format!(
        "backups/dumps/{}/original/{file_name}",
        eunha::media::int_to_path(id(backup_id))
    );
    assert!(link["url"].as_str().unwrap().contains(&key), "{link}");
    assert!(file_name.starts_with("archive-") && file_name.ends_with(".zip"));
    let bytes = ctx.state.storage.get(&key).await.unwrap();

    let media_id = id(media["id"].as_str().unwrap());
    let media_file = sqlx::query_scalar::<_, String>(
        "SELECT file_file_name FROM media_attachments WHERE id = $1",
    )
    .bind(media_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let media_path = format!(
        "media_attachments/files/{}/original/{media_file}",
        eunha::media::int_to_path(media_id)
    );
    let (names, mut archive) = zip_entries(&bytes);
    assert_eq!(
        names,
        vec![
            "outbox.json".to_owned(),
            media_path.clone(),
            "likes.json".into(),
            "bookmarks.json".into(),
            "avatar.png".into(),
            "actor.json".into(),
        ]
    );
    let bob_uri = format!("https://{d}/users/bob/statuses/{bob_id}");
    let likes = zip_json(&mut archive, "likes.json");
    assert_eq!(likes["id"], "likes.json");
    assert_eq!(likes["type"], "OrderedCollection");
    assert_eq!(likes["orderedItems"], json!([bob_uri]));
    assert!(likes.get("totalItems").is_none());
    let bookmarks = zip_json(&mut archive, "bookmarks.json");
    assert_eq!(bookmarks["orderedItems"], json!([bob_uri]));
    let outbox = zip_json(&mut archive, "outbox.json");
    assert_eq!(
        keys(&outbox),
        keys(&json!({"@context": 0, "id": 0, "type": 0, "totalItems": 0, "orderedItems": 0}))
    );
    assert_eq!(outbox["id"], "outbox.json");
    assert_eq!(outbox["type"], "OrderedCollection");
    assert_eq!(outbox["totalItems"], 4);
    assert_eq!(outbox["@context"], mastodon_full_context());
    let items = outbox["orderedItems"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    let alice_uri = format!("https://{d}/users/alice");
    // `ActivityPub::CreateNoteSerializer`, unsigned and without a context.
    let activity_keys = keys(&json!({
        "id": 0, "type": 0, "actor": 0, "published": 0, "to": 0, "cc": 0, "object": 0,
    }));
    assert_eq!(items[0]["type"], "Create");
    assert_eq!(keys(&items[0]), activity_keys);
    assert_eq!(
        items[0]["id"],
        format!("{}/activity", post["uri"].as_str().unwrap())
    );
    assert_eq!(items[0]["actor"], alice_uri.as_str());
    assert_iso8601(&items[0]["published"]);
    let note = &items[0]["object"];
    assert_eq!(note["id"], post["uri"]);
    // `ActivityPub::NoteSerializer`'s members on a local post.
    let mut note_keys = keys(&json!({
        "id": 0, "type": 0, "summary": 0, "inReplyTo": 0, "published": 0, "url": 0,
        "attributedTo": 0, "to": 0, "cc": 0, "sensitive": 0, "atomUri": 0,
        "inReplyToAtomUri": 0, "conversation": 0, "context": 0, "content": 0,
        "attachment": 0, "tag": 0, "replies": 0, "likes": 0, "shares": 0,
        "interactionPolicy": 0,
    }));
    if note.get("contentMap").is_some() {
        note_keys.push("contentMap".into());
        note_keys.sort();
    }
    assert_eq!(keys(note), note_keys);
    assert_iso8601(&note["published"]);
    assert_eq!(
        note["url"],
        format!("https://{d}/@alice/{}", post["id"].as_str().unwrap())
    );
    assert_eq!(note["atomUri"], post["uri"]);
    assert!(note["inReplyToAtomUri"].is_null());
    let replies = format!("{}/replies", post["uri"].as_str().unwrap());
    assert_eq!(
        note["replies"],
        json!({
            "id": replies,
            "type": "Collection",
            "first": {
                "type": "CollectionPage",
                "next": format!("{replies}?only_other_accounts=true&page=true"),
                "partOf": replies,
                "items": [],
            },
        })
    );
    assert_eq!(
        note["likes"],
        json!({ "id": format!("{}/likes", post["uri"].as_str().unwrap()), "type": "Collection", "totalItems": 0 })
    );
    assert_eq!(note["shares"]["totalItems"], 0);
    // An attachment's URL becomes its path after `/system/`.
    let attachment = &note["attachment"][0];
    for key in ["type", "mediaType", "url", "name", "blurhash"] {
        assert!(attachment.get(key).is_some(), "{key} in {attachment}");
    }
    let media_url = url::Url::parse(media["url"].as_str().unwrap()).unwrap();
    let rewritten = media_url.path();
    assert_eq!(
        attachment["url"],
        rewritten.strip_prefix("/system/").unwrap_or(rewritten)
    );
    // `ActivityPub::AnnounceNoteSerializer`: the boosted author first in `cc`.
    assert_eq!(items[1]["type"], "Announce");
    assert_eq!(keys(&items[1]), activity_keys);
    assert_eq!(items[1]["object"], bob_uri.as_str());
    assert_eq!(
        items[1]["to"],
        json!(["https://www.w3.org/ns/activitystreams#Public"])
    );
    assert_eq!(
        items[1]["cc"],
        json!([
            format!("https://{d}/users/bob"),
            format!("{alice_uri}/followers")
        ])
    );
    assert_iso8601(&items[1]["published"]);
    // Her own followers-only post goes inline in her boost of it.
    assert_eq!(items[2]["type"], "Create");
    assert_eq!(items[3]["type"], "Announce");
    assert_eq!(items[3]["to"], json!([format!("{alice_uri}/followers")]));
    assert_eq!(items[3]["cc"], json!([alice_uri]));
    assert_eq!(items[3]["object"]["id"], private["uri"]);
    assert_eq!(items[3]["object"]["type"], "Note");
    assert!(items[3]["object"].get("@context").is_none());

    let actor = zip_json(&mut archive, "actor.json");
    // `ActivityPub::ActorSerializer`'s members, for an account with an
    // avatar and no header, moves or emoji, and the archive's three.
    assert_eq!(
        keys(&actor),
        keys(&json!({
            "@context": 0, "id": 0, "webfinger": 0, "type": 0, "following": 0,
            "followers": 0, "inbox": 0, "outbox": 0, "featured": 0, "featuredTags": 0,
            "preferredUsername": 0, "name": 0, "summary": 0, "url": 0,
            "manuallyApprovesFollowers": 0, "discoverable": 0, "indexable": 0,
            "published": 0, "memorial": 0, "showFeatured": 0, "showMedia": 0,
            "showRepliesInMedia": 0, "interactionPolicy": 0, "featuredCollections": 0,
            "publicKey": 0, "tag": 0, "attachment": 0, "endpoints": 0, "icon": 0,
            "likes": 0, "bookmarks": 0,
        }))
    );
    let context = actor["@context"].as_array().unwrap();
    assert_eq!(
        context[..3],
        [
            json!("https://www.w3.org/ns/activitystreams"),
            json!("https://w3id.org/security/v1"),
            json!("https://purl.archive.org/socialweb/webfinger"),
        ]
    );
    assert_eq!(
        keys(&context[3]),
        keys(&json!({
            "manuallyApprovesFollowers": 0, "toot": 0, "featured": 0, "featuredTags": 0,
            "alsoKnownAs": 0, "movedTo": 0, "schema": 0, "PropertyValue": 0, "value": 0,
            "discoverable": 0, "suspended": 0, "memorial": 0, "indexable": 0,
            "attributionDomains": 0, "showFeatured": 0, "showMedia": 0,
            "showRepliesInMedia": 0, "gts": 0, "interactionPolicy": 0, "canFeature": 0,
            "canQuote": 0, "automaticApproval": 0, "manualApproval": 0, "focalPoint": 0,
        }))
    );
    assert_eq!(actor["webfinger"], format!("alice@{d}"));
    assert_eq!(
        actor["featuredTags"],
        format!("{alice_uri}/collections/tags")
    );
    assert!(actor["published"].as_str().unwrap().ends_with("T00:00:00Z"));
    assert_eq!(actor["outbox"], "outbox.json");
    assert_eq!(actor["likes"], "likes.json");
    assert_eq!(actor["bookmarks"], "bookmarks.json");
    assert_eq!(
        actor["icon"],
        json!({ "type": "Image", "mediaType": "image/png", "url": "avatar.png" })
    );
    let mut picture = Vec::new();
    std::io::Read::read_to_end(&mut archive.by_name(&media_path).unwrap(), &mut picture).unwrap();
    assert_eq!(picture, ctx.state.storage.get(&media_path).await.unwrap());
    assert!(!picture.is_empty());
    let mut avatar = Vec::new();
    std::io::Read::read_to_end(&mut archive.by_name("avatar.png").unwrap(), &mut avatar).unwrap();
    assert_eq!(avatar, tiny_png());

    // `BackupPolicy`: one archive in six days.
    let resp = ctx
        .api
        .post_json("/api/eunha/v1/backups", Some(&ctx.alice_token), &json!({}))
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let page: Value = ctx
        .api
        .get("/api/eunha/v1/exports", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(page["can_request_backup"], false);
    assert_eq!(page["backups"].as_array().unwrap().len(), 1);
    // Someone else's archive is nobody's business.
    let resp = ctx
        .api
        .get(
            &format!("/api/eunha/v1/backups/{backup_id}/download"),
            Some(&ctx.bob_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // `UserMailer#backup_ready`'s link, `BackupsController#download`: a
    // signed-in browser is sent to a link to the file.
    let alice_cookie =
        account_session_cookie(&ctx.api, "alice@test.invalid", "testpassword123").await;
    let resp = download(&ctx, backup_id, Some(&alice_cookie)).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert!(resp.headers()["location"].to_str().unwrap().contains(&key));
    // Someone else's is not found among theirs.
    let bob_cookie = account_session_cookie(&ctx.api, "bob@test.invalid", "testpassword123").await;
    let resp = download(&ctx, backup_id, Some(&bob_cookie)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // A suspended member may still take theirs away.
    exec(
        &ctx,
        "UPDATE accounts SET suspended_at = now() WHERE id = $1",
        &[alice],
    )
    .await;
    let resp = download(&ctx, backup_id, Some(&alice_cookie)).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    exec(
        &ctx,
        "UPDATE accounts SET suspended_at = NULL WHERE id = $1",
        &[alice],
    )
    .await;
    // Signed out: to the sign-in page, which sends the browser back here.
    let resp = download(&ctx, backup_id, None).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(resp.headers()["location"], "/account/login");
    let return_to = resp.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert!(return_to.starts_with("account_return_to="), "{return_to}");
    let resp = ctx
        .api
        .http
        .post(ctx.api.url("/account/login"))
        .header("host", &ctx.api.host)
        .header("cookie", &return_to)
        .form(&[
            ("email", "alice@test.invalid"),
            ("password", "testpassword123"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        resp.headers()["location"],
        format!("/backups/{backup_id}/download").as_str()
    );

    // Six days on, another may be made, and it replaces the first.
    exec(
        &ctx,
        "UPDATE backups SET created_at = now() - interval '6 days 1 hour'",
        &[],
    )
    .await;
    let resp = ctx
        .api
        .post_json("/api/eunha/v1/backups", Some(&ctx.alice_token), &json!({}))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let second: Value = resp.json().await.unwrap();
    let ids = sqlx::query_scalar::<_, i64>("SELECT id FROM backups ORDER BY id")
        .fetch_all(&ctx.db)
        .await
        .unwrap();
    assert_eq!(ids, vec![id(second["id"].as_str().unwrap())]);
    assert!(
        ctx.state.storage.get(&key).await.unwrap().is_empty(),
        "the old file went too"
    );

    // `Vacuum::BackupsVacuum` keeps archives `backups_retention_period` days.
    exec(
        &ctx,
        "UPDATE backups SET created_at = now() - interval '8 days'",
        &[],
    )
    .await;
    assert_eq!(
        eunha::portability::backup::vacuum(&ctx.state)
            .await
            .unwrap(),
        1
    );
    assert_eq!(count(&ctx, "SELECT count(*) FROM backups", &[]).await, 0);
}
