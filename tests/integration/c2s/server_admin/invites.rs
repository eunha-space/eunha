//! `Admin::InvitesController`.

use super::*;

const MANAGE_INVITES: i64 = 1 << 11;

/// Every invite on the server, for `manage_invites`: filtered by whether it
/// is still usable, expired by the invites API, and deactivated all at once.
#[tokio::test]
async fn test_admin_invites() {
    let ctx = TestContext::new("srv-invites").await;
    let made = json_ok(
        ctx.api
            .post_json(
                "/api/v1/invites",
                Some(&ctx.alice_token),
                &json!({"max_uses": 5}),
            )
            .await,
    )
    .await;
    json_ok(
        ctx.api
            .post_json("/api/v1/invites", Some(&ctx.alice_token), &json!({}))
            .await,
    )
    .await;
    crate::helpers::grant_admin_scopes(&ctx.db, id(&ctx.bob_id)).await;
    assert_eq!(
        ctx.api
            .get("/api/v1/admin/invites", Some(&ctx.bob_token))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    give_role(&ctx, &ctx.bob_id, 10, MANAGE_INVITES).await;
    let all = json_ok(
        ctx.api
            .get("/api/v1/admin/invites", Some(&ctx.bob_token))
            .await,
    )
    .await;
    assert_eq!(all.as_array().unwrap().len(), 2);
    assert_eq!(all[1]["id"], made["id"]);
    assert_eq!(all[1]["max_uses"], 5);
    assert_eq!(all[1]["valid_for_use"], true);
    assert_eq!(all[1]["account"]["id"], ctx.alice_id.as_str());

    // A moderator may expire anyone's invite through the invites API.
    let expired = ctx
        .api
        .delete(
            &format!("/api/v1/invites/{}", made["id"].as_str().unwrap()),
            &ctx.bob_token,
        )
        .await;
    assert_eq!(expired.status(), StatusCode::OK);
    let expired = json_ok(
        ctx.api
            .get("/api/v1/admin/invites?expired=1", Some(&ctx.bob_token))
            .await,
    )
    .await;
    assert_eq!(expired.as_array().unwrap().len(), 1);
    assert_eq!(expired[0]["expired"], true);
    assert_eq!(expired[0]["valid_for_use"], false);

    json_ok(
        ctx.api
            .post_json(
                "/api/v1/admin/invites/deactivate_all",
                Some(&ctx.bob_token),
                &json!({}),
            )
            .await,
    )
    .await;
    let available = json_ok(
        ctx.api
            .get("/api/v1/admin/invites?available=1", Some(&ctx.bob_token))
            .await,
    )
    .await;
    assert_eq!(available, json!([]));
}
