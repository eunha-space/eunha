use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

/// Full filter v1 lifecycle: create → get → list → update → delete.
#[tokio::test]
async fn test_filter_v1_crud() {
    let ctx = TestContext::new("filter-v1").await;

    let create_resp = ctx
        .api
        .post_json(
            "/api/v1/filters",
            Some(&ctx.alice_token),
            &json!({
                "phrase": "badword",
                "context": ["home", "notifications"],
                "irreversible": false,
                "whole_word": true
            }),
        )
        .await;
    assert_eq!(create_resp.status(), StatusCode::OK);
    let filter: Value = create_resp.json().await.unwrap();
    let filter_id = filter["id"].as_str().unwrap().to_string();
    assert_eq!(filter["phrase"].as_str(), Some("badword"));
    assert!(filter["context"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c == "home"));

    let get_resp = ctx
        .api
        .get(
            &format!("/api/v1/filters/{filter_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(get_resp.status(), StatusCode::OK);
    let f: Value = get_resp.json().await.unwrap();
    assert_eq!(f["phrase"].as_str(), Some("badword"));

    let list: Vec<Value> = ctx
        .api
        .get("/api/v1/filters", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(list
        .iter()
        .any(|f| f["id"].as_str() == Some(filter_id.as_str())));

    let update_resp = ctx
        .api
        .put_json(
            &format!("/api/v1/filters/{filter_id}"),
            Some(&ctx.alice_token),
            &json!({
                "phrase": "badword2",
                "context": ["home"],
            }),
        )
        .await;
    assert_eq!(update_resp.status(), StatusCode::OK);
    let updated: Value = update_resp.json().await.unwrap();
    assert_eq!(updated["phrase"].as_str(), Some("badword2"));

    let del_resp = ctx
        .api
        .delete(&format!("/api/v1/filters/{filter_id}"), &ctx.alice_token)
        .await;
    assert_eq!(del_resp.status(), StatusCode::OK);

    let gone_resp = ctx
        .api
        .get(
            &format!("/api/v1/filters/{filter_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(gone_resp.status(), StatusCode::NOT_FOUND);
}

/// Full filter v2 lifecycle including keyword management.
#[tokio::test]
async fn test_filter_v2_crud() {
    let ctx = TestContext::new("filter-v2").await;

    let create_resp = ctx
        .api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.alice_token),
            &json!({
                "title": "Spam Filter",
                "context": ["home", "public"],
                "filter_action": "warn",
                "keywords_attributes": [
                    {"keyword": "spam", "whole_word": false}
                ]
            }),
        )
        .await;
    assert_eq!(create_resp.status(), StatusCode::OK);
    let filter: Value = create_resp.json().await.unwrap();
    let filter_id = filter["id"].as_str().unwrap().to_string();
    assert_eq!(filter["title"].as_str(), Some("Spam Filter"));
    assert!(filter["keywords"]
        .as_array()
        .unwrap()
        .iter()
        .any(|k| k["keyword"] == "spam"));

    let get_resp = ctx
        .api
        .get(
            &format!("/api/v2/filters/{filter_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(get_resp.status(), StatusCode::OK);

    let list: Vec<Value> = ctx
        .api
        .get("/api/v2/filters", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(list
        .iter()
        .any(|f| f["id"].as_str() == Some(filter_id.as_str())));

    let update_resp = ctx
        .api
        .put_json(
            &format!("/api/v2/filters/{filter_id}"),
            Some(&ctx.alice_token),
            &json!({
                "title": "Updated Filter",
                "context": ["home"],
                "filter_action": "hide"
            }),
        )
        .await;
    assert_eq!(update_resp.status(), StatusCode::OK);
    let updated: Value = update_resp.json().await.unwrap();
    assert_eq!(updated["title"].as_str(), Some("Updated Filter"));
    assert_eq!(updated["filter_action"].as_str(), Some("hide"));

    // Add keyword
    let add_kw_resp = ctx
        .api
        .post_json(
            &format!("/api/v2/filters/{filter_id}/keywords"),
            Some(&ctx.alice_token),
            &json!({"keyword": "junk", "whole_word": true}),
        )
        .await;
    assert_eq!(add_kw_resp.status(), StatusCode::OK);
    let kw: Value = add_kw_resp.json().await.unwrap();
    let kw_id = kw["id"].as_str().unwrap().to_string();
    assert_eq!(kw["keyword"].as_str(), Some("junk"));

    // List keywords
    let kws: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v2/filters/{filter_id}/keywords"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(kws.iter().any(|k| k["keyword"] == "junk"));

    // `namespace :filters` routes a keyword at `/api/v2/filters/keywords/:id`;
    // there is no `/api/v2/filter_keywords`.
    let old_path = ctx
        .api
        .get(
            &format!("/api/v2/filter_keywords/{kw_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(old_path.status(), StatusCode::NOT_FOUND);

    // Get single keyword
    let kw_resp = ctx
        .api
        .get(
            &format!("/api/v2/filters/keywords/{kw_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(kw_resp.status(), StatusCode::OK);

    // Update keyword
    let upd_kw: Value = ctx
        .api
        .put_json(
            &format!("/api/v2/filters/keywords/{kw_id}"),
            Some(&ctx.alice_token),
            &json!({"keyword": "garbage", "whole_word": false}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(upd_kw["keyword"].as_str(), Some("garbage"));

    // Delete keyword
    let del_kw_resp = ctx
        .api
        .delete(
            &format!("/api/v2/filters/keywords/{kw_id}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(del_kw_resp.status(), StatusCode::OK);

    // Delete filter
    let del_resp = ctx
        .api
        .delete(&format!("/api/v2/filters/{filter_id}"), &ctx.alice_token)
        .await;
    assert_eq!(del_resp.status(), StatusCode::OK);
}

// ── filter statuses (v2) ──────────────────────────────────────────────────────

/// Full filter_statuses lifecycle: create filter → add status → list → get → delete.
#[tokio::test]
async fn test_filter_statuses_crud() {
    let ctx = TestContext::new("filter-statuses").await;

    // Create a filter.
    let filter: Value = ctx
        .api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.alice_token),
            &json!({
                "title": "Status filter",
                "context": ["home"],
                "filter_action": "hide"
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let filter_id = filter["id"].as_str().unwrap();

    // Post a status to attach.
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "filtered status", "public")
        .await;
    let status_id = status["id"].as_str().unwrap();

    // Add status to filter.
    let add_resp = ctx
        .api
        .post_json(
            &format!("/api/v2/filters/{filter_id}/statuses"),
            Some(&ctx.alice_token),
            &json!({"status_id": status_id}),
        )
        .await;
    assert_eq!(add_resp.status(), StatusCode::OK);
    let fs: Value = add_resp.json().await.unwrap();
    let fs_id = fs["id"].as_str().unwrap().to_string();
    assert_eq!(fs["status_id"].as_str(), Some(status_id));

    // List filter statuses.
    let list: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v2/filters/{filter_id}/statuses"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(list
        .iter()
        .any(|s| s["status_id"].as_str() == Some(status_id)));

    // Get individual filter status.
    let get_resp = ctx
        .api
        .get(
            &format!("/api/v2/filters/statuses/{fs_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(get_resp.status(), StatusCode::OK);
    let fs2: Value = get_resp.json().await.unwrap();
    assert_eq!(fs2["status_id"].as_str(), Some(status_id));
    let old_path = ctx
        .api
        .get(
            &format!("/api/v2/filter_statuses/{fs_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(old_path.status(), StatusCode::NOT_FOUND);

    // Delete filter status.
    let del_resp = ctx
        .api
        .delete(
            &format!("/api/v2/filters/statuses/{fs_id}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(del_resp.status(), StatusCode::OK);

    // Verify deleted.
    let after: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v2/filters/{filter_id}/statuses"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(!after
        .iter()
        .any(|s| s["status_id"].as_str() == Some(status_id)));
}

/// GET /api/v2/filters/:id returns 404 for a filter belonging to another user.
#[tokio::test]
async fn test_get_filter_v2_other_user_is_404() {
    let ctx = TestContext::new("filter-v2-other").await;

    // Alice creates a filter.
    let filter: Value = ctx
        .api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.alice_token),
            &json!({
                "title": "Alice's filter",
                "context": ["home"],
                "filter_action": "warn"
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let filter_id = filter["id"].as_str().unwrap();

    // Bob tries to access it.
    let resp = ctx
        .api
        .get(
            &format!("/api/v2/filters/{filter_id}"),
            Some(&ctx.bob_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// GET /api/v1/filters/:id returns 404 for a filter belonging to another user.
#[tokio::test]
async fn test_get_filter_v1_other_user_is_404() {
    let ctx = TestContext::new("filter-v1-other").await;

    let filter: Value = ctx
        .api
        .post_json(
            "/api/v1/filters",
            Some(&ctx.alice_token),
            &json!({
                "phrase": "alice_private_word",
                "context": ["home"]
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let filter_id = filter["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/filters/{filter_id}"),
            Some(&ctx.bob_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// GET /api/v2/filters/statuses/:id for unknown id returns 404.
#[tokio::test]
async fn test_get_filter_status_not_found() {
    let ctx = TestContext::new("filter-status-404").await;

    let resp = ctx
        .api
        .get("/api/v2/filters/statuses/99999999", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── filter validation ─────────────────────────────────────────────────────────

/// Creating a v1 filter with no phrase (required string field) returns 422.
#[tokio::test]
async fn test_filter_v1_missing_phrase_returns_422() {
    let ctx = TestContext::new("filter-v1-nophrase").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/filters",
            Some(&ctx.alice_token),
            &json!({
                "context": ["home"]
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Creating a v2 filter with no title (required string field) returns 422.
#[tokio::test]
async fn test_filter_v2_missing_title_returns_422() {
    let ctx = TestContext::new("filter-v2-notitle").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.alice_token),
            &json!({
                "context": ["home"],
                "filter_action": "warn"
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Filter validation parity with Mastodon's CustomFilter model: invalid or
/// empty context, unknown action, and over-long titles are all rejected.
#[tokio::test]
async fn test_filter_v2_validation() {
    let ctx = TestContext::new("filter-v2-validate").await;

    let post = |body: Value| {
        let ctx = &ctx;
        async move {
            ctx.api
                .post_json("/api/v2/filters", Some(&ctx.alice_token), &body)
                .await
                .status()
        }
    };

    assert_eq!(
        post(json!({ "title": "x", "context": ["nonsense"] })).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid context must be rejected",
    );
    assert_eq!(
        post(json!({ "title": "x", "context": [] })).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "empty context must be rejected",
    );
    assert_eq!(
        post(json!({ "title": "x", "context": ["home"], "filter_action": "explode" })).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid filter_action must be rejected",
    );
    assert_eq!(
        post(json!({ "title": "x".repeat(257), "context": ["home"] })).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "over-long title must be rejected",
    );
    assert_eq!(
        post(json!({ "title": "ok", "context": ["home", "public"] })).await,
        StatusCode::OK,
        "a valid filter should be accepted",
    );
}

/// The `blur` filter action is accepted and round-trips (Mastodon supports
/// warn/hide/blur).
#[tokio::test]
async fn test_filter_v2_blur_action() {
    let ctx = TestContext::new("filter-v2-blur").await;

    let filter: Value = ctx
        .api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.alice_token),
            &json!({ "title": "blurry", "context": ["home"], "filter_action": "blur" }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        filter["filter_action"].as_str(),
        Some("blur"),
        "blur action should round-trip"
    );
}

// ── filter expiry ──────────────────────────────────────────────────────────────

/// Creating a filter with expires_in sets the expires_at field.
#[tokio::test]
async fn test_filter_v2_with_expires_in() {
    let ctx = TestContext::new("filter-expires").await;

    let create_resp = ctx
        .api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.alice_token),
            &json!({
                "title": "Expiring Filter",
                "context": ["home"],
                "filter_action": "warn",
                "expires_in": 3600
            }),
        )
        .await;
    assert_eq!(create_resp.status(), StatusCode::OK);
    let filter: Value = create_resp.json().await.unwrap();
    assert!(
        filter["expires_at"].as_str().is_some(),
        "expires_at should be set when expires_in provided"
    );
}

/// Creating a v1 filter with expires_in sets the expires_at field.
#[tokio::test]
async fn test_filter_v1_with_expires_in() {
    let ctx = TestContext::new("filter-v1-expires").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/filters",
            Some(&ctx.alice_token),
            &json!({
                "phrase": "expiringword",
                "context": ["home"],
                "expires_in": 7200
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let filter: Value = resp.json().await.unwrap();
    assert!(
        filter["expires_at"].as_str().is_some(),
        "expires_at should be set when expires_in provided"
    );
}

// ── v1 filter whole_word reflects database value ───────────────────────────────

/// The v1 filter whole_word field reads from the keyword row, not hardcoded.
#[tokio::test]
async fn test_filter_v1_whole_word_reads_from_db() {
    let ctx = TestContext::new("filter-ww").await;

    // Create a v2 filter with whole_word=false via the v2 API.
    let create_resp = ctx
        .api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.alice_token),
            &json!({
                "title": "whole_word test",
                "context": ["home"],
                "filter_action": "warn",
                "keywords_attributes": [{"keyword": "badword", "whole_word": false}]
            }),
        )
        .await;
    assert_eq!(create_resp.status(), StatusCode::OK);
    let filter: Value = create_resp.json().await.unwrap();
    let filter_id = filter["id"].as_str().unwrap();

    // Retrieve via v1 and check that whole_word is false (not hardcoded true).
    let get_resp = ctx
        .api
        .get(
            &format!("/api/v1/filters/{filter_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(get_resp.status(), StatusCode::OK);
    let v1_filter: Value = get_resp.json().await.unwrap();
    assert_eq!(
        v1_filter["whole_word"].as_bool(),
        Some(false),
        "whole_word should be false as stored, not hardcoded to true"
    );

    // Create another filter with whole_word=true and verify it reads back correctly.
    let create2 = ctx
        .api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.alice_token),
            &json!({
                "title": "whole_word true test",
                "context": ["home"],
                "filter_action": "warn",
                "keywords_attributes": [{"keyword": "strictword", "whole_word": true}]
            }),
        )
        .await;
    let filter2: Value = create2.json().await.unwrap();
    let filter2_id = filter2["id"].as_str().unwrap();

    let get2 = ctx
        .api
        .get(
            &format!("/api/v1/filters/{filter2_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    let v1_filter2: Value = get2.json().await.unwrap();
    assert_eq!(
        v1_filter2["whole_word"].as_bool(),
        Some(true),
        "whole_word should be true as stored"
    );
}

/// A keyword made without `whole_word` takes the column's default, `true`,
/// and an update that leaves it out leaves it as it was — `update!` with
/// only the attributes given.
#[tokio::test]
async fn test_whole_word_defaults_and_is_kept() {
    let ctx = TestContext::new("filter-ww-default").await;
    let token = Some(ctx.alice_token.as_str());

    let v1: Value = ctx
        .api
        .post_json(
            "/api/v1/filters",
            token,
            &json!({"phrase": "oldstyle", "context": ["home"]}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v1["whole_word"], json!(true), "{v1}");
    let id = v1["id"].as_str().unwrap();
    let updated: Value = ctx
        .api
        .put_json(
            &format!("/api/v1/filters/{id}"),
            token,
            &json!({"phrase": "oldstyle2", "context": ["home"], "whole_word": false}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(updated["whole_word"], json!(false), "{updated}");
    let kept: Value = ctx
        .api
        .put_json(
            &format!("/api/v1/filters/{id}"),
            token,
            &json!({"phrase": "oldstyle3", "context": ["home"]}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(kept["whole_word"], json!(false), "{kept}");

    let v2: Value = ctx
        .api
        .post_json(
            "/api/v2/filters",
            token,
            &json!({"title": "t", "context": ["home"],
                    "keywords_attributes": [{"keyword": "word"}]}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v2["keywords"][0]["whole_word"], json!(true), "{v2}");
    let keyword = v2["keywords"][0]["id"].as_str().unwrap();
    for whole_word in [Some(false), None] {
        let mut body = json!({"keyword": "word2"});
        if let Some(w) = whole_word {
            body["whole_word"] = json!(w);
        }
        let kw: Value = ctx
            .api
            .put_json(&format!("/api/v2/filters/keywords/{keyword}"), token, &body)
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(kw["whole_word"], json!(false), "{kw}");
    }
}

/// A single status carries the viewer's matching filters in `filtered`
/// (`status_matches_filters`), whatever context those filters apply in.
#[tokio::test]
async fn test_single_status_carries_matching_filters() {
    let ctx = TestContext::new("filter-show").await;
    let filter: Value = ctx
        .api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.alice_token),
            &json!({"title": "words", "context": ["notifications"],
                    "keywords_attributes": [{"keyword": "parityword"}]}),
        )
        .await
        .json()
        .await
        .unwrap();
    let status = ctx
        .api
        .post_status(&ctx.bob_token, "a parityword here", "public")
        .await;
    let id = status["id"].as_str().unwrap();
    let seen: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(seen["filtered"][0]["filter"]["id"], filter["id"], "{seen}");
    assert_eq!(
        seen["filtered"][0]["keyword_matches"],
        json!(["parityword"]),
        "{seen}"
    );

    let other = ctx
        .api
        .post_status(&ctx.bob_token, "nothing to see", "public")
        .await;
    let other_id = other["id"].as_str().unwrap();
    let seen: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{other_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(seen["filtered"], json!([]), "{seen}");
}
