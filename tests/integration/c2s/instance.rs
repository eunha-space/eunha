use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

/// GET /api/v1/instance: `REST::V1::InstanceSerializer`.
#[tokio::test]
async fn test_instance_v1() {
    let ctx = TestContext::new("instance-v1").await;

    let v1 = || async {
        let resp = ctx.api.get("/api/v1/instance", None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        resp.json::<Value>().await.unwrap()
    };
    let body = v1().await;
    assert_eq!(body["uri"], json!(ctx.domain));
    assert!(body["title"].as_str().is_some(), "title field missing");
    assert!(body["version"].as_str().is_some(), "version field missing");
    // `streaming_api_base_url`, which clients add the path to.
    assert_eq!(
        body["urls"]["streaming_api"],
        json!(format!("wss://{}", ctx.domain))
    );
    // `[I18n.default_locale]`.
    assert_eq!(body["languages"], json!(["en"]));
    // `frontend_asset_url('images/preview.png')` with no thumbnail uploaded.
    assert_eq!(
        body["thumbnail"],
        json!(format!("https://{}/images/preview.png", ctx.domain))
    );
    // `UserRole.everyone.can?(:invite_users)`, which it can by default.
    assert_eq!(body["invites_enabled"], json!(true));
    // `User.confirmed.joins(:account).merge(Account.without_suspended)`.
    let users = body["stats"]["user_count"].as_i64().unwrap();
    assert!(users >= 2, "alice and bob are confirmed: {body}");

    sqlx::query("UPDATE user_roles SET permissions = 0 WHERE id = -99")
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    // `Instance.count`: the `instances` view, a blocked domain with no
    // accounts included once it is refreshed.
    let domains_before = body["stats"]["domain_count"].as_i64().unwrap();
    sqlx::query(
        "INSERT INTO domain_blocks (domain, severity, created_at, updated_at)
         VALUES ('blocked.example', 1, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query("REFRESH MATERIALIZED VIEW instances")
        .execute(&ctx.db)
        .await
        .unwrap();

    let body = v1().await;
    assert_eq!(body["invites_enabled"], json!(false));
    assert_eq!(body["stats"]["user_count"].as_i64().unwrap(), users - 1);
    assert_eq!(
        body["stats"]["domain_count"].as_i64().unwrap(),
        domains_before + 1
    );
}

/// GET /api/v2/instance: `REST::InstanceSerializer`.
#[tokio::test]
async fn test_instance_v2() {
    let ctx = TestContext::new("instance-v2").await;

    let resp = ctx.api.get("/api/v2/instance", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    assert_eq!(body["domain"], json!(ctx.domain));
    assert!(body["version"].as_str().is_some(), "version field missing");
    assert_eq!(body["api_versions"], json!({"mastodon": 11}));
    assert_eq!(body["languages"], json!(["en"]));
    assert_eq!(
        body["configuration"]["urls"]["streaming"],
        json!(format!("wss://{}", ctx.domain))
    );
    // No thumbnail uploaded: the frontend's picture and its description,
    // without a blurhash or versions.
    assert_eq!(
        body["thumbnail"],
        json!({
            "url": format!("https://{}/images/preview.png", ctx.domain),
            "description": eunha::api::mastodon::instance::DEFAULT_THUMBNAIL_DESCRIPTION,
        })
    );
    // No app icon uploaded: every `ANDROID_ICON_SIZES` entry, from the
    // frontend.
    let icons = body["icon"].as_array().unwrap();
    let sizes: Vec<&str> = icons.iter().map(|i| i["size"].as_str().unwrap()).collect();
    assert_eq!(
        sizes,
        [
            "36x36", "48x48", "72x72", "96x96", "144x144", "192x192", "256x256", "384x384",
            "512x512"
        ]
    );
    assert_eq!(
        icons[0]["src"],
        json!(format!(
            "https://{}/icons/android-chrome-36x36.png",
            ctx.domain
        ))
    );
    // `AnnualReport.current_campaign`: the year from 10 December, while the
    // `wrapstodon` setting is on.
    let now = chrono::Utc::now();
    use chrono::Datelike;
    let campaign = if now.month() == 12 && now.day() >= 10 {
        json!(now.year())
    } else {
        Value::Null
    };
    assert_eq!(body["wrapstodon"], campaign);
    crate::helpers::set_setting(&ctx.db, "wrapstodon", "false").await;
    let body: Value = ctx
        .api
        .get("/api/v2/instance", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["wrapstodon"], Value::Null);
}

/// The pictures the instance API names when nothing was uploaded are served
/// by the web frontend.
#[test]
fn test_instance_default_images_exist() {
    let public = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("frontend/public");
    assert!(public.join("images/preview.png").is_file());
    for size in [36, 48, 72, 96, 144, 192, 256, 384, 512] {
        assert!(
            public
                .join(format!("icons/android-chrome-{size}x{size}.png"))
                .is_file(),
            "missing the {size}px icon"
        );
    }
}

/// GET /api/v1/instance/extended_description returns 200.
#[tokio::test]
async fn test_instance_extended_description() {
    let ctx = TestContext::new("inst-ext").await;

    let resp = ctx
        .api
        .get("/api/v1/instance/extended_description", None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// GET /api/v1/instance/peers returns an array.
#[tokio::test]
async fn test_instance_peers() {
    let ctx = TestContext::new("inst-peers").await;

    let resp = ctx.api.get("/api/v1/instance/peers", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _: Vec<String> = resp.json().await.unwrap();
}

/// GET /api/v1/instance/privacy_policy returns 200.
#[tokio::test]
async fn test_instance_privacy_policy() {
    let ctx = TestContext::new("inst-priv").await;

    let resp = ctx.api.get("/api/v1/instance/privacy_policy", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// GET /api/v1/instance/translation_languages returns 200.
#[tokio::test]
async fn test_instance_translation_languages() {
    let ctx = TestContext::new("inst-trans").await;

    let resp = ctx
        .api
        .get("/api/v1/instance/translation_languages", None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// POST /api/v1/apps registers an application and returns credentials.
#[tokio::test]
async fn test_register_app() {
    let ctx = TestContext::new("oauth-app").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &json!({
                "client_name": "Test App",
                "redirect_uris": "urn:ietf:wg:oauth:2.0:oob",
                "scopes": "read write"
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    assert!(body["client_id"].as_str().is_some(), "client_id missing");
    assert!(
        body["client_secret"].as_str().is_some(),
        "client_secret missing"
    );
    assert_eq!(body["name"].as_str(), Some("Test App"));
}

/// GET /api/v1/announcements returns an array (empty when none published).
#[tokio::test]
async fn test_get_announcements() {
    let ctx = TestContext::new("announce").await;

    let resp = ctx
        .api
        .get("/api/v1/announcements", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _: Vec<Value> = resp.json().await.unwrap();
}

/// GET /api/v1/custom_emojis returns a JSON array.
#[tokio::test]
async fn test_get_custom_emojis() {
    let ctx = TestContext::new("emojis").await;

    let resp = ctx.api.get("/api/v1/custom_emojis", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _: Vec<Value> = resp.json().await.unwrap();
}

/// GET /api/v1/trends/tags returns a JSON array.
#[tokio::test]
async fn test_trending_tags() {
    let ctx = TestContext::new("trends-tags").await;

    let resp = ctx.api.get("/api/v1/trends/tags", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _: Vec<Value> = resp.json().await.unwrap();
}

/// GET /api/v1/trends/statuses returns a JSON array.
#[tokio::test]
async fn test_trending_statuses() {
    let ctx = TestContext::new("trends-stat").await;

    let resp = ctx.api.get("/api/v1/trends/statuses", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _: Vec<Value> = resp.json().await.unwrap();
}

/// GET /api/v1/trends/links returns a JSON array.
#[tokio::test]
async fn test_trending_links() {
    let ctx = TestContext::new("trends-links").await;

    let resp = ctx.api.get("/api/v1/trends/links", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _: Vec<Value> = resp.json().await.unwrap();
}

/// POST /api/v1/emails/confirmations is for the app a user signed up
/// through (`require_user_owned_by_application!`); alice's token is not one.
/// The rest is in `surfaces::sign_up`.
#[tokio::test]
async fn test_email_confirmations_endpoint() {
    let ctx = TestContext::new("email-confirm").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/emails/confirmations",
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        "This method is only available to the application the user originally signed-up with"
    );
}

/// GET /api/v1/announcements returns read=false before dismissal, read=true after.
#[tokio::test]
async fn test_announcement_dismiss() {
    let ctx = TestContext::new("ann-dismiss").await;

    // Insert a published announcement via direct DB write.
    let pool = ctx.db.clone();

    let ann_id: i64 = sqlx::query_scalar!(
        r#"INSERT INTO announcements (text, published, all_day, published_at, created_at, updated_at)
           VALUES ('test announcement', true, false, now(), now(), now())
           RETURNING id"#,
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // Get announcements — should appear as unread.
    let before: Vec<Value> = ctx
        .api
        .get("/api/v1/announcements", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let ann = before
        .iter()
        .find(|a| a["id"].as_str().and_then(|s| s.parse::<i64>().ok()) == Some(ann_id));
    assert!(ann.is_some(), "announcement should appear in list");
    assert_eq!(
        ann.unwrap()["read"].as_bool(),
        Some(false),
        "should be unread initially"
    );

    // Dismiss it.
    let dismiss_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/announcements/{ann_id}/dismiss"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(dismiss_resp.status(), StatusCode::OK);

    // After dismissal it should appear as read.
    let after: Vec<Value> = ctx
        .api
        .get("/api/v1/announcements", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let ann2 = after
        .iter()
        .find(|a| a["id"].as_str().and_then(|s| s.parse::<i64>().ok()) == Some(ann_id));
    assert!(
        ann2.is_some(),
        "announcement should still appear after dismiss"
    );
    assert_eq!(
        ann2.unwrap()["read"].as_bool(),
        Some(true),
        "should be read after dismiss"
    );
}

/// GET /api/v1/instance/activity: twelve weeks from `ActivityTracker`, the
/// first ending today, counting local public and unlisted posts but not
/// private ones, rendered once and then served from the cache for a day.
#[tokio::test]
async fn test_instance_activity_counts_from_activity_tracker() {
    let ctx = TestContext::new("inst-activity").await;

    ctx.api
        .post_status(&ctx.alice_token, "some activity", "public")
        .await;
    ctx.api
        .post_status(&ctx.alice_token, "more activity", "unlisted")
        .await;
    ctx.api
        .post_status(&ctx.alice_token, "followers only", "private")
        .await;

    let resp = ctx.api.get("/api/v1/instance/activity", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 12, "twelve weeks, always: {body:?}");
    for entry in &body {
        for field in ["week", "statuses", "logins", "registrations"] {
            assert!(entry[field].is_string(), "{field} should be a string");
        }
    }
    assert_eq!(body[0]["statuses"], "2", "{body:?}");
    let week: i64 = body[0]["week"].as_str().unwrap().parse().unwrap();
    assert!((chrono::Utc::now().timestamp() - week).abs() < 60);
    assert_eq!(body[1]["statuses"], "0");
    let week_before: i64 = body[1]["week"].as_str().unwrap().parse().unwrap();
    assert_eq!(week - week_before, 7 * 24 * 60 * 60);

    // Cached, as `render_with_cache` keeps it, under Mastodon's key.
    ctx.api
        .post_status(&ctx.alice_token, "after the cache", "public")
        .await;
    let again: Vec<Value> = ctx
        .api
        .get("/api/v1/instance/activity", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(again, body);
    let mut redis = ctx.state.redis.clone();
    let cached: Option<String> = redis::cmd("GET")
        .arg(
            ctx.state
                .redis_keys
                .key("cache:api/v1/instances/activity/show"),
        )
        .query_async(&mut redis)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<Value>>(&cached.unwrap()).unwrap(),
        body
    );
}

/// The day's interactions, as `ActivityTracker` counts them: a favourite, a
/// boost, a follow and a reply to someone else, each once.
#[tokio::test]
async fn test_interactions_are_counted_in_activity_tracker() {
    let ctx = TestContext::new("inst-interactions").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "interact with me", "public")
        .await;
    let id = status["id"].as_str().unwrap();
    let interactions = || async {
        eunha::activity_tracker::sum(
            &ctx.state,
            eunha::activity_tracker::INTERACTIONS,
            eunha::activity_tracker::Kind::Basic,
            chrono::Utc::now().date_naive(),
            chrono::Utc::now().date_naive(),
        )
        .await
        .unwrap()
    };
    assert_eq!(interactions().await, 0);

    for action in ["favourite", "favourite", "reblog"] {
        let resp = ctx
            .api
            .post_json(
                &format!("/api/v1/statuses/{id}/{action}"),
                Some(&ctx.bob_token),
                &json!({}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
    // The second favourite is the one already made.
    assert_eq!(interactions().await, 2);

    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    assert_eq!(interactions().await, 3);

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({ "status": "a reply", "in_reply_to_id": id }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // A reply to one's own post is no interaction.
    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({ "status": "my own thread", "in_reply_to_id": id }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(interactions().await, 4);
}

/// GET /api/v1/instance/rules returns an array (may be empty).
#[tokio::test]
async fn test_instance_rules_returns_array() {
    let ctx = TestContext::new("inst-rules").await;

    let resp = ctx.api.get("/api/v1/instance/rules", None).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.is_array(),
        "instance/rules should return an array, got: {body:?}"
    );
}

/// GET /api/v1/peers/search?q= returns matching peer domains.
#[tokio::test]
async fn test_peers_search_returns_array() {
    let ctx = TestContext::new("peers-search").await;

    let resp = ctx.api.get("/api/v1/peers/search?q=test", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    let _ = body; // just verify it returns valid JSON array
}

/// `Api::CachingConcern`: the instance may be cached five minutes by anyone,
/// a status fifteen seconds when nobody is signed in, and anything else is
/// `private, no-store`, as `set_cache_control_defaults` leaves it.
#[tokio::test]
async fn test_api_cache_control() {
    let ctx = TestContext::new("api-cache-control").await;
    let cache_control = |resp: &reqwest::Response| {
        resp.headers()
            .get("cache-control")
            .map(|v| v.to_str().unwrap().to_owned())
    };

    let resp = ctx
        .api
        .get("/api/v2/instance", Some(&ctx.alice_token))
        .await;
    assert_eq!(
        cache_control(&resp).as_deref(),
        Some("max-age=300, public, stale-while-revalidate=30, stale-if-error=86400")
    );

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "cache me", "public")
        .await;
    let path = format!("/api/v1/statuses/{}", status["id"].as_str().unwrap());
    let resp = ctx.api.get(&path, None).await;
    assert_eq!(
        cache_control(&resp).as_deref(),
        Some("max-age=15, public, stale-while-revalidate=30, stale-if-error=86400")
    );
    let resp = ctx.api.get(&path, Some(&ctx.bob_token)).await;
    assert_eq!(cache_control(&resp).as_deref(), Some("private, no-store"));

    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.bob_token))
        .await;
    assert_eq!(cache_control(&resp).as_deref(), Some("private, no-store"));
    // A refusal is not cached either.
    let resp = ctx.api.get("/api/v1/statuses/1", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(cache_control(&resp).as_deref(), Some("private, no-store"));
}

/// `vary_by`: an API response varies by `Authorization`, as
/// `Api::BaseController` says, except where its controller says otherwise:
/// nothing for the instance and its subresources, the trending posts and
/// links by `Accept-Language` too.
#[tokio::test]
async fn test_api_vary() {
    let ctx = TestContext::new("api-vary").await;
    // What the API says it varies by, beside the `Accept-Encoding` that
    // compression adds and the `Origin` and preflight headers CORS does.
    let vary = async |path: &str| {
        let resp = ctx.api.get(path, None).await;
        let vary: Vec<String> = resp
            .headers()
            .get_all("vary")
            .iter()
            .flat_map(|v| v.to_str().unwrap().split(','))
            .map(str::trim)
            .filter(|v| {
                let v = v.to_ascii_lowercase();
                v != "accept-encoding" && v != "origin" && !v.starts_with("access-control-")
            })
            .map(str::to_owned)
            .collect();
        (!vary.is_empty()).then(|| vary.join(", "))
    };
    assert_eq!(vary("/api/v2/instance").await, None);
    assert_eq!(vary("/api/v1/instance/rules").await, None);
    assert_eq!(vary("/api/v1/custom_emojis").await, None);
    assert_eq!(
        vary("/api/v1/timelines/public").await.as_deref(),
        Some("Authorization")
    );
    assert_eq!(
        vary("/api/v1/trends/statuses").await.as_deref(),
        Some("Authorization, Accept-Language")
    );
    assert_eq!(
        vary("/api/v1/instance/domain_blocks").await.as_deref(),
        Some("Authorization")
    );
}
