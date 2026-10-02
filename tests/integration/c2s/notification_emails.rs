//! Notification emails: `NotifyService#send_email!`, `NotificationMailer`,
//! and the one-click unsubscribe links in what it sends.

use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{seed_user, user_id_for, TestContext};

const ALICE: &str = "alice@test.invalid";

async fn set_preferences(ctx: &TestContext, body: Value) {
    let resp = ctx
        .api
        .patch_json("/api/eunha/v1/preferences", Some(&ctx.alice_token), &body)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Give a background task the time it would take to mail, then say whether
/// nothing reached `address` with `subject`.
async fn nothing_mailed(ctx: &TestContext, subject: &str) -> bool {
    tokio::time::sleep(Duration::from_millis(500)).await;
    !ctx.sent_to(ALICE)
        .iter()
        .any(|m| m.subject.contains(subject))
}

/// The address the unsubscribe link of `mail` points at, made relative.
fn unsubscribe_path(ctx: &TestContext, mail: &eunha::email::SentMail) -> String {
    let header = mail.header("List-Unsubscribe").expect("List-Unsubscribe");
    let url = header.trim_start_matches('<').trim_end_matches('>');
    url.strip_prefix(&format!("https://{}", ctx.domain))
        .expect("a link to this instance")
        .to_owned()
}

#[tokio::test]
async fn test_follow_mail_and_its_headers() {
    let ctx = TestContext::new("notifmail-follow").await;
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    let mail = ctx
        .mail_to(ALICE, "bob is now following you")
        .await
        .expect("a follow mail");
    assert_eq!(mail.to, format!("alice <{ALICE}>"));
    assert!(mail.html.contains("New follower"));
    assert!(mail.html.contains(&format!("https://{}/@bob", ctx.domain)));
    assert_eq!(
        mail.header("List-ID"),
        Some(format!("<follow.alice.{}>", ctx.domain).as_str())
    );
    assert_eq!(
        mail.header("List-Unsubscribe-Post"),
        Some("List-Unsubscribe=One-Click")
    );
    assert_eq!(mail.header("Auto-Submitted"), Some("auto-generated"));
    assert!(unsubscribe_path(&ctx, &mail).ends_with("&type=follow"));
}

#[tokio::test]
async fn test_favourites_are_mailed_only_when_asked_for() {
    let ctx = TestContext::new("notifmail-favourite").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "a post worth a star", "public")
        .await;
    let id = status["id"].as_str().unwrap();
    let favourite = |token: String| {
        let path = format!("/api/v1/statuses/{id}/favourite");
        let api = &ctx.api;
        async move { api.post_json(&path, Some(&token), &json!({})).await }
    };

    // `notification_emails.favourite` is off unless chosen.
    favourite(ctx.bob_token.clone()).await;
    assert!(nothing_mailed(&ctx, "favorited your post").await);

    set_preferences(
        &ctx,
        json!({ "notification_emails": { "favourite": true } }),
    )
    .await;
    let (_, carol_token) = seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    favourite(carol_token).await;
    let mail = ctx
        .mail_to(ALICE, "carol favorited your post")
        .await
        .expect("a favourite mail");
    assert!(mail.html.contains("a post worth a star"));
    assert!(mail
        .html
        .contains(&format!("https://{}/@alice/{id}", ctx.domain)));
    // `thread_by_conversation!`
    assert!(mail.header("In-Reply-To").is_some_and(
        |h| h.starts_with("<conversation-") && h.ends_with(&format!("@{}>", ctx.domain))
    ));
}

#[tokio::test]
async fn test_mention_mail_writes_the_time_in_the_users_zone() {
    let ctx = TestContext::new("notifmail-mention").await;
    set_preferences(&ctx, json!({ "time_zone": "Asia/Seoul" })).await;
    ctx.api
        .post_status(&ctx.bob_token, "@alice are you there?", "public")
        .await;
    let mail = ctx
        .mail_to(ALICE, "You were mentioned by bob")
        .await
        .expect("a mention mail");
    assert!(mail.html.contains("are you there?"));
    assert!(mail.html.contains(" KST</a>"), "{}", mail.html);
}

