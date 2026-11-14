use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::{make_admin, seed_user, TestContext};

/// Record that `invitee` signed up through an invite `inviter` made.
async fn invited(ctx: &TestContext, inviter: i64, invitee: i64) {
    let invite_id: i64 = sqlx::query_scalar(
        "INSERT INTO invites (code, user_id, uses, created_at, updated_at)
         VALUES (md5(random()::text), (SELECT id FROM users WHERE account_id = $1), 1, now(), now())
         RETURNING id",
    )
    .bind(inviter)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    sqlx::query("UPDATE users SET invite_id = $1 WHERE account_id = $2")
        .bind(invite_id)
        .bind(invitee)
        .execute(&ctx.db)
        .await
        .unwrap();
}

async fn tree(ctx: &TestContext, token: &str, query: &str) -> Value {
    let resp = ctx
        .api
        .get(&format!("/api/eunha/v1/invite_tree{query}"), Some(token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    resp.json().await.unwrap()
}

fn member<'a>(body: &'a Value, id: &str) -> Option<&'a Value> {
    body["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"].as_str() == Some(id))
}

fn id(ctx_id: &str) -> i64 {
    ctx_id.parse().unwrap()
}

/// The eunha invite-tree endpoint requires authentication.
#[tokio::test]
async fn test_invite_tree_requires_auth() {
    let ctx = TestContext::new("invite-tree-auth").await;
    let resp = ctx.api.get("/api/eunha/v1/invite_tree", None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Invitees name their inviter as parent; uninvited members are roots, and
/// every parent comes before its children.
#[tokio::test]
async fn test_invite_tree_nests_invitees() {
    let ctx = TestContext::new("invite-tree-nest").await;
    invited(&ctx, id(&ctx.alice_id), id(&ctx.bob_id)).await;

    let body = tree(&ctx, &ctx.alice_token, "").await;
    assert!(body["total"].as_u64().unwrap() >= 2);
    let alice = member(&body, &ctx.alice_id).expect("alice is listed");
    assert_eq!(alice["kind"], "member");
    assert!(alice["parent_id"].is_null());
    assert_eq!(alice["root_reason"], "no_recorded_inviter");
    // The still avatar too, which the web client shows until hovered; without
    // an uploaded GIF it is the avatar itself.
    assert_eq!(alice["avatar_static"], alice["avatar"]);
    assert!(alice["joined_at"].as_str().is_some());
    let bob = member(&body, &ctx.bob_id).expect("bob is listed");
    assert_eq!(bob["parent_id"], ctx.alice_id);
    assert!(bob.get("root_reason").is_none());

    let members = body["members"].as_array().unwrap();
    let at = |id: &str| members.iter().position(|m| m["id"] == id).unwrap();
    assert!(at(&ctx.alice_id) < at(&ctx.bob_id));
}

/// Unapproved (and unconfirmed / suspended) accounts are excluded from the tree.
#[tokio::test]
async fn test_invite_tree_excludes_unapproved() {
    let ctx = TestContext::new("invite-tree-unapproved").await;
    sqlx::query("UPDATE users SET approved = false WHERE account_id = $1")
        .bind(id(&ctx.bob_id))
        .execute(&ctx.db)
        .await
        .unwrap();
    let body = tree(&ctx, &ctx.alice_token, "").await;
    assert!(
        member(&body, &ctx.bob_id).is_none(),
        "unapproved bob should not appear anywhere in the tree",
    );
}

/// An inviter who is not a member any more stands as one anonymous former
/// member, so their invitees keep their branch, and nothing names them.
#[tokio::test]
async fn test_invite_tree_keeps_a_former_members_branch() {
    let ctx = TestContext::new("invite-tree-former").await;
    let (carol, _) = seed_user(&ctx.db, &ctx.domain, "carol", "carol@example.test").await;
    // bob invited alice and carol, then was suspended.
    invited(&ctx, id(&ctx.bob_id), id(&ctx.alice_id)).await;
    invited(&ctx, id(&ctx.bob_id), carol).await;
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(id(&ctx.bob_id))
        .execute(&ctx.db)
        .await
        .unwrap();

    let body = tree(&ctx, &ctx.alice_token, "").await;
    assert_eq!(body["total"], 2);
    assert!(member(&body, &ctx.bob_id).is_none());
    let alice = member(&body, &ctx.alice_id).unwrap();
    let carol = member(&body, &carol.to_string()).unwrap();
    let parent = alice["parent_id"].as_str().unwrap();
    assert_eq!(carol["parent_id"], parent, "one placeholder for both");
    let former = member(&body, parent).unwrap();
    assert_eq!(former["kind"], "former_member");
    assert!(former["parent_id"].is_null());
    assert!(former.get("username").is_none() && former.get("avatar").is_none());
    assert_ne!(parent, ctx.bob_id);
}

/// The lineage kept when an inviter deletes their user record still places
/// their invitees under one former member.
#[tokio::test]
async fn test_invite_tree_reads_kept_lineage() {
    let ctx = TestContext::new("invite-tree-lineage").await;
    sqlx::query(
        "INSERT INTO eunha.invite_lineage (account_id, inviter_account_id) VALUES ($1, $2)",
    )
    .bind(id(&ctx.alice_id))
    .bind(id(&ctx.bob_id))
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query("UPDATE users SET approved = false WHERE account_id = $1")
        .bind(id(&ctx.bob_id))
        .execute(&ctx.db)
        .await
        .unwrap();
    let body = tree(&ctx, &ctx.alice_token, "").await;
    assert_eq!(body["total"], 1);
    let alice = member(&body, &ctx.alice_id).unwrap();
    let former = member(&body, alice["parent_id"].as_str().unwrap()).unwrap();
    assert_eq!(former["kind"], "former_member");
}

/// A member who hides their network (`hide_collections`) is an anonymous place
/// to everyone else, keeping their branch; they see themselves, and staff who
/// hand out invites see everyone.
#[tokio::test]
async fn test_invite_tree_respects_a_hidden_network() {
    let ctx = TestContext::new("invite-tree-hidden").await;
    let (carol, carol_token) = seed_user(&ctx.db, &ctx.domain, "carol", "carol@example.test").await;
    invited(&ctx, id(&ctx.alice_id), id(&ctx.bob_id)).await;
    invited(&ctx, id(&ctx.bob_id), carol).await;
    sqlx::query("UPDATE accounts SET hide_collections = true WHERE id = $1")
        .bind(id(&ctx.bob_id))
        .execute(&ctx.db)
        .await
        .unwrap();

    let body = tree(&ctx, &carol_token, "").await;
    assert!(member(&body, &ctx.bob_id).is_none());
    let carol_node = member(&body, &carol.to_string()).unwrap();
    let hidden = member(&body, carol_node["parent_id"].as_str().unwrap()).unwrap();
    assert_eq!(hidden["kind"], "hidden_member");
    assert_eq!(hidden["parent_id"], ctx.alice_id);
    assert!(hidden.get("username").is_none() && hidden.get("joined_at").is_none());
    let raw = serde_json::to_string(&body).unwrap();
    assert!(!raw.contains("\"bob\""), "bob is not named: {raw}");

    // Bob sees himself.
    let own = tree(&ctx, &ctx.bob_token, "").await;
    assert_eq!(member(&own, &ctx.bob_id).unwrap()["kind"], "member");

    // Staff see him too.
    make_admin(&ctx.db, id(&ctx.alice_id)).await;
    let staff = tree(&ctx, &ctx.alice_token, "").await;
    assert_eq!(member(&staff, &ctx.bob_id).unwrap()["acct"], "bob");
}

/// `?scope=branch` is the viewer's line up to a root and everyone below them,
/// without the rest of the instance.
#[tokio::test]
async fn test_invite_tree_branch_scope() {
    let ctx = TestContext::new("invite-tree-branch").await;
    let (carol, _) = seed_user(&ctx.db, &ctx.domain, "carol", "carol@example.test").await;
    let (dave, _) = seed_user(&ctx.db, &ctx.domain, "dave", "dave@example.test").await;
    let (erin, _) = seed_user(&ctx.db, &ctx.domain, "erin", "erin@example.test").await;
    // alice -> bob -> carol, alice -> dave, erin uninvited.
    invited(&ctx, id(&ctx.alice_id), id(&ctx.bob_id)).await;
    invited(&ctx, id(&ctx.bob_id), carol).await;
    invited(&ctx, id(&ctx.alice_id), dave).await;

    let body = tree(&ctx, &ctx.bob_token, "?scope=branch").await;
    let ids: Vec<&str> = body["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![
            ctx.alice_id.as_str(),
            ctx.bob_id.as_str(),
            &carol.to_string()
        ]
    );
    assert!(!ids.contains(&dave.to_string().as_str()));
    assert!(!ids.contains(&erin.to_string().as_str()));
    assert_eq!(body["total"], 5, "the instance's total, whatever the view");
}

/// The tree answers `If-None-Match` with 304 while nothing has changed.
#[tokio::test]
async fn test_invite_tree_etag() {
    let ctx = TestContext::new("invite-tree-etag").await;
    let first = ctx
        .api
        .get("/api/eunha/v1/invite_tree", Some(&ctx.alice_token))
        .await;
    let etag = first.headers()["etag"].to_str().unwrap().to_string();
    let again = ctx
        .api
        .http
        .get(ctx.api.url("/api/eunha/v1/invite_tree"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::NOT_MODIFIED);

    invited(&ctx, id(&ctx.alice_id), id(&ctx.bob_id)).await;
    let changed = ctx
        .api
        .http
        .get(ctx.api.url("/api/eunha/v1/invite_tree"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_invite_tree_retains_cyclic_lineage_members_once() {
    let ctx = TestContext::new("invite-tree-cycle").await;
    let alice = id(&ctx.alice_id);
    let bob = id(&ctx.bob_id);
    sqlx::query("INSERT INTO eunha.invite_lineage (account_id, inviter_account_id) VALUES ($1, $2), ($2, $1)")
        .bind(alice).bind(bob).execute(&ctx.db).await.unwrap();
    let body = tree(&ctx, &ctx.alice_token, "").await;
    assert_eq!(body["total"], 2);
    let members = body["members"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    let roots: Vec<&Value> = members
        .iter()
        .filter(|m| m["parent_id"].is_null())
        .collect();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0]["root_reason"], "lineage_unavailable");
    let child = members.iter().find(|m| !m["parent_id"].is_null()).unwrap();
    assert_eq!(child["parent_id"], roots[0]["id"]);
}

/// A long chain of invitations is read without recursing on it.
#[tokio::test]
async fn test_invite_tree_survives_a_long_chain() {
    let ctx = TestContext::new("invite-tree-chain").await;
    // A chain of 3,000 in the kept lineage, the cheapest place to make one.
    sqlx::query(
        "WITH made AS (
           INSERT INTO accounts (username, display_name, note, url, uri, public_key,
                                 inbox_url, outbox_url, shared_inbox_url, discoverable,
                                 created_at, updated_at)
           SELECT 'chain' || n, '', '', 'https://example.test/@chain' || n,
                  'https://example.test/users/chain' || n, 'test-public-key', '', '', '',
                  true, now(), now()
           FROM generate_series(1, 3000) n
           RETURNING id, username
         ), users_made AS (
           INSERT INTO users (email, account_id, created_at, updated_at, approved, confirmed_at)
           SELECT username || '@example.test', id, now() + (substr(username, 6)::int || ' ms')::interval,
                  now(), true, now()
           FROM made RETURNING account_id
         )
         INSERT INTO eunha.invite_lineage (account_id, inviter_account_id)
         SELECT m.id, p.id FROM made m JOIN made p
           ON substr(p.username, 6)::int = substr(m.username, 6)::int - 1",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let body = tree(&ctx, &ctx.alice_token, "").await;
    assert_eq!(body["total"], 3002);
    let members = body["members"].as_array().unwrap();
    assert_eq!(members.len(), 3002);
}
