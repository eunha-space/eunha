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
        // Mastodon's `public_invite_url`.
        assert!(invite["url"].as_str().unwrap().contains("/invite/"));
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
            &json!({"all": true, "count": 1, "max_uses": 5}),
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
        assert!(invites[0]["comment"].is_null());
        assert_eq!(invites[0]["bypass_approval"], true);
        assert!(invites[0]["grant"]["id"].is_string());
    }
}

/// A grant and an invite are made from a form as from JSON, their numbers
/// and booleans read from the form's strings.
#[tokio::test]
async fn test_grant_and_invite_from_a_form() {
    let ctx = TestContext::new("invite-grant-form").await;
    make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;

    let result: Value = ctx
        .api
        .post_form(
            "/api/eunha/v1/invite_grants",
            Some(&ctx.alice_token),
            &[
                ("account_id", &ctx.bob_id),
                ("count", "2"),
                ("max_uses", "3"),
            ],
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(result["granted"].as_i64(), Some(2));
    let bobs = invites_of(&ctx, &ctx.bob_token).await;
    assert_eq!(bobs.len(), 2);
    assert_eq!(bobs[0]["max_uses"].as_i64(), Some(3));

    let invite: Value = ctx
        .api
        .post_form(
            "/api/v1/invites",
            Some(&ctx.alice_token),
            &[("max_uses", "4"), ("autofollow", "1"), ("comment", "hi")],
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(invite["max_uses"].as_i64(), Some(4));
    assert_eq!(invite["autofollow"], true);
    assert_eq!(invite["comment"], "hi");
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
    let carol = tree["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["username"].as_str() == Some("carol"))
        .expect("carol should be in the tree");
    assert_eq!(
        carol["parent_id"], ctx.bob_id,
        "carol should hang under bob, whose code she used",
    );
    // And bob's own list says who came through the link.
    let invite = &invites_of(&ctx, &ctx.bob_token).await[0];
    assert_eq!(invite["joined"]["accounts"][0]["acct"], "carol");
    assert_eq!(invite["joined"]["pending"], 0);
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
            .post_json(
                "/api/eunha/v1/invite_grants",
                None,
                &json!({"all": true, "count": 1}),
            )
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
        json!({"all": true, "count": 0}),
        json!({"all": true, "count": 26}),
        json!({"all": true, "count": 1, "max_uses": 0}),
        json!({"all": true, "count": 1, "max_uses": 101}),
        json!({"all": true, "count": 1, "expires_in": -1}),
        json!({"all": true, "count": 1, "expires_in": i64::MAX}),
        json!({"account_id": "", "count": 1}),
        // Naming nobody is not naming everybody.
        json!({"count": 1}),
        json!({"account_id": ctx.bob_id, "all": true, "count": 1}),
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

/// Staff grants approve admission without adding bypass permission to the owner.
/// Self-created/legacy codes remain role-based, including after subsequent grants.
#[tokio::test]
async fn test_staff_grants_bypass_review_and_preserve_batches() {
    let ctx = TestContext::with_approval_required("grant-admission").await;
    make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    let legacy: Value = ctx
        .api
        .post_json(
            "/api/v1/invites",
            Some(&ctx.bob_token),
            &json!({"max_uses": 1}),
        )
        .await
        .json()
        .await
        .unwrap();
    close_invites(&ctx).await;
    for expiry in [86400, 604800] {
        let response = ctx
            .api
            .post_json(
                "/api/eunha/v1/invite_grants",
                Some(&ctx.alice_token),
                &json!({"account_id": ctx.bob_id, "count": 2, "expires_in": expiry}),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    let invites = invites_of(&ctx, &ctx.bob_token).await;
    assert_eq!(invites.len(), 5);
    let grants: std::collections::HashSet<_> = invites
        .iter()
        .filter_map(|i| i["grant"]["id"].as_str())
        .collect();
    assert_eq!(grants.len(), 2);
    let granted = invites.iter().find(|i| !i["grant"].is_null()).unwrap();
    assert_eq!(granted["grant"]["granted_by"], "alice");
    assert_eq!(granted["bypass_approval"], true);
    let code = granted["code"].as_str().unwrap();
    let resolved: Value = ctx
        .api
        .get(&format!("/api/eunha/v1/invite?invite={code}"), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(resolved["bypass_approval"], true);
    let legacy_code = legacy["code"].as_str().unwrap();
    let resolved: Value = ctx
        .api
        .get(&format!("/api/eunha/v1/invite?invite={legacy_code}"), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(resolved["bypass_approval"], false);
    let signup = ctx
        .sign_up(&json!({"username": "carol", "email": "carol@example.com",
        "password": "a-long-enough-password", "agreement": true, "invite_code": code}))
        .await;
    assert_eq!(signup.status(), StatusCode::OK);
    let (approved, confirmed, invite_id): (bool, bool, i64) = sqlx::query_as(
        "SELECT u.approved, u.confirmed_at IS NOT NULL, u.invite_id FROM users u JOIN accounts a ON a.id = u.account_id WHERE a.username = 'carol'")
        .fetch_one(&ctx.db).await.unwrap();
    assert!(approved);
    assert!(
        !confirmed,
        "staff approval does not replace email confirmation"
    );
    assert_eq!(invite_id.to_string(), granted["id"].as_str().unwrap());
    let maxed: Value = ctx
        .api
        .get(&format!("/api/eunha/v1/invite?invite={code}"), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(maxed["valid"], false);
    assert_eq!(maxed["reason"], "err_invite_maxed");
    assert_eq!(
        invites_of(&ctx, &ctx.bob_token)
            .await
            .iter()
            .filter(|i| i["uses"] == 1)
            .count(),
        1
    );
    let suspended = ctx
        .api
        .post_json(
            "/api/eunha/v1/invite_grants",
            Some(&ctx.alice_token),
            &json!({"account_id": ctx.bob_id, "count": 1}),
        )
        .await;
    assert_eq!(suspended.status(), StatusCode::OK);
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let other = invites
        .iter()
        .find(|i| !i["grant"].is_null() && i["code"] != code)
        .unwrap()["code"]
        .as_str()
        .unwrap();
    let invalid: Value = ctx
        .api
        .get(&format!("/api/eunha/v1/invite?invite={other}"), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        invalid["valid"], false,
        "grant bypass must still validate the owner"
    );
}

#[tokio::test]
async fn test_grant_recipients_match_bulk_eligibility_and_require_staff() {
    let ctx = TestContext::new("grant-recipients").await;
    make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    assert_eq!(
        ctx.api
            .get(
                "/api/eunha/v1/invite_grants/recipients",
                Some(&ctx.bob_token)
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        ctx.api
            .get("/api/eunha/v1/invite_grants/recipients", None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    sqlx::query("UPDATE users SET disabled = true WHERE account_id = $1")
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let recipients: Vec<Value> = ctx
        .api
        .get(
            "/api/eunha/v1/invite_grants/recipients",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(recipients.len(), 1);
    assert_eq!(recipients[0]["id"], ctx.alice_id);
    let response: Value = ctx
        .api
        .post_json(
            "/api/eunha/v1/invite_grants",
            Some(&ctx.alice_token),
            &json!({"all": true, "count": 3}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(response["accounts"], 1);
    assert_eq!(response["granted"], 3);
    assert!(invites_of(&ctx, &ctx.bob_token).await.is_empty());
}

/// Staff can review every handout and take one back. Revoking expires the
/// links still usable, as revoking one invite does, and keeps every row.
#[tokio::test]
async fn test_grants_are_listed_and_revocable() {
    let ctx = TestContext::new("invite-grant-revoke").await;
    make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    ctx.api
        .post_json(
            "/api/eunha/v1/invite_grants",
            Some(&ctx.alice_token),
            &json!({"account_id": ctx.bob_id, "count": 2}),
        )
        .await;

    // Members cannot see the record.
    assert_eq!(
        ctx.api
            .get("/api/eunha/v1/invite_grants", Some(&ctx.bob_token))
            .await
            .status(),
        StatusCode::FORBIDDEN,
    );
    let grants: Vec<Value> = ctx
        .api
        .get("/api/eunha/v1/invite_grants", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(grants.len(), 1);
    let grant = &grants[0];
    assert_eq!(grant["granted_by"], "alice");
    assert_eq!(grant["recipient"], "bob");
    assert_eq!(grant["recipients"], 1);
    assert_eq!(grant["links"], 2);
    assert_eq!(grant["available"], 2);
    assert!(grant["revoked_at"].is_null());

    let id = grant["id"].as_str().unwrap();
    assert_eq!(
        ctx.api
            .post_json(
                &format!("/api/eunha/v1/invite_grants/{id}/revoke"),
                Some(&ctx.bob_token),
                &json!({}),
            )
            .await
            .status(),
        StatusCode::FORBIDDEN,
    );
    let revoked: Value = ctx
        .api
        .post_json(
            &format!("/api/eunha/v1/invite_grants/{id}/revoke"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(revoked["expired"], 2);

    let grants: Vec<Value> = ctx
        .api
        .get("/api/eunha/v1/invite_grants", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(grants[0]["available"], 0);
    assert_eq!(grants[0]["revoked_by"], "alice");
    let invites = invites_of(&ctx, &ctx.bob_token).await;
    assert_eq!(invites.len(), 2, "revoking keeps the rows");
    assert!(invites.iter().all(|i| i["valid_for_use"] == false));

    assert_eq!(
        ctx.api
            .post_json(
                "/api/eunha/v1/invite_grants/999999/revoke",
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await
            .status(),
        StatusCode::NOT_FOUND,
    );
}

/// A staff-granted single-use link admits one person without review, however
/// many use it at once: the second is treated as Mastodon treats anyone with a
/// used-up code, which on an invite-only instance is a refusal.
#[tokio::test]
async fn test_a_granted_link_admits_one_person_at_once() {
    let ctx = TestContext::new("invite-grant-race").await;
    make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    crate::helpers::set_setting(&ctx.db, "registrations_mode", "none").await;
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

    let body = |name: &str| {
        json!({
            "username": name,
            "email": format!("{name}@example.com"),
            "password": "a-long-enough-password",
            "agreement": true,
            "invite_code": code,
        })
    };
    let (one, two, three) = (body("racer1"), body("racer2"), body("racer3"));
    let (a, b, c) = tokio::join!(ctx.sign_up(&one), ctx.sign_up(&two), ctx.sign_up(&three));
    let admitted = [a.status(), b.status(), c.status()]
        .iter()
        .filter(|s| **s == StatusCode::OK)
        .count();
    assert_eq!(admitted, 1, "{:?}", [a.status(), b.status(), c.status()]);
    let uses: i32 = sqlx::query_scalar("SELECT uses FROM invites WHERE code = $1")
        .bind(&code)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(uses, 1);
}