#[tokio::test]
async fn test_no_mail_while_online_unless_always() {
    let ctx = TestContext::new("notifmail-online").await;
    let alice_user = user_id_for(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    // A web push subscription counts as being online.
    sqlx::query(
        "INSERT INTO web_push_subscriptions
           (endpoint, key_p256dh, key_auth, data, access_token_id, user_id, created_at, updated_at)
         SELECT 'https://push.invalid/1', 'p256dh', 'auth', '{}', t.id, $1, now(), now()
         FROM oauth_access_tokens t WHERE t.resource_owner_id = $1 LIMIT 1",
    )
    .bind(alice_user)
    .execute(&ctx.db)
    .await
    .unwrap();
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    assert!(nothing_mailed(&ctx, "is now following you").await);

    set_preferences(&ctx, json!({ "always_send_emails": true })).await;
    let (_, carol_token) = seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    ctx.api.follow(&carol_token, &ctx.alice_id).await;
    assert!(ctx
        .mail_to(ALICE, "carol is now following you")
        .await
        .is_some());
}

#[tokio::test]
async fn test_no_mail_while_streaming_the_users_own_stream() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let ctx = TestContext::new("notifmail-streaming").await;
    let url = format!(
        "{}/api/v1/streaming?stream=user:notification&access_token={}",
        ctx.api.base_url.replace("http://", "ws://"),
        ctx.alice_token
    );
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("host", ctx.domain.parse().unwrap());
    let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    assert!(nothing_mailed(&ctx, "is now following you").await);

    // Once the connection is gone, so is the reason not to mail.
    drop(socket);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (_, carol_token) = seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    ctx.api.follow(&carol_token, &ctx.alice_id).await;
    assert!(ctx
        .mail_to(ALICE, "carol is now following you")
        .await
        .is_some());
}

#[tokio::test]
async fn test_no_mail_for_a_disabled_user() {
    let ctx = TestContext::new("notifmail-disabled").await;
    sqlx::query("UPDATE users SET disabled = true WHERE account_id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    assert!(nothing_mailed(&ctx, "is now following you").await);
}

#[tokio::test]
async fn test_unsubscribe_link_turns_the_type_off() {
    let ctx = TestContext::new("notifmail-unsubscribe").await;
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    let mail = ctx
        .mail_to(ALICE, "is now following you")
        .await
        .expect("a follow mail");
    let path = unsubscribe_path(&ctx, &mail);

    // `show` asks first, and changes nothing.
    let page = ctx.api.get(&path, None).await;
    assert_eq!(page.status(), StatusCode::OK);
    let html = page.text().await.unwrap();
    assert!(html.contains("Unsubscribe from follow notification emails?"));
    assert!(html.contains("name=\"type\" value=\"follow\""));

    // Another type than the link's is still a known one; a made-up one is not.
    let token_query = path.split_once('?').unwrap().1.split('&').next().unwrap();
    let refused = ctx
        .api
        .get(&format!("/unsubscribe?{token_query}&type=status"), None)
        .await;
    assert_eq!(refused.status(), StatusCode::NOT_FOUND);
    let forged = ctx
        .api
        .get("/unsubscribe?token=VXNlci8x--00&type=follow", None)
        .await;
    assert_eq!(forged.status(), StatusCode::NOT_FOUND);

    // What a mail client's one-click unsubscribe posts.
    let done = ctx
        .api
        .post_form(&path, None, &[("List-Unsubscribe", "One-Click")])
        .await;
    assert_eq!(done.status(), StatusCode::OK);
    assert!(done.text().await.unwrap().contains("You are unsubscribed"));
    let prefs: Value = ctx
        .api
        .get("/api/eunha/v1/preferences", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(prefs["notification_emails"]["follow"], false);

    let (_, carol_token) = seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    ctx.api.follow(&carol_token, &ctx.alice_id).await;
    assert!(nothing_mailed(&ctx, "carol is now following you").await);
}
