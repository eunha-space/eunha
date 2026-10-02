//! The server administration Mastodon has only as server-rendered admin
//! pages, as eunha serves it over REST: the server settings and site uploads,
//! and what the settings drive, here; the rest in the modules below.

mod announcements;
mod instances;
mod invites;
mod relays;
mod roles;
mod rules;

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

fn id(s: &str) -> i64 {
    s.parse().unwrap()
}

async fn make_admin(ctx: &TestContext) {
    crate::helpers::make_admin(&ctx.db, id(&ctx.alice_id)).await;
}

/// A role with `permissions` at `position`, given to `account_id`, whose
/// tokens get the admin scopes.
async fn give_role(ctx: &TestContext, account_id: &str, position: i32, permissions: i64) -> i64 {
    let role: i64 = sqlx::query_scalar(
        "INSERT INTO user_roles (name, position, permissions, highlighted, created_at, updated_at)
         VALUES ($1, $2, $3, true, now(), now()) RETURNING id",
    )
    .bind(format!("Role {position}"))
    .bind(position)
    .bind(permissions)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    sqlx::query("UPDATE users SET role_id = $1 WHERE account_id = $2")
        .bind(role)
        .bind(id(account_id))
        .execute(&ctx.db)
        .await
        .unwrap();
    crate::helpers::grant_admin_scopes(&ctx.db, id(account_id)).await;
    role
}

async fn json_ok(resp: reqwest::Response) -> Value {
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{text}");
    serde_json::from_str(&text).unwrap()
}

