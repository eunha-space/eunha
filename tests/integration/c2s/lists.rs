use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{seed_user, TestContext};

/// Full list lifecycle: create, rename, list, delete.
#[tokio::test]
async fn test_list_crud() {
    let ctx = TestContext::new("list-crud").await;

    let create_resp = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "My Friends"}),
        )
        .await;
    assert_eq!(create_resp.status(), StatusCode::OK);
    let list: Value = create_resp.json().await.unwrap();
    let list_id = list["id"].as_str().unwrap().to_string();
    assert_eq!(list["title"].as_str(), Some("My Friends"));

    // GET single list
    let get_resp = ctx
        .api
        .get(&format!("/api/v1/lists/{list_id}"), Some(&ctx.alice_token))
        .await;
    assert_eq!(get_resp.status(), StatusCode::OK);
    let got: Value = get_resp.json().await.unwrap();
    assert_eq!(got["id"].as_str(), Some(list_id.as_str()));

    // GET all lists
    let all: Vec<Value> = ctx
        .api
        .get("/api/v1/lists", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(all
        .iter()
        .any(|l| l["id"].as_str() == Some(list_id.as_str())));

    // PUT to rename
    let rename_resp = ctx
        .api
        .put_json(
            &format!("/api/v1/lists/{list_id}"),
            Some(&ctx.alice_token),
            &json!({"title": "Close Friends"}),
        )
        .await;
    assert_eq!(rename_resp.status(), StatusCode::OK);
    let renamed: Value = rename_resp.json().await.unwrap();
    assert_eq!(renamed["title"].as_str(), Some("Close Friends"));

    // DELETE
    let del_resp = ctx
        .api
        .delete(&format!("/api/v1/lists/{list_id}"), &ctx.alice_token)
        .await;
    assert_eq!(del_resp.status(), StatusCode::OK);

    let gone: Vec<Value> = ctx
        .api
        .get("/api/v1/lists", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!gone
        .iter()
        .any(|l| l["id"].as_str() == Some(list_id.as_str())));
}

/// Adding and removing accounts from a list.
#[tokio::test]
async fn test_list_add_and_remove_accounts() {
    let ctx = TestContext::new("list-accounts").await;

    // Alice must follow Bob before adding him to a list.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "Test List"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    // Add Bob
    let add_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}),
        )
        .await;
    assert_eq!(add_resp.status(), StatusCode::OK);

    let members: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(members
        .iter()
        .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())));

    // Remove Bob
    let rm_resp = ctx
        .api
        .http
        .delete(ctx.api.url(&format!("/api/v1/lists/{list_id}/accounts")))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"account_ids": [ctx.bob_id]}))
        .send()
        .await
        .unwrap();
    assert_eq!(rm_resp.status(), StatusCode::OK);

    let after: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(!after
        .iter()
        .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())));
}

/// GET /api/v1/lists/:id/accounts respects limit parameter.
#[tokio::test]
async fn test_list_accounts_limit_param() {
    let ctx = TestContext::new("list-acct-limit").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let (charlie_id, _charlie_token) = crate::helpers::seed_user(
        &ctx.db,
        &ctx.domain,
        "charlielimit",
        "charlielimit@test.invalid",
    )
    .await;
    let charlie_id = charlie_id.to_string();
    ctx.api.follow(&ctx.alice_token, &charlie_id).await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "Limit List"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id, charlie_id]}),
        )
        .await;

    let limited: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/lists/{list_id}/accounts?limit=1"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        limited.len() <= 1,
        "limit=1 should return at most 1 account, got {}",
        limited.len()
    );
}

