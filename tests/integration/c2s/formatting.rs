//! Content formatting as the API serves it: `TextFormatter` over what local
//! accounts write, `MASTODON_STRICT` over what remote servers send.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

async fn insert_remote_account(ctx: &TestContext, note: &str, fields: Value) -> i64 {
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, fields, url, uri,
                                inbox_url, created_at, updated_at)
           VALUES ($1, 'carol', 'remote.example', 'Carol', $2, $3,
                   'https://remote.example/@carol', 'https://remote.example/users/carol',
                   'https://remote.example/users/carol/inbox', now(), now())"#,
    )
    .bind(id)
    .bind(note)
    .bind(fields)
    .execute(&ctx.db)
    .await
    .unwrap();
    id
}

async fn get_json(ctx: &TestContext, path: &str) -> Value {
    let resp = ctx.api.get(path, Some(&ctx.alice_token)).await;
    assert_eq!(resp.status(), StatusCode::OK, "{path}");
    resp.json().await.unwrap()
}

/// A local post's text is escaped, its URL shortened, its hashtag and its
/// mention of a known account linked, and its lines made paragraphs.
#[tokio::test]
async fn test_local_status_content_is_text_formatted() {
    let ctx = TestContext::new("fmt-local-status").await;
    let status = ctx
        .api
        .post_status(
            &ctx.alice_token,
            "Hi @bob & @nobody <b>\nsee https://example.com/a/very/long/path/indeed #Fediverse\n\nbye",
            "public",
        )
        .await;
    let domain = &ctx.domain;
    assert_eq!(
        status["content"].as_str().unwrap(),
        format!(
            r#"<p>Hi <span class="h-card" translate="no"><a href="https://{domain}/@bob" class="u-url mention">@<span>bob</span></a></span> &amp; @nobody &lt;b&gt;<br />see <a href="https://example.com/a/very/long/path/indeed" target="_blank" rel="nofollow noopener" translate="no"><span class="invisible">https://</span><span class="ellipsis">example.com/a/very/long/path/i</span><span class="invisible">ndeed</span></a> <a href="https://{domain}/tags/Fediverse" class="mention hashtag" rel="tag">#<span>Fediverse</span></a></p><p>bye</p>"#
        )
    );
    assert_eq!(
        status["mentions"][0]["url"],
        format!("https://{domain}/@bob")
    );
}