async fn error_of(resp: reqwest::Response, status: StatusCode) -> String {
    let got = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(got, status, "{text}");
    serde_json::from_str::<Value>(&text).unwrap()["error"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

async fn logs(ctx: &TestContext) -> Vec<(String, String)> {
    sqlx::query_as("SELECT action, target_type FROM admin_action_logs ORDER BY id")
        .fetch_all(&ctx.db)
        .await
        .unwrap()
}

async fn patch_settings(ctx: &TestContext, body: Value) -> reqwest::Response {
    ctx.api
        .patch_json("/api/v1/admin/settings", Some(&ctx.alice_token), &body)
        .await
}

const MANAGE_SETTINGS: i64 = 1 << 6;

/// The settings answer only to `manage_settings`, and show the configured
/// title and registrations until the settings say otherwise.
#[tokio::test]
async fn test_settings_need_manage_settings() {
    let ctx = TestContext::new("srv-settings-auth").await;
    crate::helpers::grant_admin_scopes(&ctx.db, id(&ctx.bob_id)).await;
    let refused = ctx
        .api
        .get("/api/v1/admin/settings", Some(&ctx.bob_token))
        .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    give_role(&ctx, &ctx.bob_id, 10, MANAGE_SETTINGS).await;
    let settings = json_ok(
        ctx.api
            .get("/api/v1/admin/settings", Some(&ctx.bob_token))
            .await,
    )
    .await;
    assert_eq!(settings["site_title"], "c2s test");
    assert_eq!(settings["registrations_mode"], "open");
    assert_eq!(settings["trends"], true);
    assert_eq!(settings["backups_retention_period"], 7);
    assert_eq!(settings["thumbnail"], Value::Null);
    assert_eq!(settings["overridden"], json!([]));
    // Saving settings is not logged, as `Admin::SettingsController` logs
    // nothing.
    json_ok(
        ctx.api
            .patch_json(
                "/api/v1/admin/settings",
                Some(&ctx.bob_token),
                &json!({"site_title": "Renamed"}),
            )
            .await,
    )
    .await;
    assert!(logs(&ctx).await.is_empty());
}

/// `Form::AdminSettings`' validations, with its messages, refuse the whole
/// save.
#[tokio::test]
async fn test_settings_validate_as_the_form_does() {
    let ctx = TestContext::new("srv-settings-validate").await;
    make_admin(&ctx).await;

    let cases = [
        (
            json!({"registrations_mode": "sometimes"}),
            "Validation failed: Registrations mode is not included in the list",
        ),
        (
            json!({"site_short_description": "x".repeat(201)}),
            "Validation failed: Site short description is too long (maximum is 200 characters)",
        ),
        (
            json!({"status_page_url": "status.example.com"}),
            "Validation failed: Status page url is invalid",
        ),
        (
            json!({"media_cache_retention_period": "soon"}),
            "Validation failed: Media cache retention period is not a number",
        ),
        (
            json!({"content_cache_retention_period": "1.5"}),
            "Validation failed: Content cache retention period must be an integer",
        ),
        (
            json!({"local_topic_feed_access": "disabled"}),
            "Validation failed: Local topic feed access is not included in the list",
        ),
        (
            json!({"site_contact_username": "nobody"}),
            "Validation failed: Site contact email can't be blank, \
             Site contact username could not find a local user with that username",
        ),
        (
            json!({"bootstrap_timeline_accounts": "bob,ghost"}),
            "Validation failed: Bootstrap timeline accounts could not find ghost",
        ),
    ];
    for (body, message) in cases {
        let error = error_of(
            patch_settings(&ctx, body.clone()).await,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
        assert_eq!(error, message, "for {body}");
    }
    // Nothing was saved by any of them.
    let saved: i64 = sqlx::query_scalar("SELECT count(*) FROM settings")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(saved, 0);
}

/// What the branding, about and registrations pages save is what the instance
/// API, the sign-up and the directory then serve.
#[tokio::test]
async fn test_settings_drive_the_instance() {
    let ctx = TestContext::new("srv-settings-drive").await;
    make_admin(&ctx).await;

    json_ok(
        patch_settings(
            &ctx,
            json!({
                "site_title": "Galaxy",
                "site_short_description": "A small server",
                "site_contact_email": "staff@example.com",
                "site_contact_username": "@bob",
                "status_page_url": "https://status.example.com",
                "registrations_mode": "none",
                "closed_registrations_message": "We are **closed**.",
                "site_extended_description": "# About\n\nHello.",
            }),
        )
        .await,
    )
    .await;

    let v2 = json_ok(ctx.api.get("/api/v2/instance", None).await).await;
    assert_eq!(v2["title"], "Galaxy");
    assert_eq!(v2["description"], "A small server");
    assert_eq!(v2["contact"]["email"], "staff@example.com");
    assert_eq!(v2["contact"]["account"]["username"], "bob");
    assert_eq!(
        v2["configuration"]["urls"]["status"],
        "https://status.example.com"
    );
    assert_eq!(v2["registrations"]["enabled"], false);
    assert_eq!(
        v2["registrations"]["message"],
        "<p>We are <strong>closed</strong>.</p>\n"
    );
    let v1 = json_ok(ctx.api.get("/api/v1/instance", None).await).await;
    assert_eq!(v1["title"], "Galaxy");
    assert_eq!(v1["short_description"], "A small server");
    assert_eq!(v1["registrations"], false);
    let about = json_ok(
        ctx.api
            .get("/api/v1/instance/extended_description", None)
            .await,
    )
    .await;
    assert_eq!(about["content"], "<h1>About</h1>\n<p>Hello.</p>\n");
    assert!(about["updated_at"].is_string());

    let signup = |username: &'static str, reason: Option<&'static str>| {
        let api = &ctx.api;
        async move {
            let mut body = json!({
                "username": username,
                "email": format!("{username}@example.com"),
                "password": "a-long-enough-password",
                "agreement": true,
            });
            if let Some(reason) = reason {
                body["reason"] = json!(reason);
            }
            api.post_json("/api/v1/accounts", None, &body).await
        }
    };
    assert_eq!(
        signup("carol", None).await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );

    // Approval with a reason required.
    json_ok(
        patch_settings(
            &ctx,
            json!({"registrations_mode": "approved", "require_invite_text": "1"}),
        )
        .await,
    )
    .await;
    let v2 = json_ok(ctx.api.get("/api/v2/instance", None).await).await;
    assert_eq!(v2["registrations"]["enabled"], true);
    assert_eq!(v2["registrations"]["approval_required"], true);
    assert_eq!(v2["registrations"]["reason_required"], true);
    assert_eq!(v2["registrations"]["message"], Value::Null);
    assert_eq!(
        error_of(
            signup("carol", None).await,
            StatusCode::UNPROCESSABLE_ENTITY
        )
        .await,
        "Validation failed: Reason can't be blank"
    );
    assert_eq!(
        signup("carol", Some("I like stars")).await.status(),
        StatusCode::OK
    );

    // The directory follows `profile_directory`.
    assert_eq!(
        ctx.api
            .get("/api/v1/directory", Some(&ctx.alice_token))
            .await
            .status(),
        StatusCode::OK
    );
    json_ok(patch_settings(&ctx, json!({"profile_directory": false})).await).await;
    assert_eq!(
        ctx.api
            .get("/api/v1/directory", Some(&ctx.alice_token))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    // NodeInfo reads the same settings.
    let nodeinfo = json_ok(ctx.api.get("/nodeinfo/2.1", None).await).await;
    assert_eq!(nodeinfo["metadata"]["nodeName"], "Galaxy");
    assert_eq!(nodeinfo["openRegistrations"], true);
}

/// A PNG of `width` by `height` pixels.
fn png(width: u32, height: u32) -> Vec<u8> {
    let image =
        image::RgbImage::from_fn(width, height, |x, _| image::Rgb([(x % 255) as u8, 80, 160]));
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// The thumbnail and app icon are uploaded with the settings, rendered in
/// Mastodon's styles, served by the instance API, and removed again.
#[tokio::test]
async fn test_site_uploads() {
    let ctx = TestContext::new("srv-site-uploads").await;
    make_admin(&ctx).await;

    let form = reqwest::multipart::Form::new()
        .text("thumbnail_description", "Stars over a hill")
        .part(
            "thumbnail",
            reqwest::multipart::Part::bytes(png(60, 30))
                .file_name("thumb.png")
                .mime_str("image/png")
                .unwrap(),
        )
        .part(
            "app_icon",
            reqwest::multipart::Part::bytes(png(64, 64))
                .file_name("icon.png")
                .mime_str("image/png")
                .unwrap(),
        );
    let settings = json_ok(
        ctx.api
            .http
            .patch(ctx.api.url("/api/v1/admin/settings"))
            .header("host", &ctx.api.host)
            .bearer_auth(&ctx.alice_token)
            .multipart(form)
            .send()
            .await
            .unwrap(),
    )
    .await;
    let thumbnail = &settings["thumbnail"];
    assert_eq!(thumbnail["meta"], json!({"width": 60, "height": 30}));
    assert!(thumbnail["url"]
        .as_str()
        .unwrap()
        .contains("/site_uploads/files/"));

    let v2 = json_ok(ctx.api.get("/api/v2/instance", None).await).await;
    let url = v2["thumbnail"]["url"].as_str().unwrap();
    assert!(url.contains("/@1x/") && url.ends_with(".png"), "{url}");
    assert!(v2["thumbnail"]["blurhash"].is_string());
    assert_eq!(v2["thumbnail"]["description"], "Stars over a hill");
    assert!(v2["thumbnail"]["versions"]["@2x"]
        .as_str()
        .unwrap()
        .contains("/@2x/"));
    let icons = v2["icon"].as_array().unwrap();
    assert_eq!(icons.len(), 9);
    assert_eq!(icons[0]["size"], "36x36");
    assert!(icons[0]["src"].as_str().unwrap().contains("/36/"));

    // Something that is not an image is refused.
    let form = reqwest::multipart::Form::new().part(
        "favicon",
        reqwest::multipart::Part::bytes(b"not an image".to_vec())
            .file_name("favicon.txt")
            .mime_str("text/plain")
            .unwrap(),
    );
    let refused = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v1/admin/settings"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        error_of(refused, StatusCode::UNPROCESSABLE_ENTITY).await,
        "Validation failed: Favicon is invalid"
    );

    let upload_id = thumbnail["id"].as_str().unwrap();
    json_ok(
        ctx.api
            .delete(
                &format!("/api/v1/admin/site_uploads/{upload_id}"),
                &ctx.alice_token,
            )
            .await,
    )
    .await;
    let v2 = json_ok(ctx.api.get("/api/v2/instance", None).await).await;
    assert!(v2["thumbnail"]["url"]
        .as_str()
        .unwrap()
        .ends_with("/instance-thumbnail.png"));
}

/// The content retention periods drive the vacuum: remote posts and cached
/// remote media past them are forgotten, local ones never are.
#[tokio::test]
async fn test_content_retention() {
    let ctx = TestContext::new("srv-retention").await;
    let remote: i64 = sqlx::query_scalar(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, created_at, updated_at)
           VALUES ($1, 'far', 'remote.invalid', 'far', '', 'https://remote.invalid/@far',
                   'https://remote.invalid/users/far', now(), now())
           RETURNING id"#,
    )
    .bind(eunha::snowflake::next_id())
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let old_id = |days: i64| {
        (chrono::Utc::now() - chrono::Duration::days(days)).timestamp_millis() << 16 | 7
    };
    let insert = |status_id: i64, account_id: i64| {
        let db = ctx.db.clone();
        async move {
            sqlx::query(
                r#"INSERT INTO statuses (id, account_id, text, visibility, uri, local, created_at, updated_at)
                   VALUES ($1, $2, 'post', 0, 'https://remote.invalid/s/' || $1, false, now(), now())"#,
            )
            .bind(status_id)
            .bind(account_id)
            .execute(&db)
            .await
            .unwrap();
        }
    };
    let (old_remote, new_remote, old_local) = (old_id(40), old_id(2), old_id(41));
    insert(old_remote, remote).await;
    insert(new_remote, remote).await;
    insert(old_local, id(&ctx.alice_id)).await;
    let media: i64 = sqlx::query_scalar(
        r#"INSERT INTO media_attachments (id, account_id, status_id, remote_url, file_file_name,
             file_content_type, type, created_at, updated_at)
           VALUES ($1, $2, $3, 'https://remote.invalid/m.png', 'm.png', 'image/png', 0,
                   now() - interval '20 days', now() - interval '20 days')
           RETURNING id"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(remote)
    .bind(new_remote)
    .fetch_one(&ctx.db)
    .await
    .unwrap();

    // Without periods, nothing goes.
    eunha::vacuum::perform(&ctx.state).await;
    let count = |ids: Vec<i64>| {
        let db = ctx.db.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE id = ANY($1)")
                .bind(ids)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    assert_eq!(count(vec![old_remote, new_remote, old_local]).await, 3);

    crate::helpers::set_setting(&ctx.db, "content_cache_retention_period", "30").await;
    crate::helpers::set_setting(&ctx.db, "media_cache_retention_period", "14").await;
    eunha::vacuum::perform(&ctx.state).await;
    assert_eq!(count(vec![old_remote]).await, 0);
    assert_eq!(count(vec![new_remote, old_local]).await, 2);
    let (file, remote_url): (Option<String>, String) =
        sqlx::query_as("SELECT file_file_name, remote_url FROM media_attachments WHERE id = $1")
            .bind(media)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(file, None);
    assert_eq!(remote_url, "https://remote.invalid/m.png");
}