/// GET /api/v1/lists/:id returns 404 when the list does not exist.
#[tokio::test]
async fn test_get_list_not_found() {
    let ctx = TestContext::new("list-404").await;

    let resp = ctx
        .api
        .get("/api/v1/lists/999999999", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// GET /api/v1/lists/:id returns 404 when the list belongs to another user.
#[tokio::test]
async fn test_get_list_other_user_is_404() {
    let ctx = TestContext::new("list-other-user").await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.bob_token),
            &json!({"title": "Bob's private list"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(&format!("/api/v1/lists/{list_id}"), Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// GET /api/v1/lists response includes replies_policy and exclusive fields.
#[tokio::test]
async fn test_list_response_includes_replies_policy_and_exclusive() {
    let ctx = TestContext::new("list-fields").await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "Policy List", "replies_policy": "followed", "exclusive": true}),
        )
        .await
        .json()
        .await
        .unwrap();

    assert_eq!(list["replies_policy"].as_str(), Some("followed"));
    assert_eq!(list["exclusive"].as_bool(), Some(true));

    let list_id = list["id"].as_str().unwrap();
    let fetched: Value = ctx
        .api
        .get(&format!("/api/v1/lists/{list_id}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(fetched["replies_policy"].as_str(), Some("followed"));
    assert_eq!(fetched["exclusive"].as_bool(), Some(true));
}

/// POST /api/v1/lists with empty title returns 422.
#[tokio::test]
async fn test_create_list_empty_title_returns_422() {
    let ctx = TestContext::new("list-empty-title").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": ""}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// POST /api/v1/lists with invalid replies_policy returns 422.
#[tokio::test]
async fn test_create_list_invalid_replies_policy_returns_422() {
    let ctx = TestContext::new("list-bad-policy").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "My List", "replies_policy": "whatever"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("Replies policy"),
        "error message should mention replies_policy: {body}",
    );
}

/// POST /api/v1/lists with a title over 256 characters returns 422
/// (Mastodon List::TITLE_LENGTH_LIMIT).
#[tokio::test]
async fn test_create_list_title_too_long_returns_422() {
    let ctx = TestContext::new("list-long-title").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({ "title": "x".repeat(257) }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Creating more than 50 lists returns 422 (Mastodon List::PER_ACCOUNT_LIMIT).
#[tokio::test]
async fn test_create_list_per_account_limit() {
    let ctx = TestContext::new("list-limit").await;

    for i in 0..50 {
        let resp = ctx
            .api
            .post_json(
                "/api/v1/lists",
                Some(&ctx.alice_token),
                &json!({ "title": format!("list {i}") }),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK, "list {i} should be created");
    }
    let resp = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({ "title": "one too many" }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "51st list should be rejected"
    );
}

/// PUT /api/v1/lists/:id validates the title and replies_policy too.
#[tokio::test]
async fn test_update_list_validates() {
    let ctx = TestContext::new("list-update-validate").await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({ "title": "valid" }),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    let empty = ctx
        .api
        .put_json(
            &format!("/api/v1/lists/{list_id}"),
            Some(&ctx.alice_token),
            &json!({ "title": "" }),
        )
        .await;
    assert_eq!(
        empty.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "empty title on update should be rejected"
    );

    let bad_policy = ctx
        .api
        .put_json(
            &format!("/api/v1/lists/{list_id}"),
            Some(&ctx.alice_token),
            &json!({ "title": "valid", "replies_policy": "nonsense" }),
        )
        .await;
    assert_eq!(
        bad_policy.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid replies_policy on update should be rejected"
    );
}

/// PUT /api/v1/lists/:id updates replies_policy and exclusive.
#[tokio::test]
async fn test_update_list_replies_policy_and_exclusive() {
    let ctx = TestContext::new("list-update-policy").await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "Update Policy", "replies_policy": "list", "exclusive": false}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    let updated: Value = ctx
        .api
        .put_json(
            &format!("/api/v1/lists/{list_id}"),
            Some(&ctx.alice_token),
            &json!({"title": "Update Policy", "replies_policy": "followed", "exclusive": true}),
        )
        .await
        .json()
        .await
        .unwrap();

    assert_eq!(updated["replies_policy"].as_str(), Some("followed"));
    assert_eq!(updated["exclusive"].as_bool(), Some(true));
}

/// PUT /api/v1/lists/:id returns 404 when the list does not exist.
#[tokio::test]
async fn test_update_list_not_found() {
    let ctx = TestContext::new("list-put-404").await;

    let resp = ctx
        .api
        .put_json(
            "/api/v1/lists/999999999",
            Some(&ctx.alice_token),
            &json!({"title": "Ghost List"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// PUT /api/v1/lists/:id returns 404 when the list belongs to another user.
#[tokio::test]
async fn test_update_list_other_user_is_404() {
    let ctx = TestContext::new("list-put-other").await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.bob_token),
            &json!({"title": "Bob's List"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    let resp = ctx
        .api
        .put_json(
            &format!("/api/v1/lists/{list_id}"),
            Some(&ctx.alice_token),
            &json!({"title": "Hijacked"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// DELETE /api/v1/lists/:id returns 404 when the list does not exist.
#[tokio::test]
async fn test_delete_list_not_found() {
    let ctx = TestContext::new("list-del-404").await;

    let resp = ctx
        .api
        .delete("/api/v1/lists/999999999", &ctx.alice_token)
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// DELETE /api/v1/lists/:id returns 404 when the list belongs to another user.
#[tokio::test]
async fn test_delete_list_other_user_is_404() {
    let ctx = TestContext::new("list-del-other").await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.bob_token),
            &json!({"title": "Bob's List to Delete"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    let resp = ctx
        .api
        .delete(&format!("/api/v1/lists/{list_id}"), &ctx.alice_token)
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// GET /api/v1/timelines/list/:id shows posts from list members.
#[tokio::test]
async fn test_list_timeline() {
    let ctx = TestContext::new("list-tl").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "TL List"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}),
        )
        .await;

    let status = ctx
        .api
        .post_status(&ctx.bob_token, "bob on the list", "public")
        .await;
    let status_id = status["id"].as_str().unwrap();

    let timeline: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/timelines/list/{list_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        timeline.iter().any(|s| s["id"].as_str() == Some(status_id)),
        "Bob's status should appear in the list timeline",
    );
}

/// A list member's boost of a muted account is hidden from the list timeline,
/// as the fan-out wrote it.
#[tokio::test]
async fn test_list_timeline_hides_boost_of_muted() {
    let ctx = TestContext::new("list-tl-mute").await;

    let (carol_id, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carollist", "carollist@test.invalid")
            .await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({ "title": "mute list" }),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({ "account_ids": [ctx.bob_id] }),
        )
        .await;

    // Alice mutes Carol; Carol posts; Bob (list member) boosts it.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{carol_id}/mute"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    let carol_status = ctx
        .api
        .post_status(&carol_token, "carol muted list post", "public")
        .await;
    let carol_status_id = carol_status["id"].as_str().unwrap();
    let boost: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{carol_status_id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    let boost_id = boost["id"].as_str().unwrap();

    let timeline: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/timelines/list/{list_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !timeline.iter().any(|s| s["id"].as_str() == Some(boost_id)),
        "a list member's boost of a muted account must not appear",
    );
}

/// List timeline respects max_id pagination.
#[tokio::test]
async fn test_list_timeline_max_id_pagination() {
    let ctx = TestContext::new("list-tl-maxid").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "MaxId List"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}),
        )
        .await;

    let s1 = ctx
        .api
        .post_status(&ctx.bob_token, "list maxid first", "public")
        .await;
    let s2 = ctx
        .api
        .post_status(&ctx.bob_token, "list maxid second", "public")
        .await;
    let s1_id = s1["id"].as_str().unwrap();
    let s2_id = s2["id"].as_str().unwrap();

    let paged: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/timelines/list/{list_id}?max_id={s2_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        !paged.iter().any(|s| s["id"].as_str() == Some(s2_id)),
        "max_id status should be excluded",
    );
    assert!(
        paged.iter().any(|s| s["id"].as_str() == Some(s1_id)),
        "s1 should appear when max_id=s2_id",
    );
}

/// POST /api/v1/lists/:id/accounts returns 422 when the account is not followed.
#[tokio::test]
async fn test_list_add_unfollowed_account_returns_422() {
    let ctx = TestContext::new("list-add-unfollow").await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "No Follow List"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    // Alice does NOT follow Bob. Adding Bob to a list should return 422.
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "adding unfollowed account should return 422"
    );
}

/// An account already on the list is refused (`ListAccount`'s uniqueness
/// validation), and the request it came in adds nothing, since
/// `AddAccountsToListService` adds them all in one transaction.
#[tokio::test]
async fn test_list_add_existing_member_returns_422() {
    let ctx = TestContext::new("list-add-twice").await;
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({ "title": "twice" }),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();
    let add = |ids: Value| {
        let path = format!("/api/v1/lists/{list_id}/accounts");
        let token = ctx.alice_token.clone();
        let api = &ctx.api;
        async move {
            api.post_json(&path, Some(&token), &json!({ "account_ids": ids }))
                .await
                .status()
        }
    };
    assert_eq!(add(json!([ctx.bob_id])).await, StatusCode::OK);
    assert_eq!(
        add(json!([ctx.bob_id])).await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // Herself, then Bob again: refused, and she is not left on the list.
    assert_eq!(
        add(json!([ctx.alice_id, ctx.bob_id])).await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let accts: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(accts.len(), 1, "{accts:?}");
}

/// The list owner may add themselves to their own list without following
/// themselves (Mastodon ListAccount list_owner_account_is_account?).
#[tokio::test]
async fn test_list_add_self_allowed() {
    let ctx = TestContext::new("list-add-self").await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({ "title": "me list" }),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({ "account_ids": [ctx.alice_id] }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "adding yourself to your own list should be allowed"
    );

    let accts: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        accts
            .iter()
            .any(|a| a["id"].as_str() == Some(ctx.alice_id.as_str())),
        "self should be in the list"
    );
}

/// List timeline respects since_id pagination.
#[tokio::test]
async fn test_list_timeline_since_id_pagination() {
    let ctx = TestContext::new("list-tl-since").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "SinceId List"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}),
        )
        .await;

    let s1 = ctx
        .api
        .post_status(&ctx.bob_token, "list since first", "public")
        .await;
    let s2 = ctx
        .api
        .post_status(&ctx.bob_token, "list since second", "public")
        .await;
    let s1_id = s1["id"].as_str().unwrap();
    let s2_id = s2["id"].as_str().unwrap();

    let paged: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/timelines/list/{list_id}?since_id={s1_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        !paged.iter().any(|s| s["id"].as_str() == Some(s1_id)),
        "since_id status should be excluded",
    );
    assert!(
        paged.iter().any(|s| s["id"].as_str() == Some(s2_id)),
        "s2 should appear when since_id=s1_id",
    );
}

/// Accounts in an exclusive list are excluded from home timeline.
#[tokio::test]
async fn test_exclusive_list_excludes_from_home_timeline() {
    let ctx = TestContext::new("excl-list-home").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "Exclusive", "exclusive": true}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}),
        )
        .await;

    let status = ctx
        .api
        .post_status(&ctx.bob_token, "exclusivetermXYZ", "public")
        .await;
    let status_id = status["id"].as_str().unwrap();

    // Bob's status should NOT appear on Alice's home timeline (exclusive list).
    let home: Vec<Value> = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !home.iter().any(|s| s["id"].as_str() == Some(status_id)),
        "exclusive list member's status should be excluded from home timeline",
    );

    // But it should appear on the list timeline.
    let list_tl: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/timelines/list/{list_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        list_tl.iter().any(|s| s["id"].as_str() == Some(status_id)),
        "exclusive list member's status should appear on list timeline",
    );
}

/// List timeline with replies_policy=none hides replies.
#[tokio::test]
async fn test_list_timeline_replies_policy_none() {
    let ctx = TestContext::new("list-rep-none").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "NoReplies", "replies_policy": "none"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}),
        )
        .await;

    // Normal post
    let s1 = ctx
        .api
        .post_status(&ctx.bob_token, "not a reply", "public")
        .await;
    let s1_id = s1["id"].as_str().unwrap();
    // Reply to own status
    let s2: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &serde_json::json!({"status": "a reply here", "visibility": "public", "in_reply_to_id": s1_id}),
    ).await.json().await.unwrap();
    let s2_id = s2["id"].as_str().unwrap();

    let list_tl: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/timelines/list/{list_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        list_tl.iter().any(|s| s["id"].as_str() == Some(s1_id)),
        "non-reply should appear in list with replies_policy=none",
    );
    // Self-replies (replying to your own post) always appear regardless of replies_policy,
    // matching Mastodon's behavior (filter_from_list? checks in_reply_to_account_id != account_id).
    assert!(
        list_tl.iter().any(|s| s["id"].as_str() == Some(s2_id)),
        "self-reply should still appear in list with replies_policy=none",
    );
}

