//! `Admin::RolesController`.

use super::*;

const MANAGE_ROLES: i64 = 1 << 17;
const MANAGE_REPORTS: i64 = 1 << 4;
const INVITE_USERS: i64 = 1 << 16;

/// Roles: created, edited and deleted under `UserRolePolicy`, with
/// `UserRole`'s validations against the actor's own role, and logged.
#[tokio::test]
async fn test_roles() {
    let ctx = TestContext::new("srv-roles").await;
    // Bob manages roles from position 50, with reports and roles only.
    let bobs = give_role(&ctx, &ctx.bob_id, 50, MANAGE_ROLES | MANAGE_REPORTS).await;
    let post = |body: Value| {
        let api = &ctx.api;
        let token = ctx.bob_token.clone();
        async move {
            api.post_json("/api/v1/admin/roles", Some(&token), &body)
                .await
        }
    };
    let patch = |role: i64, body: Value| {
        let api = &ctx.api;
        let token = ctx.bob_token.clone();
        async move {
            api.patch_json(&format!("/api/v1/admin/roles/{role}"), Some(&token), &body)
                .await
        }
    };

    let cases = [
        (
            json!({"name": "", "color": "blue"}),
            "Validation failed: Name can't be blank, Color is invalid",
        ),
        (
            json!({"name": "Mods", "position": 60}),
            "Validation failed: Position cannot be higher than your current role",
        ),
        (
            json!({"name": "Mods", "permissions_as_keys": ["manage_users"]}),
            "Validation failed: Permissions as keys cannot include permissions your current role does not possess",
        ),
        (
            json!({"name": "Mods", "collection_limit": -1}),
            "Validation failed: Collection limit must be greater than or equal to 0",
        ),
    ];
    for (body, message) in cases {
        assert_eq!(
            error_of(post(body.clone()).await, StatusCode::UNPROCESSABLE_ENTITY).await,
            message,
            "for {body}"
        );
    }

    let mods = json_ok(
        post(json!({
            "name": "Moderators",
            "color": "#ff0000",
            "highlighted": true,
            "position": 20,
            "permissions_as_keys": ["manage_reports", "nonsense"],
        }))
        .await,
    )
    .await;
    assert_eq!(mods["permissions_as_keys"], json!(["manage_reports"]));
    // The computed permissions take in the everyone role's.
    assert_eq!(
        mods["permissions"],
        (MANAGE_REPORTS | INVITE_USERS).to_string()
    );
    assert_eq!(mods["can_update"], true);
    assert_eq!(mods["can_destroy"], true);
    let mods_id = id(mods["id"].as_str().unwrap());

    // Bob may not change his own role's permissions or position.
    assert_eq!(
        error_of(
            patch(
                bobs,
                json!({"position": 40, "permissions_as_keys": ["manage_roles"]})
            )
            .await,
            StatusCode::UNPROCESSABLE_ENTITY
        )
        .await,
        "Validation failed: Permissions as keys cannot be changed with your current role, \
         Position cannot be changed with your current role"
    );
    // But may rename it, and may not delete it.
    json_ok(patch(bobs, json!({"name": "Head moderator"})).await).await;
    assert_eq!(
        ctx.api
            .delete(&format!("/api/v1/admin/roles/{bobs}"), &ctx.bob_token)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    // A role above his is out of reach.
    let admins = give_role(&ctx, &ctx.alice_id, 100, 1).await;
    assert_eq!(
        patch(admins, json!({"name": "Mine now"})).await.status(),
        StatusCode::FORBIDDEN
    );

    // The everyone role takes only `Flags::SAFE`.
    assert_eq!(
        error_of(
            patch(-99, json!({"permissions_as_keys": ["manage_reports"]})).await,
            StatusCode::UNPROCESSABLE_ENTITY
        )
        .await,
        "Validation failed: Permissions as keys include permissions that are not safe for the base role"
    );
    let everyone =
        json_ok(patch(-99, json!({"permissions_as_keys": ["invite_users"]})).await).await;
    assert_eq!(everyone["everyone"], true);
    assert_eq!(everyone["position"], -1);
    assert_eq!(everyone["can_destroy"], false);

    // The list is the assignable roles, lowest first, the everyone role apart.
    let list = json_ok(
        ctx.api
            .get("/api/v1/admin/roles", Some(&ctx.bob_token))
            .await,
    )
    .await;
    // Mastodon's seeded Moderator, Admin and Owner roles are there too.
    let ours: Vec<&Value> = list
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| {
            ["Moderators", "Head moderator", "Role 100"].contains(&r["name"].as_str().unwrap())
        })
        .collect();
    let names: Vec<&str> = ours.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Moderators", "Head moderator", "Role 100"]);
    assert_eq!(ours[1]["users_count"], 1);
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["everyone"] == false));

    // Deleting a role leaves its users with none.
    sqlx::query("UPDATE users SET role_id = $1 WHERE account_id = $2")
        .bind(mods_id)
        .bind(id(&ctx.alice_id))
        .execute(&ctx.db)
        .await
        .unwrap();
    json_ok(
        ctx.api
            .delete(&format!("/api/v1/admin/roles/{mods_id}"), &ctx.bob_token)
            .await,
    )
    .await;
    let alice_role: Option<i64> =
        sqlx::query_scalar("SELECT role_id FROM users WHERE account_id = $1")
            .bind(id(&ctx.alice_id))
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(alice_role, None);

    assert_eq!(
        logs(&ctx).await,
        [
            ("create".to_owned(), "UserRole".to_owned()),
            ("update".to_owned(), "UserRole".to_owned()),
            ("update".to_owned(), "UserRole".to_owned()),
            ("destroy".to_owned(), "UserRole".to_owned()),
        ]
    );
}
