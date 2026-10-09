//! The preferences Mastodon keeps only on its web settings pages.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{set_setting, user_id_for, TestContext};

async fn preferences(ctx: &TestContext) -> Value {
    ctx.api
        .get("/api/eunha/v1/preferences", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap()
}

async fn patch(ctx: &TestContext, body: Value) -> reqwest::Response {
    ctx.api
        .patch_json("/api/eunha/v1/preferences", Some(&ctx.alice_token), &body)
        .await
}

#[tokio::test]
async fn test_preferences_default_and_change() {
    let ctx = TestContext::new("preferences").await;
    let alice_user = user_id_for(&ctx.db, ctx.alice_id.parse().unwrap()).await;

    let prefs = preferences(&ctx).await;
    assert_eq!(prefs["noindex"], false);
    assert_eq!(prefs["show_application"], true);
    assert!(prefs["chosen_languages"].is_null());
    assert_eq!(prefs["notification_emails"]["report"], true);
    assert_eq!(prefs["notification_emails"]["software_updates"], "critical");
    // `UserSettings`' defaults for the notification emails.
    assert_eq!(prefs["notification_emails"]["follow"], true);
    assert_eq!(prefs["notification_emails"]["mention"], true);
    assert_eq!(prefs["notification_emails"]["quote"], true);
    assert_eq!(prefs["notification_emails"]["follow_request"], true);
    assert_eq!(prefs["notification_emails"]["favourite"], false);
    assert_eq!(prefs["notification_emails"]["reblog"], false);
    assert_eq!(prefs["always_send_emails"], false);

    let changed: Value = patch(
        &ctx,
        json!({
            "chosen_languages": ["en", "xx", "ko"],
            "locale": "ko",
            "notification_emails": { "report": false, "software_updates": "all" },
        }),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(changed["chosen_languages"], json!(["en", "ko"]));
    assert_eq!(changed["locale"], "ko");
    assert_eq!(changed["notification_emails"]["report"], false);
    assert_eq!(changed["notification_emails"]["software_updates"], "all");
    // Stored where Mastodon keeps them, and only what was named changed.
    let settings: String = sqlx::query_scalar("SELECT settings FROM users WHERE id = $1")
        .bind(alice_user)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    let settings: Value = serde_json::from_str(&settings).unwrap();
    assert_eq!(settings["notification_emails.report"], false);
    assert!(settings.get("show_application").is_none());

    let cleared: Value = patch(&ctx, json!({ "chosen_languages": [], "locale": "zz-ZZ" }))
        .await
        .json()
        .await
        .unwrap();
    assert!(cleared["chosen_languages"].is_null());
    assert!(cleared["locale"].is_null());
    // Only `I18n.available_locales`, spelled as Mastodon spells them: not a
    // language it knows but is not offered in, nor a region of one it is.
    for locale in ["pt-BR", "ab", "en-US", "EN"] {
        let changed: Value = patch(&ctx, json!({ "locale": locale }))
            .await
            .json()
            .await
            .unwrap();
        let expected = if locale == "pt-BR" {
            json!("pt-BR")
        } else {
            Value::Null
        };
        assert_eq!(changed["locale"], expected, "{locale}");
    }

    let refused = patch(
        &ctx,
        json!({ "notification_emails": { "software_updates": "sometimes" } }),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn test_noindex_reaches_the_account_entity() {
    let ctx = TestContext::new("preferences-noindex").await;
    let account: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.alice_id), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(account["noindex"], false);

    // The privacy form posts `indexable`, the inverse.
    let prefs: Value = patch(&ctx, json!({ "indexable": false }))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(prefs["noindex"], true);
    let account: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.alice_id), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(account["noindex"], true);
    let me: Value = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me["noindex"], true);
    // And on the account a status embeds.
    ctx.api
        .post_status(&ctx.alice_token, "hello", "public")
        .await;
    let timeline = ctx.api.public_timeline().await;
    assert_eq!(timeline[0]["account"]["noindex"], true);

    // Someone who never chose follows `Setting.noindex`.
    set_setting(&ctx.db, "noindex", "true").await;
    let bob: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.bob_id), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(bob["noindex"], true);
}