/// List timeline with replies_policy=list shows replies only when replying to another list member.
#[tokio::test]
async fn test_list_timeline_replies_policy_list() {
    let ctx = TestContext::new("list-rep-list").await;

    // Create a third user (charlie) inline.
    let (charlie_id, charlie_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "charlie", "charlie@test.invalid").await;
    let charlie_id = charlie_id.to_string();

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api.follow(&ctx.alice_token, &charlie_id).await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "ListPolicy", "replies_policy": "list"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    // Add bob but not charlie to the list.
    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}),
        )
        .await;

    let charlie_post = ctx
        .api
        .post_status(&charlie_token, "charlie says hi", "public")
        .await;
    let charlie_post_id = charlie_post["id"].as_str().unwrap();

    let bob_post = ctx
        .api
        .post_status(&ctx.bob_token, "bob says hi", "public")
        .await;
    let bob_post_id = bob_post["id"].as_str().unwrap();

    // Bob replies to Charlie (not in list) — should be hidden.
    let reply_to_charlie: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &serde_json::json!({"status": "reply to charlie", "visibility": "public", "in_reply_to_id": charlie_post_id}),
    ).await.json().await.unwrap();
    let reply_to_charlie_id = reply_to_charlie["id"].as_str().unwrap();

    // Bob replies to his own post (bob is in list) — should be visible.
    let reply_to_bob: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &serde_json::json!({"status": "reply to bob", "visibility": "public", "in_reply_to_id": bob_post_id}),
    ).await.json().await.unwrap();
    let reply_to_bob_id = reply_to_bob["id"].as_str().unwrap();

    let list_tl: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/timelines/list/{list_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        !list_tl
            .iter()
            .any(|s| s["id"].as_str() == Some(reply_to_charlie_id)),
        "reply to non-list-member should be hidden with replies_policy=list",
    );
    assert!(
        list_tl
            .iter()
            .any(|s| s["id"].as_str() == Some(reply_to_bob_id)),
        "reply to list member should be visible with replies_policy=list",
    );
}

