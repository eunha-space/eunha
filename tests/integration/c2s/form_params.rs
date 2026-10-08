//! Request bodies as Rails reads them: every write endpoint takes a
//! form-encoded body (and the query string) as well as JSON, and casts
//! `"1"`, `"t"` and `"300"` as `ActiveModel` types do.

use reqwest::{Method, StatusCode};
use serde_json::{json, Value};

use crate::helpers::{make_admin, TestContext};

async fn form(
    ctx: &TestContext,
    method: Method,
    path: &str,
    token: &str,
    pairs: &[(&str, &str)],
) -> reqwest::Response {
    ctx.api
        .http
        .request(method, ctx.api.url(path))
        .header("host", &ctx.api.host)
        .bearer_auth(token)
        .form(pairs)
        .send()
        .await
        .unwrap()
}

async fn ok(response: reqwest::Response, what: &str) -> Value {
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{what}: {body}");
    serde_json::from_str(&body).unwrap_or(Value::Null)
}

/// Lists, follows, mutes, notes, filters, domain blocks, featured tags,
/// markers and the admin tag endpoint, each posted form-encoded.
#[tokio::test]
async fn test_write_endpoints_take_form_bodies() {
    let ctx = TestContext::new("form-params").await;
    let alice = ctx.alice_token.as_str();
    let bob_id = ctx.bob_id.as_str();

    // Follow with `reblogs=0`: Rails casts it false.
    let rel = ok(
        form(
            &ctx,
            Method::POST,
            &format!("/api/v1/accounts/{bob_id}/follow"),
            alice,
            &[("reblogs", "0"), ("notify", "t")],
        )
        .await,
        "follow",
    )
    .await;
    assert_eq!(rel["following"], json!(true));
    assert_eq!(rel["showing_reblogs"], json!(false));
    assert_eq!(rel["notifying"], json!(true));

    // Lists: create, update leaving the title, add and remove a member.
    let list = ok(
        form(
            &ctx,
            Method::POST,
            "/api/v1/lists",
            alice,
            &[("title", "Formed"), ("exclusive", "1")],
        )
        .await,
        "create list",
    )
    .await;
    assert_eq!(list["exclusive"], json!(true));
    let list_id = list["id"].as_str().unwrap().to_owned();
    let list = ok(
        form(
            &ctx,
            Method::PUT,
            &format!("/api/v1/lists/{list_id}"),
            alice,
            &[("replies_policy", "followed")],
        )
        .await,
        "update list",
    )
    .await;
    assert_eq!(list["title"], json!("Formed"));
    assert_eq!(list["replies_policy"], json!("followed"));
    assert_eq!(list["exclusive"], json!(true));
    ok(
        form(
            &ctx,
            Method::POST,
            &format!("/api/v1/lists/{list_id}/accounts"),
            alice,
            &[("account_ids[]", bob_id)],
        )
        .await,
        "add to list",
    )
    .await;
    let members = ok(
        ctx.api
            .get(&format!("/api/v1/lists/{list_id}/accounts"), Some(alice))
            .await,
        "list members",
    )
    .await;
    assert_eq!(members.as_array().unwrap().len(), 1);
    ok(
        form(
            &ctx,
            Method::DELETE,
            &format!("/api/v1/lists/{list_id}/accounts"),
            alice,
            &[("account_ids[]", bob_id)],
        )
        .await,
        "remove from list",
    )
    .await;

    // Mute without notifications, and a note.
    let rel = ok(
        form(
            &ctx,
            Method::POST,
            &format!("/api/v1/accounts/{bob_id}/mute"),
            alice,
            &[("notifications", "false"), ("duration", "0")],
        )
        .await,
        "mute",
    )
    .await;
    assert_eq!(rel["muting"], json!(true));
    assert_eq!(rel["muting_notifications"], json!(false));
    let rel = ok(
        form(
            &ctx,
            Method::POST,
            &format!("/api/v1/accounts/{bob_id}/note"),
            alice,
            &[("comment", "met at a conference")],
        )
        .await,
        "note",
    )
    .await;
    assert_eq!(rel["note"], json!("met at a conference"));

    // Filters v2, with nested keyword attributes by index.
    let filter = ok(
        form(
            &ctx,
            Method::POST,
            "/api/v2/filters",
            alice,
            &[
                ("title", "Spoilers"),
                ("context[]", "home"),
                ("context[]", "public"),
                ("filter_action", "hide"),
                ("expires_in", ""),
                ("keywords_attributes[0][keyword]", "finale"),
                ("keywords_attributes[0][whole_word]", "1"),
            ],
        )
        .await,
        "create filter",
    )
    .await;
    assert_eq!(filter["context"], json!(["home", "public"]));
    assert_eq!(filter["expires_at"], Value::Null);
    assert_eq!(filter["keywords"][0]["keyword"], json!("finale"));
    assert_eq!(filter["keywords"][0]["whole_word"], json!(true));
    let filter_id = filter["id"].as_str().unwrap().to_owned();
    ok(
        form(
            &ctx,
            Method::PUT,
            &format!("/api/v2/filters/{filter_id}"),
            alice,
            &[("title", "Spoilers!"), ("context[]", "home")],
        )
        .await,
        "update filter",
    )
    .await;
    let keyword = ok(
        form(
            &ctx,
            Method::POST,
            &format!("/api/v2/filters/{filter_id}/keywords"),
            alice,
            &[("keyword", "ending"), ("whole_word", "f")],
        )
        .await,
        "create keyword",
    )
    .await;
    assert_eq!(keyword["whole_word"], json!(false));
    let keyword_id = keyword["id"].as_str().unwrap().to_owned();
    let keyword = ok(
        form(
            &ctx,
            Method::PUT,
            &format!("/api/v2/filters/keywords/{keyword_id}"),
            alice,
            &[("keyword", "ending"), ("whole_word", "on")],
        )
        .await,
        "update keyword",
    )
    .await;
    assert_eq!(keyword["whole_word"], json!(true));
    let status = ctx
        .api
        .post_status(&ctx.bob_token, "the finale", "public")
        .await;
    ok(
        form(
            &ctx,
            Method::POST,
            &format!("/api/v2/filters/{filter_id}/statuses"),
            alice,
            &[("status_id", status["id"].as_str().unwrap())],
        )
        .await,
        "filter a status",
    )
    .await;

    // Filters v1.
    let v1 = ok(
        form(
            &ctx,
            Method::POST,
            "/api/v1/filters",
            alice,
            &[
                ("phrase", "cliffhanger"),
                ("context[]", "home"),
                ("irreversible", "true"),
                ("whole_word", "0"),
            ],
        )
        .await,
        "create v1 filter",
    )
    .await;
    assert_eq!(v1["irreversible"], json!(true));
    assert_eq!(v1["whole_word"], json!(false));
    let v1_id = v1["id"].as_str().unwrap().to_owned();
    ok(
        form(
            &ctx,
            Method::PUT,
            &format!("/api/v1/filters/{v1_id}"),
            alice,
            &[("phrase", "cliffhangers"), ("context[]", "home")],
        )
        .await,
        "update v1 filter",
    )
    .await;

    // Domain blocks: a form body, then the domain in the query string.
    ok(
        form(
            &ctx,
            Method::POST,
            "/api/v1/domain_blocks",
            alice,
            &[("domain", "blocked.example")],
        )
        .await,
        "block domain",
    )
    .await;
    ok(
        form(
            &ctx,
            Method::DELETE,
            "/api/v1/domain_blocks?domain=blocked.example",
            alice,
            &[],
        )
        .await,
        "unblock domain",
    )
    .await;

    // Featured tags.
    let tag = ok(
        form(
            &ctx,
            Method::POST,
            "/api/v1/featured_tags",
            alice,
            &[("name", "formed")],
        )
        .await,
        "feature tag",
    )
    .await;
    assert_eq!(tag["name"], json!("formed"));

    // Markers, by bracketed names.
    let markers = ok(
        form(
            &ctx,
            Method::POST,
            "/api/v1/markers",
            alice,
            &[("home[last_read_id]", status["id"].as_str().unwrap())],
        )
        .await,
        "markers",
    )
    .await;
    assert_eq!(markers["home"]["last_read_id"], status["id"]);

    // Aliases and moves answer as Rails would, never a 415.
    for path in [
        "/api/v1/profile/aliases",
        "/api/v1/accounts/move",
        "/api/v1/accounts/redirect",
    ] {
        let response = form(&ctx, Method::POST, path, alice, &[("acct", "")]).await;
        assert_ne!(
            response.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{path}"
        );
    }

    // Collections.
    let collection = ok(
        form(
            &ctx,
            Method::POST,
            "/api/v1/collections",
            alice,
            &[("name", "Formed people"), ("discoverable", "1")],
        )
        .await,
        "create collection",
    )
    .await;
    let collection_id = collection["collection"]["id"].as_str().unwrap().to_owned();
    let collection = ok(
        form(
            &ctx,
            Method::PATCH,
            &format!("/api/v1/collections/{collection_id}"),
            alice,
            &[("description", "people, formed")],
        )
        .await,
        "update collection",
    )
    .await;
    assert_eq!(
        collection["collection"]["description"],
        json!("people, formed")
    );
    let response = form(
        &ctx,
        Method::POST,
        &format!("/api/v1/collections/{collection_id}/items"),
        alice,
        &[("account_id", bob_id)],
    )
    .await;
    assert_ne!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // The admin tag endpoint.
    make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    let tag_id: i64 = sqlx::query_scalar("SELECT id FROM tags WHERE name = 'formed'")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    let admin_tag = ok(
        form(
            &ctx,
            Method::PUT,
            &format!("/api/v1/admin/tags/{tag_id}"),
            alice,
            &[("trendable", "1"), ("usable", "0")],
        )
        .await,
        "admin tag",
    )
    .await;
    assert_eq!(admin_tag["trendable"], json!(true));
    assert_eq!(admin_tag["usable"], json!(false));
}

