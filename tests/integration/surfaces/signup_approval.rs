//! Whether an invite skips the approval queue.
//!
//! Mastodon's `User#set_approved` grants approval on an approval-required
//! instance only through `valid_bypassing_invitation?`, and `Invite#bypass_approval?`
//! is `user&.role&.can?(:invite_bypass_approval)` — a question about who wrote
//! the invite, not about whether one was used.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

/// Mint an invite as bob and sign somebody up through it, returning whether the
/// new account came out approved.
async fn signup_through_bobs_invite(ctx: &TestContext, username: &str) -> bool {
    let invite: Value = ctx
        .api
        .post_json("/api/v1/invites", Some(&ctx.bob_token), &json!({}))
        .await
        .json()
        .await
        .unwrap();
    let code = invite["code"].as_str().expect("bob may invite").to_string();

    let signup = ctx
        .sign_up(&json!({
                "username": username,
                "email": format!("{username}@example.com"),
                "password": "a-long-enough-password",
                "agreement": true,
                "invite_code": code,
        }))
        .await;
    assert_eq!(signup.status(), StatusCode::OK);

    // Approval is decided when the user is saved (`User#set_approved`);
    // confirming it is what an approved user is welcomed on.
    let token = ctx.confirmation_token(username).await;
    let confirmed = ctx
        .api
        .get(&format!("/auth/confirm?token={token}"), None)
        .await;
    assert_eq!(confirmed.status(), StatusCode::SEE_OTHER);

    sqlx::query_scalar::<_, bool>(
        "SELECT u.approved FROM users u JOIN accounts a ON a.id = u.account_id
         WHERE a.username = $1",
    )
    .bind(username)
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

/// An ordinary member's invite gets its holder reviewed like anyone else. The
/// everyone role carries `Flags::DEFAULT` — `invite_users` and nothing more —
/// so bob may invite without being able to waive the instance's own review.
#[tokio::test]
async fn test_a_plain_invite_does_not_skip_approval() {
    let ctx = TestContext::with_approval_required("signup-approval-invite").await;

    assert!(
        !signup_through_bobs_invite(&ctx, "carol").await,
        "an invite from a member without `invite_bypass_approval` should still be reviewed",
    );
}

/// With `invite_bypass_approval` on the everyone role — one of the two flags
/// `UserRole::Flags::SAFE` lets that role hold — the same invite approves its
/// holder outright. This is how an instance asks for the behaviour eunha used
/// to give every invite.
#[tokio::test]
async fn test_an_invite_from_someone_who_may_bypass_skips_approval() {
    let ctx = TestContext::with_approval_required("signup-approval-bypass").await;
    sqlx::query("UPDATE user_roles SET permissions = permissions | (1 << 21) WHERE id = -99")
        .execute(&ctx.db)
        .await
        .unwrap();

    assert!(
        signup_through_bobs_invite(&ctx, "carol").await,
        "an invite from a member who may bypass approval should approve its holder",
    );
}

/// Nothing here changes what happens without an invite: an approval-required
/// instance reviews an uninvited signup, as it always did.
#[tokio::test]
async fn test_an_uninvited_signup_is_still_reviewed() {
    let ctx = TestContext::with_approval_required("signup-approval-none").await;

    let signup = ctx
        .sign_up(&json!({
                "username": "carol",
                "email": "carol@example.com",
                "password": "a-long-enough-password",
                "agreement": true,
        }))
        .await;
    assert_eq!(signup.status(), StatusCode::OK);

    let token = ctx.confirmation_token("carol").await;
    ctx.api
        .get(&format!("/auth/confirm?token={token}"), None)
        .await;

    let approved: bool = sqlx::query_scalar(
        "SELECT u.approved FROM users u JOIN accounts a ON a.id = u.account_id
         WHERE a.username = 'carol'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(!approved, "an uninvited signup should await approval");
}