/// GET /api/v1/timelines/list/:id returns 404 for a non-existent list.
#[tokio::test]
async fn test_list_timeline_not_found() {
    let ctx = TestContext::new("list-tl-404").await;

    let resp = ctx
        .api
        .get("/api/v1/timelines/list/99999999", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// List timeline min_id returns the statuses just after the anchor, newest first.
#[tokio::test]
async fn test_list_timeline_min_id_pagination() {
    let ctx = TestContext::new("list-tl-minid").await;

    // Alice follows Bob so Bob can be added to a list.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "minid list"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}),
        )
        .await;

    let s1 = ctx
        .api
        .post_status(&ctx.bob_token, "list minid first", "public")
        .await;
    let s2 = ctx
        .api
        .post_status(&ctx.bob_token, "list minid second", "public")
        .await;
    let s3 = ctx
        .api
        .post_status(&ctx.bob_token, "list minid third", "public")
        .await;
    let s1_id = s1["id"].as_str().unwrap().to_string();
    let s2_id = s2["id"].as_str().unwrap().to_string();
    let s3_id = s3["id"].as_str().unwrap().to_string();

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/timelines/list/{list_id}?min_id={s1_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let paged: Vec<Value> = resp.json().await.unwrap();

    assert!(
        !paged
            .iter()
            .any(|s| s["id"].as_str() == Some(s1_id.as_str())),
        "min_id anchor should be excluded",
    );
    assert!(
        paged
            .iter()
            .any(|s| s["id"].as_str() == Some(s2_id.as_str())),
        "s2 should appear after min_id=s1",
    );
    assert!(
        paged
            .iter()
            .any(|s| s["id"].as_str() == Some(s3_id.as_str())),
        "s3 should appear after min_id=s1",
    );
    // A min_id page reads newest first, like any other.
    let ids: Vec<i64> = paged
        .iter()
        .filter_map(|s| s["id"].as_str()?.parse::<i64>().ok())
        .collect();
    let sorted = {
        let mut s = ids.clone();
        s.sort_by(|a, b| b.cmp(a));
        s
    };
    assert_eq!(ids, sorted, "min_id results should be newest first");
}

/// List timeline with replies_policy=followed shows replies only when the viewer follows the parent's author.
#[tokio::test]
async fn test_list_timeline_replies_policy_followed() {
    let ctx = TestContext::new("list-rep-followed").await;

    let (charlie_id, charlie_token) = crate::helpers::seed_user(
        &ctx.db,
        &ctx.domain,
        "charlie-lrf",
        "charlie-lrf@test.invalid",
    )
    .await;
    let charlie_id = charlie_id.to_string();

    // Alice follows Bob and Charlie.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api.follow(&ctx.alice_token, &charlie_id).await;

    // Alice creates a list with Bob and replies_policy=followed.
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "FollowedPolicy", "replies_policy": "followed"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}),
        )
        .await;

    // Charlie posts a status.
    let charlie_post = ctx
        .api
        .post_status(&charlie_token, "charlie original post", "public")
        .await;
    let charlie_post_id = charlie_post["id"].as_str().unwrap();

    // Bob replies to Charlie (Alice follows Charlie) → should appear.
    let reply_to_followed: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "reply to charlie who alice follows", "visibility": "public", "in_reply_to_id": charlie_post_id}),
    ).await.json().await.unwrap();
    let reply_to_followed_id = reply_to_followed["id"].as_str().unwrap();

    let list_tl: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/timelines/list/{list_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        list_tl
            .iter()
            .any(|s| s["id"].as_str() == Some(reply_to_followed_id)),
        "reply to an account alice follows should appear with replies_policy=followed",
    );
}

