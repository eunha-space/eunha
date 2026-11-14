use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

/// Create an invite, list it, then delete (expire) it.
#[tokio::test]
async fn test_invite_lifecycle() {
    let ctx = TestContext::new("invite").await;

    let invite: Value = ctx
        .api
        .post_json("/api/v1/invites", Some(&ctx.alice_token), &json!({}))
        .await
        .json()
        .await
        .unwrap();
    let invite_id = invite["id"].as_str().unwrap().to_string();
    assert!(invite["code"].as_str().is_some());
    assert!(invite["url"].as_str().is_some());

    let invites: Vec<Value> = ctx
        .api
        .get("/api/v1/invites", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(invites
        .iter()
        .any(|i| i["id"].as_str() == Some(invite_id.as_str())));

    let del_resp = ctx
        .api
        .delete(&format!("/api/v1/invites/{invite_id}"), &ctx.alice_token)
        .await;
    assert_eq!(del_resp.status(), StatusCode::OK);

    // Deleting an invite expires it (Mastodon Expireable#expire!) rather than
    // removing the row, so it still appears in the list but now has expires_at set.
    let after: Vec<Value> = ctx
        .api
        .get("/api/v1/invites", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let expired = after
        .iter()
        .find(|i| i["id"].as_str() == Some(invite_id.as_str()))
        .expect("expired invite should still be listed");
    assert!(expired["expires_at"].as_str().is_some());
}

/// `Invite#valid_for_use?` wants its inviter `functional?`, which a role
/// requiring two-factor authentication the inviter lacks is not.
#[tokio::test]
async fn test_an_invite_wants_a_functional_inviter() {
    let ctx = TestContext::new("invite-functional").await;
    let invite: Value = ctx
        .api
        .post_json("/api/v1/invites", Some(&ctx.alice_token), &json!({}))
        .await
        .json()
        .await
        .unwrap();
    // `Api::V1::InvitesController#show`, which asks `valid_for_use?`.
    let lookup = |code: String| {
        let api = &ctx.api;
        async move {
            api.get(&format!("/invite/{code}.json"), None)
                .await
                .status()
        }
    };
    let code = invite["code"].as_str().unwrap().to_owned();
    assert_eq!(lookup(code.clone()).await, StatusCode::OK);
    sqlx::query("UPDATE user_roles SET require_2fa = true WHERE id = -99")
        .execute(&ctx.db)
        .await
        .unwrap();
    assert_eq!(lookup(code).await, StatusCode::UNAUTHORIZED);
}

/// Invite with max_uses and expires_in round-trips those fields.
#[tokio::test]
async fn test_invite_with_options() {
    let ctx = TestContext::new("invite-opts").await;

    let invite: Value = ctx
        .api
        .post_json(
            "/api/v1/invites",
            Some(&ctx.alice_token),
            &json!({"max_uses": 5, "expires_in": 3600}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(invite["max_uses"].as_i64(), Some(5));
    assert!(invite["expires_at"].as_str().is_some());
}

/// Invite with autofollow and comment round-trips those fields on create + list.
#[tokio::test]
async fn test_invite_autofollow_and_comment() {
    let ctx = TestContext::new("invite-autofollow").await;

    let invite: Value = ctx
        .api
        .post_json(
            "/api/v1/invites",
            Some(&ctx.alice_token),
            &json!({"autofollow": true, "comment": "come join us"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let invite_id = invite["id"].as_str().unwrap().to_string();
    assert_eq!(invite["autofollow"].as_bool(), Some(true));
    assert_eq!(invite["comment"].as_str(), Some("come join us"));

    let invites: Vec<Value> = ctx
        .api
        .get("/api/v1/invites", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let listed = invites
        .iter()
        .find(|i| i["id"].as_str() == Some(invite_id.as_str()))
        .expect("invite should be listed");
    assert_eq!(listed["autofollow"].as_bool(), Some(true));
    assert_eq!(listed["comment"].as_str(), Some("come join us"));
}

/// A comment longer than Mastodon's 420-character limit is rejected.
#[tokio::test]
async fn test_invite_comment_too_long() {
    let ctx = TestContext::new("invite-longcomment").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/invites",
            Some(&ctx.alice_token),
            &json!({"comment": "x".repeat(421)}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Mastodon's `InvitePolicy#create?` is `role.can?(:invite_users)`, and that
/// permission lives on the everyone role (`UserRole::Flags::DEFAULT`). Taking
/// it off that role — which is how an instance decides to hand invites out
/// itself — stops an ordinary member creating invites, while staff keep the
/// ability through their own role. Listing stays open: an admin can mint codes
/// into a member's account, and they have to be able to read them.
#[tokio::test]
async fn test_invite_users_permission() {
    let ctx = TestContext::new("invite-perm").await;

    // Everyone role as seeded: alice may invite.
    assert_eq!(
        ctx.api
            .post_json("/api/v1/invites", Some(&ctx.alice_token), &json!({}))
            .await
            .status(),
        StatusCode::OK,
    );

    // Clear `invite_users` from the everyone role (UserRole::EVERYONE_ROLE_ID).
    sqlx::query!("UPDATE user_roles SET permissions = 0 WHERE id = -99")
        .execute(&ctx.db)
        .await
        .unwrap();

    assert_eq!(
        ctx.api
            .post_json("/api/v1/invites", Some(&ctx.alice_token), &json!({}))
            .await
            .status(),
        StatusCode::FORBIDDEN,
    );
    assert_eq!(
        ctx.api
            .get("/api/v1/invites", Some(&ctx.alice_token))
            .await
            .status(),
        StatusCode::OK,
        "a member should still be able to read invites they hold",
    );

    // An admin's own role carries the administrator flag, which grants
    // everything (`computed_permissions` returns `Flags::ALL`), so bob still
    // may — this is a setting about members, not a feature that was turned off.
    crate::helpers::make_admin(&ctx.db, ctx.bob_id.parse().unwrap()).await;
    assert_eq!(
        ctx.api
            .post_json("/api/v1/invites", Some(&ctx.bob_token), &json!({}))
            .await
            .status(),
        StatusCode::OK,
    );
}

/// `verify_credentials` reports the *computed* permissions — the account's own
/// role unioned with the everyone role's — the way Mastodon's `RoleSerializer`
/// does. A client reading the bit to decide what to offer has to agree with
/// what the server will authorize.
#[tokio::test]
async fn test_role_permissions_are_computed() {
    let ctx = TestContext::new("invite-perm-role").await;
    const INVITE_USERS: i64 = 1 << 16;

    async fn permissions(ctx: &TestContext, token: &str) -> i64 {
        let me: Value = ctx
            .api
            .get("/api/v1/accounts/verify_credentials", Some(token))
            .await
            .json()
            .await
            .unwrap();
        me["role"]["permissions"]
            .as_str()
            .unwrap()
            .parse::<i64>()
            .unwrap()
    }

    // A member with no role of its own reads the everyone role's permissions.
    assert_eq!(
        permissions(&ctx, &ctx.alice_token).await & INVITE_USERS,
        INVITE_USERS,
    );

    // An administrator reads `Flags::ALL`, invite_users among it, even though
    // upstream's Admin role does not list that permission itself.
    crate::helpers::make_admin(&ctx.db, ctx.bob_id.parse().unwrap()).await;
    assert_eq!(
        permissions(&ctx, &ctx.bob_token).await,
        (1 << 23) - 1,
        "administrator should compute to UserRole::Flags::ALL",
    );
}

/// Mastodon's invite link, `/invite/:invite_code`, and the JSON the apps read
/// from it (`Api::V1::InvitesController#show`): 404 for no such code, 401 for
/// one not good for use, 403 for a sign-up IP block, and otherwise the code and
/// the instance's API. Links handed out point there too.
#[tokio::test]
async fn test_public_invite_link_is_mastodons() {
    let ctx = TestContext::new("invite-public-link").await;
    let invite: Value = ctx
        .api
        .post_json("/api/v1/invites", Some(&ctx.alice_token), &json!({}))
        .await
        .json()
        .await
        .unwrap();
    let code = invite["code"].as_str().unwrap().to_string();
    assert!(
        invite["url"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/invite/{code}")),
        "{invite}",
    );

    let json_get = |path: String| {
        let api = &ctx.api;
        async move {
            api.http
                .get(api.url(&path))
                .header("host", &api.host)
                .header("accept", "application/json")
                .send()
                .await
                .unwrap()
        }
    };

    let ok = json_get(format!("/invite/{code}")).await;
    assert_eq!(ok.status(), StatusCode::OK);
    let body: Value = ok.json().await.unwrap();
    assert_eq!(body["invite_code"], code.as_str());
    assert!(body["instance_api_url"]
        .as_str()
        .unwrap()
        .ends_with("/api/v2/instance"));
    // The `.json` suffix asks the same, whatever the Accept header says.
    let suffixed = ctx.api.get(&format!("/invite/{code}.json"), None).await;
    assert_eq!(suffixed.status(), StatusCode::OK);
    assert_eq!(
        suffixed.json::<Value>().await.unwrap()["invite_code"],
        code.as_str()
    );

    let missing = json_get("/invite/nosuchcode".into()).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        missing.json::<Value>().await.unwrap()["error"],
        "Record not found"
    );

    ctx.api
        .delete(
            &format!("/api/v1/invites/{}", invite["id"].as_str().unwrap()),
            &ctx.alice_token,
        )
        .await;
    let expired = json_get(format!("/invite/{code}")).await;
    assert_eq!(expired.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        expired.json::<Value>().await.unwrap()["error"],
        "This invite is not valid"
    );

    // A browser is sent to the sign-up form, not given JSON.
    let page = ctx.api.get(&format!("/invite/{code}"), None).await;
    assert_eq!(
        page.headers().get("location").and_then(|v| v.to_str().ok()),
        Some(format!("/auth/signup?invite={code}").as_str()),
    );
}

/// What `GET /api/v1/invites` adds about each link: who joined with it, by name
/// unless they hide their network, and how many have not finished joining.
#[tokio::test]
async fn test_invites_say_who_joined() {
    let ctx = TestContext::new("invite-joined").await;
    let invite: Value = ctx
        .api
        .post_json("/api/v1/invites", Some(&ctx.alice_token), &json!({}))
        .await
        .json()
        .await
        .unwrap();
    let id: i64 = invite["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE users SET invite_id = $1 WHERE account_id = $2")
        .bind(id)
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let usage = joined(&ctx.api, &ctx.alice_token).await;
    assert_eq!(usage["accounts"][0]["acct"], "bob");
    assert_eq!(usage["hidden"], 0);

    sqlx::query("UPDATE accounts SET hide_collections = true WHERE id = $1")
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let usage = joined(&ctx.api, &ctx.alice_token).await;
    assert!(usage["accounts"].as_array().unwrap().is_empty());
    assert_eq!(usage["hidden"], 1);

    sqlx::query("UPDATE users SET approved = false WHERE account_id = $1")
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let usage = joined(&ctx.api, &ctx.alice_token).await;
    assert_eq!(usage["hidden"], 0);
    assert_eq!(usage["pending"], 1);
}

async fn joined(api: &crate::helpers::ApiClient, token: &str) -> Value {
    let list: Vec<Value> = api
        .get("/api/v1/invites", Some(token))
        .await
        .json()
        .await
        .unwrap();
    list[0]["joined"].clone()
}