/// Posting, editing, voting, reblogging and rescheduling, form-encoded,
/// multipart, and as JSON whose values are strings.
#[tokio::test]
async fn test_status_endpoints_take_form_bodies() {
    let ctx = TestContext::new("form-statuses").await;
    let alice = ctx.alice_token.as_str();

    let status = ok(
        form(
            &ctx,
            Method::POST,
            "/api/v1/statuses?visibility=unlisted",
            alice,
            &[
                ("status", "which one?"),
                ("sensitive", "1"),
                ("spoiler_text", "poll"),
                ("poll[options][]", "this"),
                ("poll[options][]", "that"),
                ("poll[expires_in]", "300"),
                ("poll[multiple]", "t"),
            ],
        )
        .await,
        "post a poll",
    )
    .await;
    assert_eq!(status["sensitive"], json!(true));
    assert_eq!(status["visibility"], json!("unlisted"));
    assert_eq!(status["poll"]["multiple"], json!(true));
    assert_eq!(status["poll"]["options"].as_array().unwrap().len(), 2);
    let status_id = status["id"].as_str().unwrap().to_owned();
    let poll_id = status["poll"]["id"].as_str().unwrap().to_owned();

    let poll = ok(
        form(
            &ctx,
            Method::POST,
            &format!("/api/v1/polls/{poll_id}/votes"),
            &ctx.bob_token,
            &[("choices[]", "0"), ("choices[]", "1")],
        )
        .await,
        "vote",
    )
    .await;
    assert_eq!(poll["own_votes"], json!([0, 1]));

    let reblog = ok(
        form(
            &ctx,
            Method::POST,
            &format!("/api/v1/statuses/{status_id}/reblog"),
            &ctx.bob_token,
            &[("visibility", "private")],
        )
        .await,
        "reblog",
    )
    .await;
    assert_eq!(reblog["visibility"], json!("private"));

    let plain = ctx.api.post_status(alice, "first draft", "public").await;
    let edited = ok(
        form(
            &ctx,
            Method::PUT,
            &format!("/api/v1/statuses/{}", plain["id"].as_str().unwrap()),
            alice,
            &[("status", "second draft"), ("sensitive", "true")],
        )
        .await,
        "edit",
    )
    .await;
    assert_eq!(edited["content"], json!("<p>second draft</p>"));
    assert_eq!(edited["sensitive"], json!(true));

    // Multipart, as most mobile clients post.
    let multipart = reqwest::multipart::Form::new()
        .text("status", "from a phone")
        .text("sensitive", "true")
        .text("poll[options][]", "yes")
        .text("poll[options][]", "no")
        .text("poll[expires_in]", "600");
    let response = ctx
        .api
        .http
        .post(ctx.api.url("/api/v1/statuses"))
        .header("host", &ctx.api.host)
        .bearer_auth(alice)
        .multipart(multipart)
        .send()
        .await
        .unwrap();
    let posted = ok(response, "multipart post").await;
    assert_eq!(posted["sensitive"], json!(true));
    assert_eq!(posted["poll"]["options"].as_array().unwrap().len(), 2);

    // JSON whose booleans and integers are strings.
    let posted = ok(
        ctx.api
            .post_json(
                "/api/v1/statuses",
                Some(alice),
                &json!({
                    "status": "stringly typed",
                    "sensitive": "true",
                    "poll": {"options": ["a", "b"], "expires_in": "300", "hide_totals": "1"},
                }),
            )
            .await,
        "stringly JSON",
    )
    .await;
    assert_eq!(posted["sensitive"], json!(true));
    assert!(posted["poll"].is_object());

    // A scheduled status, rescheduled by form.
    let at = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
    let scheduled = ok(
        form(
            &ctx,
            Method::POST,
            "/api/v1/statuses",
            alice,
            &[("status", "later"), ("scheduled_at", &at)],
        )
        .await,
        "schedule",
    )
    .await;
    let later = (chrono::Utc::now() + chrono::Duration::hours(3)).to_rfc3339();
    ok(
        form(
            &ctx,
            Method::PUT,
            &format!(
                "/api/v1/scheduled_statuses/{}",
                scheduled["id"].as_str().unwrap()
            ),
            alice,
            &[("scheduled_at", &later)],
        )
        .await,
        "reschedule",
    )
    .await;
}

/// Query booleans cast as `truthy_param?` casts them.
#[tokio::test]
async fn test_query_booleans_take_rails_values() {
    let ctx = TestContext::new("form-query").await;
    ctx.api
        .post_status(&ctx.alice_token, "hello", "public")
        .await;
    for path in [
        format!(
            "/api/v1/accounts/{}/statuses?exclude_replies=1&only_media=0&pinned=f",
            ctx.alice_id
        ),
        "/api/v1/timelines/public?local=t&only_media=off".to_owned(),
        "/api/v1/timelines/tag/hello?local=1&any[]=x".to_owned(),
        "/api/v1/directory?local=1&limit=5&offset=0".to_owned(),
        "/api/v1/accounts/lookup?acct=alice&skip_webfinger=1".to_owned(),
    ] {
        let response = ctx.api.get(&path, Some(&ctx.alice_token)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses?only_media=1", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(statuses.is_empty(), "only_media=1 is true: {statuses:?}");
}