/// GET /api/v1/timelines/list/:id returns 404 for another user's list.
#[tokio::test]
async fn test_list_timeline_other_user_is_404() {
    let ctx = TestContext::new("list-tl-other").await;

    // Alice creates a list.
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "alice list"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    // Bob trying to access Alice's list timeline should get 404.
    let resp = ctx
        .api
        .get(
            &format!("/api/v1/timelines/list/{list_id}"),
            Some(&ctx.bob_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn extract_ids(timeline: &[Value]) -> std::collections::HashSet<String> {
    timeline
        .iter()
        .filter_map(|s| s["id"].as_str().map(str::to_owned))
        .collect()
}

/// Create a list owned by Alice with the given replies_policy, follow and add Bob as a member.
/// Returns (list_id, bob_token).
async fn setup_list_with_bob(
    ctx: &TestContext,
    label: &str,
    replies_policy: &str,
) -> (String, String) {
    let _ = label;
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "Test List", "replies_policy": replies_policy}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap().to_owned();
    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}),
        )
        .await;
    (list_id, ctx.bob_token.clone())
}

async fn list_timeline(ctx: &TestContext, list_id: &str) -> Vec<Value> {
    ctx.api
        .get(
            &format!("/api/v1/timelines/list/{list_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap()
}

// ── Translated from Mastodon feed_manager_spec.rb — push_to_list ─────────────
//
// These tests verify that the **Redis fanout path** applies replies_policy
// correctly at write time — i.e., after the list feed is already initialized.
//
// Setup: Alice owns the list, Bob is a member.  The list feed is initialized
// via a first GET before each status is posted.

/// Translated: "pushes statuses that are not replies" (all policies).
/// A plain (non-reply) status from a list member always appears.
#[tokio::test]
async fn test_list_fanout_delivers_non_reply() {
    let ctx = TestContext::new("lf-non-reply").await;
    let (list_id, _) = setup_list_with_bob(&ctx, "lf-non-reply", "none").await;

    let _ = list_timeline(&ctx, &list_id).await; // initialize Redis feed

    let s = ctx
        .api
        .post_status(&ctx.bob_token, "plain post no reply", "public")
        .await;
    let sid = s["id"].as_str().unwrap();

    let tl = list_timeline(&ctx, &list_id).await;
    assert!(
        tl.iter().any(|s| s["id"].as_str() == Some(sid)),
        "non-reply must appear via fanout"
    );
}

/// Translated: "pushes statuses that are replies to list owner" — replies_policy=none.
/// Replies to the list owner always appear regardless of policy.
#[tokio::test]
async fn test_list_fanout_none_includes_reply_to_list_owner() {
    let ctx = TestContext::new("lf-none-owner-reply").await;
    let (list_id, _) = setup_list_with_bob(&ctx, "lf-none-owner-reply", "none").await;

    // Alice (the list owner) posts a status.
    let owner_post = ctx
        .api
        .post_status(&ctx.alice_token, "alice original post", "public")
        .await;
    let owner_post_id = owner_post["id"].as_str().unwrap();

    let _ = list_timeline(&ctx, &list_id).await; // initialize Redis feed

    // Bob replies to Alice (the list owner).
    let reply: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "reply to alice", "visibility": "public", "in_reply_to_id": owner_post_id}),
    ).await.json().await.unwrap();
    let reply_id = reply["id"].as_str().unwrap();

    let tl = list_timeline(&ctx, &list_id).await;
    assert!(
        tl.iter().any(|s| s["id"].as_str() == Some(reply_id)),
        "reply to list owner must appear even with replies_policy=none",
    );
}

/// Translated: "does not push replies to another member of the list" — replies_policy=none.
#[tokio::test]
async fn test_list_fanout_none_excludes_reply_to_other_member() {
    let ctx = TestContext::new("lf-none-member-reply").await;
    let (charlie_id, charlie_token) = seed_user(
        &ctx.db,
        &ctx.domain,
        "charlie-lf-none",
        "charlie-lf-none@test.invalid",
    )
    .await;
    let charlie_id = charlie_id.to_string();

    // Alice follows both Bob and Charlie; both are list members.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api.follow(&ctx.alice_token, &charlie_id).await;
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "None Policy", "replies_policy": "none"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id, charlie_id]}),
        )
        .await;

    // Charlie posts; Bob will reply to Charlie.
    let charlie_post = ctx
        .api
        .post_status(&charlie_token, "charlie says hi", "public")
        .await;
    let charlie_post_id = charlie_post["id"].as_str().unwrap();

    let _ = list_timeline(&ctx, list_id).await; // initialize Redis feed

    // Bob replies to Charlie (another member) — should NOT appear (policy=none).
    let reply: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "bob replies to charlie", "visibility": "public", "in_reply_to_id": charlie_post_id}),
    ).await.json().await.unwrap();
    let reply_id = reply["id"].as_str().unwrap();

    let tl = list_timeline(&ctx, list_id).await;
    assert!(
        !tl.iter().any(|s| s["id"].as_str() == Some(reply_id)),
        "reply to another list member must not appear with replies_policy=none",
    );
}

/// Translated: "pushes replies to another member of the list" — replies_policy=list.
#[tokio::test]
async fn test_list_fanout_list_includes_reply_to_list_member() {
    let ctx = TestContext::new("lf-list-member-reply").await;
    let (charlie_id, charlie_token) = seed_user(
        &ctx.db,
        &ctx.domain,
        "charlie-lf-list",
        "charlie-lf-list@test.invalid",
    )
    .await;
    let charlie_id = charlie_id.to_string();

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api.follow(&ctx.alice_token, &charlie_id).await;
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "List Policy", "replies_policy": "list"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id, charlie_id]}),
        )
        .await;

    let charlie_post = ctx
        .api
        .post_status(&charlie_token, "charlie says hi list", "public")
        .await;
    let charlie_post_id = charlie_post["id"].as_str().unwrap();

    let _ = list_timeline(&ctx, list_id).await; // initialize Redis feed

    let reply: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "bob replies to charlie on list", "visibility": "public", "in_reply_to_id": charlie_post_id}),
    ).await.json().await.unwrap();
    let reply_id = reply["id"].as_str().unwrap();

    let tl = list_timeline(&ctx, list_id).await;
    assert!(
        tl.iter().any(|s| s["id"].as_str() == Some(reply_id)),
        "reply to a list member must appear with replies_policy=list",
    );
}

