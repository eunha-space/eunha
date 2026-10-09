//! Who is kept out of a timeline.
//!
//! Mastodon builds public timelines with `not_excluded_by_account` and
//! `not_domain_blocked_by_account`, and `excluded_from_timeline_account_ids` is
//! the union of three relationships: accounts I block, accounts that block *me*,
//! and accounts I mute. The middle one is easy to miss and matters most — an
//! account that blocked someone should not go on appearing in their timeline.

use crate::helpers::TestContext;

async fn public_timeline_authors(ctx: &TestContext, token: &str) -> Vec<String> {
    let body: serde_json::Value = ctx
        .api
        .get("/api/v1/timelines/public?local=true", Some(token))
        .await
        .json()
        .await
        .unwrap();
    body.as_array()
        .map(|statuses| {
            statuses
                .iter()
                .filter_map(|s| s["account"]["username"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// An account that has blocked you does not appear in your timeline.
#[tokio::test]
async fn test_an_account_that_blocked_me_is_kept_out() {
    let ctx = TestContext::new("timeline-blocked-by").await;

    ctx.api
        .post_status(&ctx.bob_token, "bob says something", "public")
        .await;
    assert!(
        public_timeline_authors(&ctx, &ctx.alice_token)
            .await
            .contains(&"bob".to_string()),
        "bob should be visible before any block"
    );

    // Bob blocks Alice.
    sqlx::query!(
        r#"INSERT INTO blocks (id, account_id, target_account_id, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())"#,
        eunha::snowflake::next_id(),
        ctx.bob_id.parse::<i64>().unwrap(),
        ctx.alice_id.parse::<i64>().unwrap(),
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    assert!(
        !public_timeline_authors(&ctx, &ctx.alice_token)
            .await
            .contains(&"bob".to_string()),
        "an account that blocked me must not appear in my timeline"
    );
}

/// An account you mute does not appear either.
#[tokio::test]
async fn test_a_muted_account_is_kept_out() {
    let ctx = TestContext::new("timeline-muted").await;

    ctx.api
        .post_status(&ctx.bob_token, "bob says something", "public")
        .await;

    sqlx::query!(
        r#"INSERT INTO mutes (id, account_id, target_account_id, hide_notifications, created_at, updated_at)
           VALUES ($1, $2, $3, false, now(), now())"#,
        eunha::snowflake::next_id(),
        ctx.alice_id.parse::<i64>().unwrap(),
        ctx.bob_id.parse::<i64>().unwrap(),
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    assert!(
        !public_timeline_authors(&ctx, &ctx.alice_token)
            .await
            .contains(&"bob".to_string()),
        "an account I mute must not appear in my timeline"
    );
}

/// An account you block does not appear.
#[tokio::test]
async fn test_a_blocked_account_is_kept_out() {
    let ctx = TestContext::new("timeline-blocking").await;

    ctx.api
        .post_status(&ctx.bob_token, "bob says something", "public")
        .await;

    sqlx::query!(
        r#"INSERT INTO blocks (id, account_id, target_account_id, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())"#,
        eunha::snowflake::next_id(),
        ctx.alice_id.parse::<i64>().unwrap(),
        ctx.bob_id.parse::<i64>().unwrap(),
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    assert!(
        !public_timeline_authors(&ctx, &ctx.alice_token)
            .await
            .contains(&"bob".to_string()),
        "an account I block must not appear in my timeline"
    );
}

/// An account on a domain you block does not appear.
///
/// `not_domain_blocked_by_account` is a separate scope from the account-level
/// exclusions, so it is separately possible to get wrong.
#[tokio::test]
async fn test_an_account_on_a_blocked_domain_is_kept_out() {
    let ctx = TestContext::new("timeline-domain-blocked").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();

    let domain = "timeline-blocked.invalid";
    let actor_uri = format!("https://{domain}/users/stranger");
    let stranger_id = eunha::snowflake::next_id();
    sqlx::query!(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, protocol, created_at, updated_at)
           VALUES ($1, 'stranger', $2, 'stranger', '', $3::text, $3::text, 'remote-key',
                   $3::text||'/inbox', $3::text||'/outbox', 1, now(), now())"#,
        stranger_id,
        domain,
        actor_uri,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    sqlx::query!(
        r#"INSERT INTO statuses (id, account_id, text, visibility, uri, url, local, created_at, updated_at)
           VALUES ($1, $2, 'a post from elsewhere', 0, $3, $3, false, now(), now())"#,
        eunha::snowflake::next_id(),
        stranger_id,
        format!("https://{domain}/notes/1"),
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    async fn federated_authors(ctx: &TestContext) -> Vec<String> {
        let body: serde_json::Value = ctx
            .api
            .get("/api/v1/timelines/public", Some(&ctx.alice_token))
            .await
            .json()
            .await
            .unwrap();
        body.as_array()
            .map(|v| {
                v.iter()
                    .filter_map(|s| s["account"]["acct"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    let acct = format!("stranger@{domain}");
    assert!(
        federated_authors(&ctx).await.contains(&acct),
        "the remote post should be visible before the domain is blocked"
    );

    sqlx::query!(
        r#"INSERT INTO account_domain_blocks (account_id, domain, created_at, updated_at)
           VALUES ($1, $2, now(), now())"#,
        alice_id,
        domain,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    assert!(
        !federated_authors(&ctx).await.contains(&acct),
        "an account on a domain I block must not appear in my timeline"
    );
}
