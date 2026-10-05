//! Handing invites to other members — eunha's own action, which Mastodon has no
//! equivalent of.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{make_admin, TestContext};

/// Take `invite_users` off the everyone role, leaving members unable to mint
/// invites of their own — the state an instance that hands them out is in.
async fn close_invites(ctx: &TestContext) {
    sqlx::query("UPDATE user_roles SET permissions = 0 WHERE id = -99")
        .execute(&ctx.db)
        .await
        .unwrap();
}

async fn invites_of(ctx: &TestContext, token: &str) -> Vec<Value> {
    ctx.api
        .get("/api/v1/invites", Some(token))
        .await
        .json()
        .await
        .unwrap()
}

/// The codes an admin hands out belong to the member they were minted for: they
/// appear on that member's own invite page, single-use by default, and the
/// member still cannot make more.
#[tokio::test]
async fn test_grant_mints_codes_into_a_members_account() {
    let ctx = TestContext::new("invite-grant-one").await;
    close_invites(&ctx).await;
    make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;

    let result: Value = ctx
        .api
        .post_json(
            "/api/eunha/v1/invite_grants",
            Some(&ctx.alice_token),
            &json!({"account_id": ctx.bob_id, "count": 2}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(result["granted"].as_i64(), Some(2));
    assert_eq!(result["accounts"].as_i64(), Some(1));

    let bobs = invites_of(&ctx, &ctx.bob_token).await;
    assert_eq!(bobs.len(), 2, "bob should hold the two codes");
    for invite in &bobs {
        assert_eq!(invite["max_uses"].as_i64(), Some(1));
        assert_eq!(invite["uses"].as_i64(), Some(0));
        assert!(invite["expires_at"].is_null());
        assert!(invite["url"].as_str().unwrap().contains("invite="));
    }

    // Reading what you were given is not permission to make your own.
    assert_eq!(
        ctx.api
            .post_json("/api/v1/invites", Some(&ctx.bob_token), &json!({}))
            .await
            .status(),
        StatusCode::FORBIDDEN,
    );

    // Nothing landed in the admin's own list — the codes are bob's.
    assert!(invites_of(&ctx, &ctx.alice_token).await.is_empty());
}

/// Handing out to the whole userbase reaches every member, the admin included.
#[tokio::test]
async fn test_grant_to_everyone() {
    let ctx = TestContext::new("invite-grant-all").await;
    close_invites(&ctx).await;
    make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;

    let result: Value = ctx
        .api
        .post_json(
            "/api/eunha/v1/invite_grants",
            Some(&ctx.alice_token),
            &json!({"count": 1, "max_uses": 5, "comment": "one each"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(result["accounts"].as_i64(), Some(2));
    assert_eq!(result["granted"].as_i64(), Some(2));

    for token in [&ctx.alice_token, &ctx.bob_token] {
        let invites = invites_of(&ctx, token).await;
        assert_eq!(invites.len(), 1);
        assert_eq!(invites[0]["max_uses"].as_i64(), Some(5));
        assert_eq!(invites[0]["comment"].as_str(), Some("one each"));
    }
}

/// Someone signing up through a granted code joins the tree under the member it
/// was minted for, not under the admin who minted it. This is the whole point
/// of putting the codes in their name.
#[tokio::test]
async fn test_granted_code_nests_the_signup_under_its_holder() {
    let ctx = TestContext::new("invite-grant-tree").await;
    close_invites(&ctx).await;
    make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;

    ctx.api
        .post_json(
            "/api/eunha/v1/invite_grants",
            Some(&ctx.alice_token),
            &json!({"account_id": ctx.bob_id, "count": 1}),
        )
        .await;
    let code = invites_of(&ctx, &ctx.bob_token).await[0]["code"]
        .as_str()
        .unwrap()
        .to_string();

    let signup = ctx
        .sign_up(&json!({
            "username": "carol",
            "email": "carol@example.com",
            "password": "a-long-enough-password",
            "agreement": true,
            "invite_code": code,
        }))
        .await;
    assert_eq!(signup.status(), StatusCode::OK);

    let token = ctx.confirmation_token("carol").await;
    let confirmed = ctx
        .api
        .get(&format!("/auth/confirm?token={token}"), None)
        .await;
    assert_eq!(confirmed.status(), StatusCode::SEE_OTHER);

    let tree: Value = ctx
        .api
        .get("/api/eunha/v1/invite_tree", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let roots = tree["roots"].as_array().unwrap();
    let bob = roots
        .iter()
        .find(|n| n["id"].as_str() == Some(ctx.bob_id.as_str()))
        .expect("bob should be a root");
    assert!(
        bob["children"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["username"].as_str() == Some("carol")),
        "carol should hang under bob, whose code she used",
    );
}

/// Only `manage_invites` (which `administrator` includes) may hand invites out;
/// a member cannot mint codes into anyone's account, their own included.
#[tokio::test]
async fn test_grant_requires_manage_invites() {
    let ctx = TestContext::new("invite-grant-perm").await;

    assert_eq!(
        ctx.api
            .post_json(
                "/api/eunha/v1/invite_grants",
                Some(&ctx.bob_token),
                &json!({"account_id": ctx.bob_id, "count": 1}),
            )
            .await
            .status(),
        StatusCode::FORBIDDEN,
    );
    assert_eq!(
        ctx.api
            .post_json("/api/eunha/v1/invite_grants", None, &json!({"count": 1}),)
            .await
            .status(),
        StatusCode::UNAUTHORIZED,
    );
}

/// The counts are bounded: a mistyped grant across a whole userbase should not
/// write ten thousand rows, and an unknown account is not silently everyone.
#[tokio::test]
async fn test_grant_bounds_and_unknown_account() {
    let ctx = TestContext::new("invite-grant-bounds").await;
    make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;

    for body in [
        json!({"count": 0}),
        json!({"count": 26}),
        json!({"count": 1, "max_uses": 0}),
        json!({"count": 1, "max_uses": 101}),
        json!({"count": 1, "comment": "x".repeat(421)}),
    ] {
        assert_eq!(
            ctx.api
                .post_json("/api/eunha/v1/invite_grants", Some(&ctx.alice_token), &body)
                .await
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body} should be rejected",
        );
    }

    assert_eq!(
        ctx.api
            .post_json(
                "/api/eunha/v1/invite_grants",
                Some(&ctx.alice_token),
                &json!({"account_id": "1", "count": 1}),
            )
            .await
            .status(),
        StatusCode::NOT_FOUND,
    );
}

#[tokio::test]
async fn test_public_invite_resolution_tracks_owner_permissions_and_validity() {
    let ctx = TestContext::new("invite-resolution").await;
    make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    for (token, bypass) in [(&ctx.alice_token, true), (&ctx.bob_token, false)] {
        let invite: Value = ctx
            .api
            .post_json(
                "/api/v1/invites",
                Some(token),
                &json!({"max_uses": 1, "autofollow": true}),
            )
            .await
            .json()
            .await
            .unwrap();
        let path = format!(
            "/api/eunha/v1/invite?invite={}",
            invite["code"].as_str().unwrap()
        );
        let resolved: Value = ctx.api.get(&path, None).await.json().await.unwrap();
        assert_eq!(resolved["valid"], true);
        assert_eq!(resolved["bypass_approval"], bypass);
        assert_eq!(resolved["autofollow"], true);
        sqlx::query("UPDATE invites SET uses = 1 WHERE id = $1")
            .bind(invite["id"].as_str().unwrap().parse::<i64>().unwrap())
            .execute(&ctx.db)
            .await
            .unwrap();
        let resolved: Value = ctx.api.get(&path, None).await.json().await.unwrap();
        assert_eq!(resolved["valid"], false);
        assert_eq!(resolved["reason"], "err_invite_maxed");
    }
}

#[tokio::test]
async fn test_personal_invite_status_and_expiration_history() {
    let ctx = TestContext::new("invite-status").await;
    let invite: Value = ctx
        .api
        .post_json(
            "/api/v1/invites",
            Some(&ctx.alice_token),
            &json!({"max_uses": 1}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(invite["valid_for_use"], true);
    let id = invite["id"].as_str().unwrap().parse::<i64>().unwrap();
    sqlx::query("UPDATE invites SET uses = 1 WHERE id = $1")
        .bind(id)
        .execute(&ctx.db)
        .await
        .unwrap();
    let list = invites_of(&ctx, &ctx.alice_token).await;
    assert_eq!(list[0]["valid_for_use"], false);
    assert_eq!(list[0]["expired"], false);
    let response = ctx
        .api
        .delete(&format!("/api/v1/invites/{id}"), &ctx.alice_token)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let list = invites_of(&ctx, &ctx.alice_token).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["expired"], true);
    assert_eq!(list[0]["uses"], 1);
    ctx.api
        .post_json("/api/v1/invites", Some(&ctx.bob_token), &json!({}))
        .await;
    sqlx::query("UPDATE users SET disabled = true WHERE account_id = $1")
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let list = invites_of(&ctx, &ctx.bob_token).await;
    assert_eq!(list[0]["valid_for_use"], false);
}
