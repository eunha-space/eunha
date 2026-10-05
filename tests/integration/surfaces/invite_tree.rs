use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::TestContext;

/// The eunha invite-tree endpoint requires authentication.
#[tokio::test]
async fn test_invite_tree_requires_auth() {
    let ctx = TestContext::new("invite-tree-auth").await;
    let resp = ctx.api.get("/api/eunha/v1/invite_tree", None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Invitees are nested under the account whose invite they used; uninvited
/// members are roots.
#[tokio::test]
async fn test_invite_tree_nests_invitees() {
    let ctx = TestContext::new("invite-tree-nest").await;
    let alice_account_id: i64 = ctx.alice_id.parse().unwrap();
    let bob_account_id: i64 = ctx.bob_id.parse().unwrap();

    // Make bob look like he signed up through one of alice's invites.
    let alice_user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE account_id = $1")
        .bind(alice_account_id)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    let invite_id: i64 = sqlx::query_scalar(
        "INSERT INTO invites (code, user_id, uses, created_at, updated_at)
         VALUES ($1, $2, 1, now(), now()) RETURNING id",
    )
    .bind("invitetree1")
    .bind(alice_user_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    sqlx::query("UPDATE users SET invite_id = $1 WHERE account_id = $2")
        .bind(invite_id)
        .bind(bob_account_id)
        .execute(&ctx.db)
        .await
        .unwrap();

    let resp = ctx
        .api
        .get("/api/eunha/v1/invite_tree", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    assert!(body["total"].as_u64().unwrap() >= 2);
    let roots = body["roots"].as_array().unwrap();

    // alice is a registration root; bob is nested under her, not a root.
    let alice = roots
        .iter()
        .find(|n| n["id"].as_str() == Some(ctx.alice_id.as_str()))
        .expect("alice should be a root");
    assert!(
        !roots
            .iter()
            .any(|n| n["id"].as_str() == Some(ctx.bob_id.as_str())),
        "bob should not be a root",
    );
    assert_eq!(alice["root_reason"], "no_recorded_inviter");
    let children = alice["children"].as_array().unwrap();
    assert!(
        children
            .iter()
            .any(|n| n["id"].as_str() == Some(ctx.bob_id.as_str())),
        "bob should be nested under alice",
    );
}

/// Unapproved (and unconfirmed / suspended) accounts are excluded from the tree.
#[tokio::test]
async fn test_invite_tree_excludes_unapproved() {
    let ctx = TestContext::new("invite-tree-unapproved").await;
    let bob_account_id: i64 = ctx.bob_id.parse().unwrap();

    // Mark bob as pending approval.
    sqlx::query("UPDATE users SET approved = false WHERE account_id = $1")
        .bind(bob_account_id)
        .execute(&ctx.db)
        .await
        .unwrap();

    let resp = ctx
        .api
        .get("/api/eunha/v1/invite_tree", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    fn contains(nodes: &[Value], id: &str) -> bool {
        nodes.iter().any(|n| {
            n["id"].as_str() == Some(id)
                || n["children"].as_array().is_some_and(|c| contains(c, id))
        })
    }
    let roots = body["roots"].as_array().unwrap();
    assert!(
        !contains(roots, &ctx.bob_id),
        "unapproved bob should not appear anywhere in the tree",
    );
}

#[tokio::test]
async fn test_invite_tree_explains_excluded_inviter() {
    let ctx = TestContext::new("invite-tree-missing-parent").await;
    sqlx::query(
        "INSERT INTO eunha.invite_lineage (account_id, inviter_account_id) VALUES ($1, $2)",
    )
    .bind(ctx.alice_id.parse::<i64>().unwrap())
    .bind(ctx.bob_id.parse::<i64>().unwrap())
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query("UPDATE users SET approved = false WHERE account_id = $1")
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let body: Value = ctx
        .api
        .get("/api/eunha/v1/invite_tree", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["total"], 1);
    assert_eq!(body["roots"][0]["id"], ctx.alice_id);
    assert_eq!(body["roots"][0]["root_reason"], "inviter_unavailable");
    assert!(body["roots"][0].get("inviter_account_id").is_none());
}

#[tokio::test]
async fn test_invite_tree_retains_cyclic_lineage_members_once() {
    let ctx = TestContext::new("invite-tree-cycle").await;
    let alice = ctx.alice_id.parse::<i64>().unwrap();
    let bob = ctx.bob_id.parse::<i64>().unwrap();
    sqlx::query("INSERT INTO eunha.invite_lineage (account_id, inviter_account_id) VALUES ($1, $2), ($2, $1)")
        .bind(alice).bind(bob).execute(&ctx.db).await.unwrap();
    let body: Value = ctx
        .api
        .get("/api/eunha/v1/invite_tree", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["total"], 2);
    assert_eq!(body["roots"].as_array().unwrap().len(), 1);
    let root = &body["roots"][0];
    assert_eq!(root["root_reason"], "lineage_unavailable");
    assert_eq!(root["children"].as_array().unwrap().len(), 1);
    assert_ne!(root["id"], root["children"][0]["id"]);
    assert!(root["children"][0]["children"]
        .as_array()
        .unwrap()
        .is_empty());
}
