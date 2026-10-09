//! `Scheduler::AutoCloseRegistrationsScheduler`: open registrations switch
//! to approval once no moderator has been active for a week.

use crate::helpers::{make_admin, set_setting, TestContext};

async fn mode(ctx: &TestContext) -> &'static str {
    eunha::settings::registrations_mode(&ctx.state)
        .await
        .as_str()
}

/// Make alice the only moderator, last seen `days` ago.
async fn moderator_last_seen(ctx: &TestContext, days: i32) {
    make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    sqlx::query(
        "UPDATE users SET current_sign_in_at = now() - make_interval(days => $2)
         WHERE account_id = $1",
    )
    .bind(ctx.alice_id.parse::<i64>().unwrap())
    .bind(days)
    .execute(&ctx.db)
    .await
    .unwrap();
}

#[tokio::test]
async fn test_idle_moderators_close_open_registrations() {
    let ctx = TestContext::new("auto-close").await;
    moderator_last_seen(&ctx, 9).await;
    assert_eq!(mode(&ctx).await, "open");

    assert!(eunha::auto_close_registrations::check(&ctx.state)
        .await
        .unwrap());
    assert_eq!(mode(&ctx).await, "approved");
    let mail = ctx
        .mail_to("alice@test.invalid", "have been automatically switched")
        .await
        .expect("the administrator is told");
    assert!(mail.text.contains("lack of recent moderator activity"));

    // Already approval: nothing more to do.
    assert!(!eunha::auto_close_registrations::check(&ctx.state)
        .await
        .unwrap());
}

#[tokio::test]
async fn test_a_moderator_seen_this_week_keeps_them_open() {
    let ctx = TestContext::new("auto-close-active").await;
    moderator_last_seen(&ctx, 6).await;
    assert!(!eunha::auto_close_registrations::check(&ctx.state)
        .await
        .unwrap());
    assert_eq!(mode(&ctx).await, "open");
}

/// `UserTrackingConcern`: an authenticated API request signs a user in again
/// once their sign-in time is a day old, which is what keeps a moderator
/// working through the API active.
#[tokio::test]
async fn test_a_moderator_using_the_api_counts_as_active() {
    let ctx = TestContext::new("auto-close-api").await;
    moderator_last_seen(&ctx, 30).await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let times = || {
        sqlx::query_as::<
            _,
            (
                Option<chrono::NaiveDateTime>,
                Option<chrono::NaiveDateTime>,
                i32,
            ),
        >(
            "SELECT current_sign_in_at, last_sign_in_at, sign_in_count FROM users \
             WHERE account_id = $1",
        )
        .bind(alice)
        .fetch_one(&ctx.db)
    };
    let (before, _, count) = times().await.unwrap();

    ctx.api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await;
    let (current, last, after) = times().await.unwrap();
    assert!(current > before, "signed in again");
    assert_eq!(last, before, "the last sign-in is the one before");
    assert_eq!(after, count, "not a new sign-in");

    // Within the day, it does not move again.
    ctx.api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(times().await.unwrap().0, current);

    assert!(!eunha::auto_close_registrations::check(&ctx.state)
        .await
        .unwrap());
    assert_eq!(mode(&ctx).await, "open");
}

/// Only `current_sign_in_at` counts: a token used this week, by a moderator
/// whose sign-in time says otherwise, does not.
#[tokio::test]
async fn test_token_use_alone_does_not_count() {
    let ctx = TestContext::new("auto-close-token").await;
    moderator_last_seen(&ctx, 30).await;
    sqlx::query(
        "UPDATE oauth_access_tokens SET last_used_at = now() \
         WHERE resource_owner_id = (SELECT id FROM users WHERE account_id = $1)",
    )
    .bind(ctx.alice_id.parse::<i64>().unwrap())
    .execute(&ctx.db)
    .await
    .unwrap();
    assert!(eunha::auto_close_registrations::check(&ctx.state)
        .await
        .unwrap());
    assert_eq!(mode(&ctx).await, "approved");
}

#[tokio::test]
async fn test_only_open_registrations_are_switched() {
    let ctx = TestContext::new("auto-close-closed").await;
    moderator_last_seen(&ctx, 30).await;
    set_setting(&ctx.db, "registrations_mode", "none").await;
    assert!(!eunha::auto_close_registrations::check(&ctx.state)
        .await
        .unwrap());
    assert_eq!(mode(&ctx).await, "none");
}

#[tokio::test]
async fn test_the_configuration_can_turn_it_off() {
    let ctx = TestContext::with_instance_config("auto-close-off", |instance| {
        instance.disable_automatic_switching_to_approved_registrations = true
    })
    .await;
    moderator_last_seen(&ctx, 30).await;
    assert!(!eunha::auto_close_registrations::check(&ctx.state)
        .await
        .unwrap());
    assert_eq!(mode(&ctx).await, "open");
}