#[tokio::test]
async fn test_show_application_hides_the_app_from_others() {
    let ctx = TestContext::new("preferences-show-app").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "from my app", "public")
        .await;
    let id = status["id"].as_str().unwrap().to_string();
    let path = format!("/api/v1/statuses/{id}");

    let seen: Value = ctx
        .api
        .get(&path, Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(seen["application"]["name"], "test");

    patch(&ctx, json!({ "show_application": false })).await;
    let hidden: Value = ctx
        .api
        .get(&path, Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(hidden["application"].is_null());
    let anonymous: Value = ctx.api.get(&path, None).await.json().await.unwrap();
    assert!(anonymous["application"].is_null());
    let timeline = ctx.api.public_timeline().await;
    let in_timeline = timeline.iter().find(|s| s["id"] == id.as_str()).unwrap();
    assert!(in_timeline["application"].is_null());

    // The author still sees it.
    let own: Value = ctx
        .api
        .get(&path, Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(own["application"]["name"], "test");
}

#[tokio::test]
async fn test_default_sensitive_is_stored_under_mastodons_key() {
    let ctx = TestContext::new("preferences-sensitive").await;
    let alice_user = user_id_for(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    ctx.api
        .patch_json(
            "/api/v1/accounts/update_credentials",
            Some(&ctx.alice_token),
            &json!({ "source": { "sensitive": true } }),
        )
        .await;
    let settings: String = sqlx::query_scalar("SELECT settings FROM users WHERE id = $1")
        .bind(alice_user)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    let settings: Value = serde_json::from_str(&settings).unwrap();
    assert_eq!(settings["default_sensitive"], true);
    let prefs: Value = ctx
        .api
        .get("/api/v1/preferences", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(prefs["posting:default:sensitive"], true);
}

#[tokio::test]
async fn test_time_zone_preference() {
    let ctx = TestContext::new("preferences-time-zone").await;
    assert!(preferences(&ctx).await["time_zone"].is_null());

    let choices: Value = ctx
        .api
        .get(
            "/api/eunha/v1/preferences/time_zones",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let choices = choices.as_array().unwrap();
    assert_eq!(choices[0]["value"], "Etc/GMT+12");
    assert!(choices
        .iter()
        .any(|c| c["value"] == "Asia/Seoul" && c["label"] == "(GMT+09:00) Seoul"));

    for (asked, kept) in [
        ("Asia/Seoul", Some("Asia/Seoul")),
        ("Tokyo", Some("Tokyo")),
        // `normalizes :time_zone`: a name Rails cannot find clears it.
        ("Atlantis/Lost", None),
    ] {
        let changed: Value = patch(&ctx, json!({ "time_zone": asked }))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(changed["time_zone"].as_str(), kept, "{asked}");
    }
}

/// `GET /api/v1/preferences` as `REST::PreferencesSerializer` serves it: the
/// reading preferences are the `web.*` settings the appearance page sets,
/// and the posting language is `User#preferred_posting_language`, the chosen
/// one, else the interface language, else the request's.
#[tokio::test]
async fn test_mastodon_preferences_read_the_web_settings() {
    let ctx = TestContext::new("preferences-reading").await;
    let alice_user = user_id_for(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    let mastodon = |query: &'static str, accept_language: Option<&'static str>| {
        let api = &ctx.api;
        let token = ctx.alice_token.clone();
        async move {
            let mut request = api
                .http
                .get(api.url(&format!("/api/v1/preferences{query}")))
                .header("host", &api.host)
                .bearer_auth(token);
            if let Some(accept_language) = accept_language {
                request = request.header("accept-language", accept_language);
            }
            request.send().await.unwrap().json::<Value>().await.unwrap()
        }
    };

    let prefs = mastodon("", None).await;
    assert_eq!(prefs["reading:expand:media"], "default");
    assert_eq!(prefs["reading:expand:spoilers"], false);
    assert_eq!(prefs["reading:autoplay:gifs"], false);
    assert_eq!(prefs["posting:default:language"], "en", "the site's");
    assert_eq!(
        mastodon("?lang=ja", None).await["posting:default:language"],
        "ja"
    );
    assert_eq!(
        mastodon("", Some("de-DE,de;q=0.9")).await["posting:default:language"],
        "de"
    );

    let changed: Value = patch(
        &ctx,
        json!({
            "display_media": "show_all",
            "expand_content_warnings": true,
            "auto_play": true,
            "locale": "ko",
        }),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(changed["display_media"], "show_all");
    assert_eq!(changed["expand_content_warnings"], true);
    assert_eq!(changed["auto_play"], true);
    let settings: String = sqlx::query_scalar("SELECT settings FROM users WHERE id = $1")
        .bind(alice_user)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    let settings: Value = serde_json::from_str(&settings).unwrap();
    assert_eq!(settings["web.display_media"], "show_all");
    assert_eq!(settings["web.expand_content_warnings"], true);
    assert_eq!(settings["web.auto_play"], true);
    assert_eq!(
        patch(&ctx, json!({ "display_media": "sometimes" }))
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );

    let prefs = mastodon("?lang=ja", None).await;
    assert_eq!(prefs["reading:expand:media"], "show_all");
    assert_eq!(prefs["reading:expand:spoilers"], true);
    assert_eq!(prefs["reading:autoplay:gifs"], true);
    assert_eq!(
        prefs["posting:default:language"], "ko",
        "the interface language before the request's"
    );

    ctx.api
        .patch_json(
            "/api/v1/accounts/update_credentials",
            Some(&ctx.alice_token),
            &json!({ "source": { "language": "fr" } }),
        )
        .await;
    assert_eq!(mastodon("", None).await["posting:default:language"], "fr");
}