/// Translated: "does not push replies to someone not a member of the list" — replies_policy=list.
#[tokio::test]
async fn test_list_fanout_list_excludes_reply_to_non_member() {
    let ctx = TestContext::new("lf-list-nonmember-reply").await;
    let (eve_id, eve_token) = seed_user(
        &ctx.db,
        &ctx.domain,
        "eve-lf-list",
        "eve-lf-list@test.invalid",
    )
    .await;
    let eve_id = eve_id.to_string();

    // Alice follows Bob (member) and Eve (not a member).
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api.follow(&ctx.alice_token, &eve_id).await;
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "List Policy Eve", "replies_policy": "list"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}), // Eve NOT added
        )
        .await;

    let eve_post = ctx
        .api
        .post_status(&eve_token, "eve says hi", "public")
        .await;
    let eve_post_id = eve_post["id"].as_str().unwrap();

    let _ = list_timeline(&ctx, list_id).await; // initialize Redis feed

    let reply: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "bob replies to eve", "visibility": "public", "in_reply_to_id": eve_post_id}),
    ).await.json().await.unwrap();
    let reply_id = reply["id"].as_str().unwrap();

    let tl = list_timeline(&ctx, list_id).await;
    assert!(
        !tl.iter().any(|s| s["id"].as_str() == Some(reply_id)),
        "reply to a non-list-member must not appear with replies_policy=list",
    );
}

/// Translated: "pushes statuses that are replies to list owner" — replies_policy=list.
/// The list owner exception applies to all policies.
#[tokio::test]
async fn test_list_fanout_list_includes_reply_to_list_owner() {
    let ctx = TestContext::new("lf-list-owner-reply").await;
    let (list_id, _) = setup_list_with_bob(&ctx, "lf-list-owner-reply", "list").await;

    let owner_post = ctx
        .api
        .post_status(&ctx.alice_token, "alice original list policy", "public")
        .await;
    let owner_post_id = owner_post["id"].as_str().unwrap();

    let _ = list_timeline(&ctx, &list_id).await; // initialize Redis feed

    let reply: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "reply to alice list policy", "visibility": "public", "in_reply_to_id": owner_post_id}),
    ).await.json().await.unwrap();
    let reply_id = reply["id"].as_str().unwrap();

    let tl = list_timeline(&ctx, &list_id).await;
    assert!(
        tl.iter().any(|s| s["id"].as_str() == Some(reply_id)),
        "reply to list owner must appear with replies_policy=list",
    );
}

/// Translated: "pushes replies to someone not a member of the list" — replies_policy=followed.
/// If the list owner follows the reply target, the reply appears.
#[tokio::test]
async fn test_list_fanout_followed_includes_reply_to_followed_non_member() {
    let ctx = TestContext::new("lf-followed-nonmember").await;
    let (eve_id, eve_token) = seed_user(
        &ctx.db,
        &ctx.domain,
        "eve-lf-followed",
        "eve-lf-followed@test.invalid",
    )
    .await;
    let eve_id = eve_id.to_string();

    // Alice follows Bob (member) and Eve (followed, not a member).
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api.follow(&ctx.alice_token, &eve_id).await;
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "Followed Policy", "replies_policy": "followed"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}), // Eve NOT a member
        )
        .await;

    let eve_post = ctx
        .api
        .post_status(&eve_token, "eve says hi followed", "public")
        .await;
    let eve_post_id = eve_post["id"].as_str().unwrap();

    let _ = list_timeline(&ctx, list_id).await; // initialize Redis feed

    let reply: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "bob replies to eve followed", "visibility": "public", "in_reply_to_id": eve_post_id}),
    ).await.json().await.unwrap();
    let reply_id = reply["id"].as_str().unwrap();

    let tl = list_timeline(&ctx, list_id).await;
    assert!(
        tl.iter().any(|s| s["id"].as_str() == Some(reply_id)),
        "reply to a followed non-member must appear with replies_policy=followed",
    );
}

/// Translated: "does not push replies" when target not followed — replies_policy=followed.
#[tokio::test]
async fn test_list_fanout_followed_excludes_reply_to_non_followed() {
    let ctx = TestContext::new("lf-followed-notfollowed").await;
    let (stranger_id, stranger_token) = seed_user(
        &ctx.db,
        &ctx.domain,
        "stranger-lf",
        "stranger-lf@test.invalid",
    )
    .await;
    let stranger_id = stranger_id.to_string();

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    // Alice does NOT follow stranger.
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "Followed Policy Excl", "replies_policy": "followed"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}),
        )
        .await;

    let stranger_post = ctx
        .api
        .post_status(&stranger_token, "stranger says hi", "public")
        .await;
    let stranger_post_id = stranger_post["id"].as_str().unwrap();

    let _ = list_timeline(&ctx, list_id).await; // initialize Redis feed

    let reply: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "bob replies to stranger", "visibility": "public", "in_reply_to_id": stranger_post_id}),
    ).await.json().await.unwrap();
    let reply_id = reply["id"].as_str().unwrap();

    let tl = list_timeline(&ctx, list_id).await;
    assert!(
        !tl.iter().any(|s| s["id"].as_str() == Some(reply_id)),
        "reply to someone the list owner doesn't follow must not appear with replies_policy=followed",
    );
    let _ = stranger_id;
}

// ── Fan-out vs regeneration for list timelines ────────────────────────────────
//
// A list feed is written by the fan-out as statuses are posted, and by
// `RegenerationWorker` (`FeedManager#populate_list`) when its owner returns.
// These tests read what the fan-out wrote, then drop the feed and regenerate
// it, and check the two agree.

