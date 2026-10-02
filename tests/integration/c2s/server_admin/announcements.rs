//! `Admin::AnnouncementsController` and what publishing does.

use super::*;

const MANAGE_ANNOUNCEMENTS: i64 = 1 << 13;

/// Announcements: `manage_announcements`, validated as `Announcement` is,
/// published at once unless scheduled, published by the schedule when due,
/// streamed, linked to the posts they name, and logged.
#[tokio::test]
async fn test_announcements() {
    let ctx = TestContext::new("srv-announcements").await;
    give_role(&ctx, &ctx.bob_id, 10, MANAGE_ANNOUNCEMENTS).await;
    let post = |body: Value| {
        let api = &ctx.api;
        let token = ctx.bob_token.clone();
        async move {
            api.post_json("/api/v1/admin/announcements", Some(&token), &body)
                .await
        }
    };
    assert_eq!(
        error_of(
            post(json!({"text": "", "ends_at": "2030-01-02T00:00:00Z"})).await,
            StatusCode::UNPROCESSABLE_ENTITY
        )
        .await,
        "Validation failed: Text can't be blank, Starts at can't be blank"
    );

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "see this", "public")
        .await;
    let status_url = status["url"].as_str().unwrap().to_owned();
    sqlx::query("UPDATE users SET current_sign_in_at = now() WHERE account_id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let mut events = ctx
        .state
        .streaming
        .subscribe(&format!("timeline:{}", ctx.alice_id));
    let now = json_ok(post(json!({"text": format!("Hello all, {status_url}")})).await).await;
    assert_eq!(now["published"], true);
    assert!(now["published_at"].is_string());
    // `PublishScheduledAnnouncementWorker` streams it, rendered for nobody.
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let message = events.recv().await.unwrap();
            if message["event"] == "announcement" {
                break message;
            }
        }
    })
    .await
    .expect("an announcement event");
    let streamed = &event["payload"];
    assert_eq!(streamed["id"], now["id"]);
    assert!(streamed.get("read").is_none());

    let public = json_ok(
        ctx.api
            .get("/api/v1/announcements", Some(&ctx.alice_token))
            .await,
    )
    .await;
    assert_eq!(public.as_array().unwrap().len(), 1);
    assert_eq!(public[0]["read"], false);
    assert_eq!(public[0]["statuses"][0]["id"], status["id"]);

    let later = json_ok(
        post(json!({"text": "Maintenance tonight", "scheduled_at": "2099-01-01T00:00:00Z"})).await,
    )
    .await;
    assert_eq!(later["published"], false);
    let unpublished = json_ok(
        ctx.api
            .get(
                "/api/v1/admin/announcements?unpublished=1",
                Some(&ctx.bob_token),
            )
            .await,
    )
    .await;
    assert_eq!(unpublished.as_array().unwrap().len(), 1);
    assert_eq!(unpublished[0]["id"], later["id"]);

    // Once due, the schedule publishes it.
    let later_id = id(later["id"].as_str().unwrap());
    sqlx::query(
        "UPDATE announcements SET scheduled_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(later_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    eunha::announcements::run_schedule_once(&ctx.state)
        .await
        .unwrap();
    let published: bool = sqlx::query_scalar("SELECT published FROM announcements WHERE id = $1")
        .bind(later_id)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert!(published);

    // Unpublishing takes it off the public list; deleting removes it.
    json_ok(
        ctx.api
            .post_json(
                &format!("/api/v1/admin/announcements/{later_id}/unpublish"),
                Some(&ctx.bob_token),
                &json!({}),
            )
            .await,
    )
    .await;
    let public = json_ok(ctx.api.get("/api/v1/announcements", None).await).await;
    assert_eq!(public.as_array().unwrap().len(), 1);
    json_ok(
        ctx.api
            .delete(
                &format!("/api/v1/admin/announcements/{later_id}"),
                &ctx.bob_token,
            )
            .await,
    )
    .await;

    // Mailing it needs `manage_settings` too, and happens once.
    let now_id = now["id"].as_str().unwrap();
    assert_eq!(
        ctx.api
            .get(
                &format!("/api/v1/admin/announcements/{now_id}/preview"),
                Some(&ctx.bob_token),
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    make_admin(&ctx).await;
    let preview = json_ok(
        ctx.api
            .get(
                &format!("/api/v1/admin/announcements/{now_id}/preview"),
                Some(&ctx.alice_token),
            )
            .await,
    )
    .await;
    assert_eq!(preview["user_count"], 2);
    let sent = json_ok(
        ctx.api
            .post_json(
                &format!("/api/v1/admin/announcements/{now_id}/distribution"),
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await,
    )
    .await;
    assert!(sent["notification_sent_at"].is_string());
    assert_eq!(
        ctx.api
            .post_json(
                &format!("/api/v1/admin/announcements/{now_id}/distribution"),
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );

    let actions: Vec<String> = logs(&ctx)
        .await
        .into_iter()
        .filter(|(_, kind)| kind == "Announcement")
        .map(|(action, _)| action)
        .collect();
    assert_eq!(actions, ["create", "create", "update", "destroy"]);
}