/// A local post quoting another starts with the `RE:` fallback link.
#[tokio::test]
async fn test_a_local_quote_carries_the_fallback_link() {
    let ctx = TestContext::new("fmt-quote-fallback").await;
    let original = ctx
        .api
        .post_status(&ctx.bob_token, "original", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();
    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "so true", "quoted_status_id": original_id, "visibility": "public"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let quote: Value = resp.json().await.unwrap();
    let url = format!("https://{}/@bob/{original_id}", ctx.domain);
    let shown = format!(
        r#"<p class="quote-inline">RE: {}</p><p>so true</p>"#,
        eunha::formatter::text::shortened_link(&url, false)
    );
    assert!(shown.contains(&format!(r#"<a href="{url}" target="_blank""#)));
    assert_eq!(quote["content"].as_str().unwrap(), shown);
    let fetched = get_json(
        &ctx,
        &format!("/api/v1/statuses/{}", quote["id"].as_str().unwrap()),
    )
    .await;
    assert_eq!(fetched["content"].as_str().unwrap(), shown);
}

/// A remote account's bio and fields are sanitized, and a verified field is
/// shown as a link to the URL it was verified for.
#[tokio::test]
async fn test_remote_profiles_are_sanitized() {
    let ctx = TestContext::new("fmt-remote-profile").await;
    let id = insert_remote_account(
        &ctx,
        r#"<h2>Hi</h2><p class="x mention">a<script>alert(1)</script> <a href="javascript:alert(1)">b</a> <a href="https://remote.example/x" rel="tag">c</a></p>"#,
        json!([
            {"name": "Site", "value": r#"<a href="https://carol.example/" rel="me">https://carol.example/</a>"#,
             "verified_at": "2026-01-01T00:00:00Z"},
            {"name": "<b>Bold</b>", "value": r#"<em>x</em><img src="x"><a href="/rel">y</a>"#},
        ]),
    )
    .await;
    let account = get_json(&ctx, &format!("/api/v1/accounts/{id}")).await;
    assert_eq!(
        account["note"].as_str().unwrap(),
        r#"<p><strong>Hi</strong></p><p class="mention">a b <a href="https://remote.example/x" rel="nofollow noopener" target="_blank">c</a></p>"#
    );
    assert_eq!(
        account["fields"][0]["value"].as_str().unwrap(),
        r#"<a href="https://carol.example/" target="_blank" rel="nofollow noopener" translate="no"><span class="invisible">https://</span><span class="">carol.example/</span><span class="invisible"></span></a>"#
    );
    // A field's name is served as it is; a client escapes it.
    assert_eq!(
        account["fields"][1]["name"].as_str().unwrap(),
        "<b>Bold</b>"
    );
    assert_eq!(
        account["fields"][1]["value"].as_str().unwrap(),
        "<em>x</em>y"
    );
}

/// A local bio's mention links to the account it names; a field links
/// with `rel="me"` and shows the mention's domain.
#[tokio::test]
async fn test_local_bio_mentions_are_looked_up() {
    let ctx = TestContext::new("fmt-local-bio").await;
    insert_remote_account(&ctx, "", json!([])).await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    sqlx::query("UPDATE accounts SET note = $2, fields = $3 WHERE id = $1")
        .bind(alice_id)
        .bind("Friends: @bob, @carol@remote.example\nand @nobody")
        .bind(json!([{"name": "Web", "value": "https://alice.org @carol@remote.example"}]))
        .execute(&ctx.db)
        .await
        .unwrap();
    let domain = &ctx.domain;
    let account = get_json(&ctx, &format!("/api/v1/accounts/{alice_id}")).await;
    assert_eq!(
        account["note"].as_str().unwrap(),
        format!(
            r#"<p>Friends: <span class="h-card" translate="no"><a href="https://{domain}/@bob" class="u-url mention">@<span>bob</span></a></span>, <span class="h-card" translate="no"><a href="https://remote.example/@carol" class="u-url mention">@<span>carol</span></a></span><br />and @nobody</p>"#
        )
    );
    assert_eq!(
        account["fields"][0]["value"].as_str().unwrap(),
        r#"<a href="https://alice.org" target="_blank" rel="nofollow noopener me" translate="no"><span class="invisible">https://</span><span class="">alice.org</span><span class="invisible"></span></a> <span class="h-card" translate="no"><a href="https://remote.example/@carol" class="u-url mention">@<span>carol@remote.example</span></a></span>"#
    );

    // The same account, embedded in a status.
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "hello", "public")
        .await;
    let fetched = get_json(
        &ctx,
        &format!("/api/v1/statuses/{}", status["id"].as_str().unwrap()),
    )
    .await;
    assert_eq!(fetched["account"]["note"], account["note"]);
    assert_eq!(fetched["account"]["fields"], account["fields"]);

    // And as the actor's `summary` and `PropertyValue`.
    let actor: Value = ctx
        .api
        .ap_get("/users/alice", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(actor["summary"], account["note"]);
    assert_eq!(
        actor["attachment"][0]["value"],
        account["fields"][0]["value"]
    );
}

/// A remote post's HTML is sanitized with `MASTODON_STRICT`.
#[tokio::test]
async fn test_remote_status_content_is_sanitized() {
    let ctx = TestContext::new("fmt-remote-status").await;
    let account_id = insert_remote_account(&ctx, "", json!([])).await;
    let status_id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO statuses (id, account_id, uri, url, text, visibility, local, created_at, updated_at)
           VALUES ($1, $2, 'https://remote.example/statuses/1', 'https://remote.example/@carol/1',
                   $3, 0, false, now(), now())"#,
    )
    .bind(status_id)
    .bind(account_id)
    .bind(
        r#"<p>x<sup>2</sup> <math><semantics><mi>y</mi><annotation encoding="application/x-tex">y^2</annotation></semantics></math></p><div>z</div><a href="/tags/t" class="hashtag">#t</a>"#,
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let status = get_json(&ctx, &format!("/api/v1/statuses/{status_id}")).await;
    assert_eq!(status["content"].as_str().unwrap(), "<p>x2 $y^2$</p> z #t");
}