/// The list's timeline after its feed was dropped and Alice's feeds were
/// regenerated.
async fn regenerated_list(ctx: &TestContext, list_id: &str) -> Vec<Value> {
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let mut redis = ctx.state.redis.clone();
    eunha::feed::delete_list_feed(&mut redis, &ctx.state.redis_keys, list_id.parse().unwrap())
        .await;
    eunha::home_feed::regenerate(&ctx.state, alice).await;
    list_timeline(ctx, list_id).await
}

/// Basic parity: plain posts from the list member appear on both paths.
#[tokio::test]
async fn test_fanned_out_and_regenerated_list_timelines_agree_basic() {
    let ctx = TestContext::new("list-parity-basic").await;
    let (list_id, _) = setup_list_with_bob(&ctx, "list-parity-basic", "list").await;

    ctx.api
        .post_status(&ctx.bob_token, "list parity 1", "public")
        .await;
    ctx.api
        .post_status(&ctx.bob_token, "list parity 2", "public")
        .await;

    let fanned_tl = list_timeline(&ctx, &list_id).await;
    let fanned_ids = extract_ids(&fanned_tl);
    assert!(!fanned_ids.is_empty(), "the fan-out delivered the posts");

    let regenerated_tl = regenerated_list(&ctx, &list_id).await;
    let regenerated_ids = extract_ids(&regenerated_tl);

    assert_eq!(
        fanned_ids, regenerated_ids,
        "fanned-out and regenerated list timelines must agree on basic posts"
    );
}

/// Parity with replies_policy=none: only non-replies (and replies to list owner) appear on both paths.
#[tokio::test]
async fn test_fanned_out_and_regenerated_list_timelines_agree_replies_policy_none() {
    let ctx = TestContext::new("list-parity-none").await;
    let (charlie_id, charlie_token) = seed_user(
        &ctx.db,
        &ctx.domain,
        "charlie-parity-none",
        "charlie-parity-none@test.invalid",
    )
    .await;
    let charlie_id = charlie_id.to_string();

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api.follow(&ctx.alice_token, &charlie_id).await;
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "Parity None", "replies_policy": "none"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id, charlie_id]}),
        )
        .await;

    // Non-reply from Bob (should appear).
    ctx.api
        .post_status(&ctx.bob_token, "parity none non-reply", "public")
        .await;

    // Alice posts, Bob replies to her (list owner reply exception — should appear).
    let alice_post = ctx
        .api
        .post_status(&ctx.alice_token, "alice original parity none", "public")
        .await;
    let alice_post_id = alice_post["id"].as_str().unwrap();
    ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "bob replies to alice", "visibility": "public", "in_reply_to_id": alice_post_id}),
    ).await;

    // Charlie posts, Bob replies to Charlie (should NOT appear with none policy).
    let charlie_post = ctx
        .api
        .post_status(&charlie_token, "charlie original parity none", "public")
        .await;
    let charlie_post_id = charlie_post["id"].as_str().unwrap();
    ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "bob replies to charlie parity", "visibility": "public", "in_reply_to_id": charlie_post_id}),
    ).await;

    // What the fan-out wrote.
    let fanned_tl = list_timeline(&ctx, list_id).await;
    let fanned_ids = extract_ids(&fanned_tl);

    // What a regeneration writes.
    let regenerated_tl = regenerated_list(&ctx, list_id).await;
    let regenerated_ids = extract_ids(&regenerated_tl);

    assert_eq!(
        fanned_ids, regenerated_ids,
        "fanned-out and regenerated list timelines must agree with replies_policy=none",
    );
}

/// Parity with replies_policy=list: replies to list members appear on both paths;
/// replies to non-members are absent from both.
#[tokio::test]
async fn test_fanned_out_and_regenerated_list_timelines_agree_replies_policy_list() {
    let ctx = TestContext::new("list-parity-list").await;
    let (charlie_id, charlie_token) = seed_user(
        &ctx.db,
        &ctx.domain,
        "charlie-parity-list",
        "charlie-parity-list@test.invalid",
    )
    .await;
    let charlie_id = charlie_id.to_string();
    let (eve_id, eve_token) = seed_user(
        &ctx.db,
        &ctx.domain,
        "eve-parity-list",
        "eve-parity-list@test.invalid",
    )
    .await;
    let eve_id = eve_id.to_string();

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api.follow(&ctx.alice_token, &charlie_id).await;
    ctx.api.follow(&ctx.alice_token, &eve_id).await;
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "Parity List", "replies_policy": "list"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();
    // Bob and Charlie in list; Eve is NOT a list member.
    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id, charlie_id]}),
        )
        .await;

    ctx.api
        .post_status(&ctx.bob_token, "parity list non-reply", "public")
        .await;
    let charlie_post = ctx
        .api
        .post_status(&charlie_token, "charlie list parity", "public")
        .await;
    let charlie_post_id = charlie_post["id"].as_str().unwrap();
    ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "bob replies to charlie list parity", "visibility": "public", "in_reply_to_id": charlie_post_id}),
    ).await;
    let eve_post = ctx
        .api
        .post_status(&eve_token, "eve parity list", "public")
        .await;
    let eve_post_id = eve_post["id"].as_str().unwrap();
    ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "bob replies to eve list parity", "visibility": "public", "in_reply_to_id": eve_post_id}),
    ).await;

    let fanned_tl = list_timeline(&ctx, list_id).await;
    let fanned_ids = extract_ids(&fanned_tl);

    let regenerated_tl = regenerated_list(&ctx, list_id).await;
    let regenerated_ids = extract_ids(&regenerated_tl);

    assert_eq!(
        fanned_ids, regenerated_ids,
        "fanned-out and regenerated list timelines must agree with replies_policy=list",
    );
}

/// Parity with replies_policy=followed: replies to followed accounts appear on both paths;
/// replies to non-followed accounts are absent from both.
#[tokio::test]
async fn test_fanned_out_and_regenerated_list_timelines_agree_replies_policy_followed() {
    let ctx = TestContext::new("list-parity-followed").await;
    let (eve_id, eve_token) = seed_user(
        &ctx.db,
        &ctx.domain,
        "eve-parity-followed",
        "eve-parity-followed@test.invalid",
    )
    .await;
    let eve_id = eve_id.to_string();
    let (stranger_id, stranger_token) = seed_user(
        &ctx.db,
        &ctx.domain,
        "stranger-parity",
        "stranger-parity@test.invalid",
    )
    .await;
    let stranger_id = stranger_id.to_string();

    // Alice follows Bob and Eve; does NOT follow stranger.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api.follow(&ctx.alice_token, &eve_id).await;
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "Parity Followed", "replies_policy": "followed"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}),
        )
        .await;

    ctx.api
        .post_status(&ctx.bob_token, "parity followed non-reply", "public")
        .await;
    let eve_post = ctx
        .api
        .post_status(&eve_token, "eve parity followed", "public")
        .await;
    let eve_post_id = eve_post["id"].as_str().unwrap();
    ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "bob replies to eve parity", "visibility": "public", "in_reply_to_id": eve_post_id}),
    ).await;
    let stranger_post = ctx
        .api
        .post_status(&stranger_token, "stranger parity", "public")
        .await;
    let stranger_post_id = stranger_post["id"].as_str().unwrap();
    ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "bob replies to stranger parity", "visibility": "public", "in_reply_to_id": stranger_post_id}),
    ).await;

    let fanned_tl = list_timeline(&ctx, list_id).await;
    let fanned_ids = extract_ids(&fanned_tl);

    let regenerated_tl = regenerated_list(&ctx, list_id).await;
    let regenerated_ids = extract_ids(&regenerated_tl);

    assert_eq!(
        fanned_ids, regenerated_ids,
        "fanned-out and regenerated list timelines must agree with replies_policy=followed",
    );
    let _ = (stranger_id, eve_id, stranger_post_id);
}

// ── List timeline mute filtering ──────────────────────────────────────────────

/// The list timeline hides statuses from muted accounts.
#[tokio::test]
async fn test_list_timeline_hides_muted_accounts() {
    let ctx = TestContext::new("list-tl-muted").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "Mute Test List"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}),
        )
        .await;

    let status = ctx
        .api
        .post_status(&ctx.bob_token, "bob list mute post", "public")
        .await;
    let status_id = status["id"].as_str().unwrap();

    // Visible before the mute.
    let before = list_timeline(&ctx, list_id).await;
    assert!(
        before.iter().any(|s| s["id"].as_str() == Some(status_id)),
        "bob's status should appear in list timeline before mute",
    );

    // Alice mutes Bob.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/mute", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // Hidden once muted.
    let after = list_timeline(&ctx, list_id).await;
    assert!(
        !after.iter().any(|s| s["id"].as_str() == Some(status_id)),
        "muted account's status should be hidden from list timeline",
    );
}

/// The fan-out and a regeneration agree when a list member is muted.
#[tokio::test]
async fn test_fanned_out_and_regenerated_list_timelines_agree_with_muted_member() {
    let ctx = TestContext::new("list-parity-mute").await;
    let (carol_id, carol_token) = seed_user(
        &ctx.db,
        &ctx.domain,
        "carol-list-mute",
        "carol-list-mute@test.invalid",
    )
    .await;
    let carol_id = carol_id.to_string();

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{carol_id}/follow"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "Parity Mute List"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id, carol_id]}),
        )
        .await;

    // Alice mutes Carol.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{carol_id}/mute"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    ctx.api
        .post_status(&ctx.bob_token, "bob list parity mute", "public")
        .await;
    ctx.api
        .post_status(&carol_token, "carol list parity muted", "public")
        .await;

    // What the fan-out wrote.
    let fanned_tl = list_timeline(&ctx, list_id).await;
    let fanned_ids = extract_ids(&fanned_tl);

    // What a regeneration writes.
    let regenerated_tl = regenerated_list(&ctx, list_id).await;
    let regenerated_ids = extract_ids(&regenerated_tl);

    assert_eq!(
        fanned_ids, regenerated_ids,
        "fanned-out and regenerated list timelines must agree when a muted member is present",
    );
    // Carol's status must be absent from both paths.
    assert!(
        fanned_ids.iter().all(|id| {
            fanned_tl
                .iter()
                .find(|s| s["id"].as_str() == Some(id.as_str()))
                .map(|s| s["account"]["id"].as_str() != Some(carol_id.as_str()))
                .unwrap_or(true)
        }),
        "muted carol's status should be absent from list timeline",
    );
}

/// `replies_policy` is stored as Mastodon's enum: list 0, followed 1, none 2.
#[tokio::test]
async fn test_replies_policy_stored_as_mastodon_enum() {
    let ctx = TestContext::new("list-policy-enum").await;
    for (policy, stored) in [("list", 0), ("followed", 1), ("none", 2)] {
        let list: Value = ctx
            .api
            .post_json(
                "/api/v1/lists",
                Some(&ctx.alice_token),
                &json!({"title": policy, "replies_policy": policy}),
            )
            .await
            .json()
            .await
            .unwrap();
        let value: i32 = sqlx::query_scalar("SELECT replies_policy FROM lists WHERE id = $1")
            .bind(list["id"].as_str().unwrap().parse::<i64>().unwrap())
            .fetch_one(&ctx.db)
            .await
            .unwrap();
        assert_eq!(value, stored, "{policy}");
    }
}
