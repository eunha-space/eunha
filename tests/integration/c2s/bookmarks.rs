use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::TestContext;

/// Parse the `max_id` value from the `rel="next"` Link header entry.
fn link_next_max_id(link: &str) -> Option<String> {
    // Format: <url?max_id=N>; rel="next"
    for part in link.split(',') {
        if part.contains(r#"rel="next""#) {
            if let Some(url_part) = part.split(';').next() {
                let url = url_part
                    .trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>');
                for param in url.split('?').nth(1).unwrap_or("").split('&') {
                    if let Some(val) = param.strip_prefix("max_id=") {
                        return Some(val.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Parse the `min_id` value from the `rel="prev"` Link header entry.
fn link_prev_min_id(link: &str) -> Option<String> {
    for part in link.split(',') {
        if part.contains(r#"rel="prev""#) {
            if let Some(url_part) = part.split(';').next() {
                let url = url_part
                    .trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>');
                for param in url.split('?').nth(1).unwrap_or("").split('&') {
                    if let Some(val) = param.strip_prefix("min_id=") {
                        return Some(val.to_string());
                    }
                }
            }
        }
    }
    None
}

/// GET /api/v1/bookmarks returns statuses the user has bookmarked.
#[tokio::test]
async fn test_bookmarks_returns_bookmarked_statuses() {
    let ctx = TestContext::new("bmarks-list").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "bookmark listing test", "public")
        .await;
    let status_id = status["id"].as_str().unwrap().to_string();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{status_id}/bookmark"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;

    let resp = ctx.api.get("/api/v1/bookmarks", Some(&ctx.bob_token)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Vec<Value> = resp.json().await.unwrap();
    let ids: Vec<&str> = body.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        ids.contains(&status_id.as_str()),
        "bookmarked status missing from /api/v1/bookmarks"
    );
}

/// The bookmarked field on the returned status objects is true.
#[tokio::test]
async fn test_bookmarks_status_has_bookmarked_true() {
    let ctx = TestContext::new("bmarks-field").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "bookmark field test", "public")
        .await;
    let status_id = status["id"].as_str().unwrap().to_string();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{status_id}/bookmark"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;

    let resp = ctx.api.get("/api/v1/bookmarks", Some(&ctx.bob_token)).await;
    let body: Vec<Value> = resp.json().await.unwrap();
    let entry = body
        .iter()
        .find(|s| s["id"].as_str() == Some(&status_id))
        .unwrap();
    assert_eq!(entry["bookmarked"], true, "bookmarked field should be true");
}

/// GET /api/v1/bookmarks is empty when nothing has been bookmarked.
#[tokio::test]
async fn test_bookmarks_empty_when_none() {
    let ctx = TestContext::new("bmarks-empty").await;

    let resp = ctx
        .api
        .get("/api/v1/bookmarks", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Vec<Value> = resp.json().await.unwrap();
    assert!(body.is_empty());
}

/// GET /api/v1/bookmarks requires authentication.
#[tokio::test]
async fn test_bookmarks_requires_auth() {
    let ctx = TestContext::new("bmarks-unauth").await;

    let resp = ctx.api.get("/api/v1/bookmarks", None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Unbookmarking a status removes it from GET /api/v1/bookmarks.
#[tokio::test]
async fn test_bookmarks_unbookmark_removes_from_list() {
    let ctx = TestContext::new("bmarks-remove").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "unbookmark listing test", "public")
        .await;
    let status_id = status["id"].as_str().unwrap().to_string();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{status_id}/bookmark"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{status_id}/unbookmark"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;

    let resp = ctx.api.get("/api/v1/bookmarks", Some(&ctx.bob_token)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Vec<Value> = resp.json().await.unwrap();
    let ids: Vec<&str> = body.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        !ids.contains(&status_id.as_str()),
        "unbookmarked status still in bookmarks list"
    );
}

/// max_id pagination excludes bookmarks at or newer than the given sort_id cursor.
#[tokio::test]
async fn test_bookmarks_max_id_pagination() {
    let ctx = TestContext::new("bmarks-maxid").await;

    let s1 = ctx
        .api
        .post_status(&ctx.alice_token, "bmark maxid first", "public")
        .await;
    let s2 = ctx
        .api
        .post_status(&ctx.alice_token, "bmark maxid second", "public")
        .await;
    let s1_id = s1["id"].as_str().unwrap().to_string();
    let s2_id = s2["id"].as_str().unwrap().to_string();

    // Bookmark s1 then s2; s2 gets the higher sort_id (bookmarked last).
    for id in [&s1_id, &s2_id] {
        ctx.api
            .post_json(
                &format!("/api/v1/statuses/{id}/bookmark"),
                Some(&ctx.bob_token),
                &serde_json::json!({}),
            )
            .await;
    }

    // Fetch just the newest bookmark (s2) so we can get its sort_id cursor.
    let first_resp = ctx
        .api
        .get("/api/v1/bookmarks?limit=1", Some(&ctx.bob_token))
        .await;
    let link_header = first_resp
        .headers()
        .get("link")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // The `next` link carries max_id=<s2's sort_id>.
    let cursor = link_next_max_id(&link_header)
        .expect("Link header with next cursor expected when two bookmarks exist");

    let paged: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/bookmarks?max_id={cursor}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        !paged
            .iter()
            .any(|s| s["id"].as_str() == Some(s2_id.as_str())),
        "max_id bookmark should be excluded",
    );
    assert!(
        paged
            .iter()
            .any(|s| s["id"].as_str() == Some(s1_id.as_str())),
        "s1 should appear when max_id=s2's sort_id",
    );
}

/// since_id pagination returns only bookmarks newer than the given sort_id cursor.
#[tokio::test]
async fn test_bookmarks_since_id_pagination() {
    let ctx = TestContext::new("bmarks-since").await;

    let s1 = ctx
        .api
        .post_status(&ctx.alice_token, "bmark since first", "public")
        .await;
    let s2 = ctx
        .api
        .post_status(&ctx.alice_token, "bmark since second", "public")
        .await;
    let s1_id = s1["id"].as_str().unwrap().to_string();
    let s2_id = s2["id"].as_str().unwrap().to_string();

    // Bookmark s1 then s2; s1 gets the lower sort_id.
    for id in [&s1_id, &s2_id] {
        ctx.api
            .post_json(
                &format!("/api/v1/statuses/{id}/bookmark"),
                Some(&ctx.bob_token),
                &serde_json::json!({}),
            )
            .await;
    }

    // Fetch all; the `next` link carries max_id=<s1's sort_id> (oldest in page).
    let all_resp = ctx.api.get("/api/v1/bookmarks", Some(&ctx.bob_token)).await;
    let link_header = all_resp
        .headers()
        .get("link")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let s1_sort_id = link_next_max_id(&link_header).expect("Link header with next cursor expected");

    let paged: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/bookmarks?since_id={s1_sort_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        !paged
            .iter()
            .any(|s| s["id"].as_str() == Some(s1_id.as_str())),
        "since_id bookmark should be excluded",
    );
    assert!(
        paged
            .iter()
            .any(|s| s["id"].as_str() == Some(s2_id.as_str())),
        "s2 should appear when since_id=s1's sort_id",
    );
}

/// The limit parameter caps the number of results returned.
#[tokio::test]
async fn test_bookmarks_limit_param() {
    let ctx = TestContext::new("bmarks-limit").await;

    for i in 0..3 {
        let s = ctx
            .api
            .post_status(&ctx.alice_token, &format!("bookmark limit {i}"), "public")
            .await;
        let sid = s["id"].as_str().unwrap().to_string();
        ctx.api
            .post_json(
                &format!("/api/v1/statuses/{sid}/bookmark"),
                Some(&ctx.bob_token),
                &serde_json::json!({}),
            )
            .await;
    }

    let resp = ctx
        .api
        .get("/api/v1/bookmarks?limit=2", Some(&ctx.bob_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 2, "limit=2 should return exactly 2 items");
}

/// min_id pagination returns the bookmarks just newer than the anchor, newest first.
#[tokio::test]
async fn test_bookmarks_min_id_pagination() {
    let ctx = TestContext::new("bmarks-min-id").await;

    let s1 = ctx
        .api
        .post_status(&ctx.alice_token, "min-id bmark 1", "public")
        .await;
    let s2 = ctx
        .api
        .post_status(&ctx.alice_token, "min-id bmark 2", "public")
        .await;
    let s3 = ctx
        .api
        .post_status(&ctx.alice_token, "min-id bmark 3", "public")
        .await;
    let s1_id = s1["id"].as_str().unwrap().to_string();
    let s2_id = s2["id"].as_str().unwrap().to_string();
    let s3_id = s3["id"].as_str().unwrap().to_string();

    // Bookmark in order s1, s2, s3; s1 gets the lowest sort_id.
    for id in [&s1_id, &s2_id, &s3_id] {
        ctx.api
            .post_json(
                &format!("/api/v1/statuses/{id}/bookmark"),
                Some(&ctx.bob_token),
                &serde_json::json!({}),
            )
            .await;
    }

    // The `next` link on the full page carries max_id=<s1's sort_id> (oldest).
    let all_resp = ctx.api.get("/api/v1/bookmarks", Some(&ctx.bob_token)).await;
    let link_header = all_resp
        .headers()
        .get("link")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let s1_sort_id = link_next_max_id(&link_header).expect("Link header with next cursor expected");

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/bookmarks?min_id={s1_sort_id}"),
            Some(&ctx.bob_token),
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
        "s2 should appear after min_id=s1's sort_id",
    );
    assert!(
        paged
            .iter()
            .any(|s| s["id"].as_str() == Some(s3_id.as_str())),
        "s3 should appear after min_id=s1's sort_id",
    );
}

/// Bookmarks are ordered by bookmark creation time (sort_id), not status ID.
/// When an older status is bookmarked after a newer one it appears first in the list.
#[tokio::test]
async fn test_bookmarks_pagination_consistent_with_ordering() {
    let ctx = TestContext::new("bmarks-order-pag").await;

    // s1 has a lower status ID (older), s2 has a higher status ID (newer).
    let s1 = ctx
        .api
        .post_status(&ctx.alice_token, "bmarks order older", "public")
        .await;
    let s2 = ctx
        .api
        .post_status(&ctx.alice_token, "bmarks order newer", "public")
        .await;
    let s1_id = s1["id"].as_str().unwrap().to_string();
    let s2_id = s2["id"].as_str().unwrap().to_string();

    // Bookmark s2 (newer status) first, then s1 (older status).
    // s1 gets the higher sort_id and should therefore appear first in the list.
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{s2_id}/bookmark"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{s1_id}/bookmark"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;

    // Default list is ordered by bookmark creation time (DESC), so s1 — most
    // recently bookmarked — should appear before s2.
    let all_resp = ctx.api.get("/api/v1/bookmarks", Some(&ctx.bob_token)).await;
    let link_header = all_resp
        .headers()
        .get("link")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let all: Vec<Value> = all_resp.json().await.unwrap();
    let all_ids: Vec<&str> = all.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        all_ids.contains(&s1_id.as_str()),
        "s1 should be in bookmarks"
    );
    assert!(
        all_ids.contains(&s2_id.as_str()),
        "s2 should be in bookmarks"
    );
    let s1_pos = all_ids.iter().position(|&id| id == s1_id.as_str()).unwrap();
    let s2_pos = all_ids.iter().position(|&id| id == s2_id.as_str()).unwrap();
    assert!(
        s1_pos < s2_pos,
        "s1 (bookmarked last) should appear before s2 (bookmarked first)"
    );

    // The `prev` link carries min_id=<s1's sort_id> (newest in page).
    // Using that value as max_id should return only s2.
    let s1_sort_id = link_prev_min_id(&link_header).expect("Link header with prev cursor expected");
    let paged: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/bookmarks?max_id={s1_sort_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        paged
            .iter()
            .any(|s| s["id"].as_str() == Some(s2_id.as_str())),
        "s2 should appear when paginating past s1's sort_id",
    );
    assert!(
        !paged
            .iter()
            .any(|s| s["id"].as_str() == Some(s1_id.as_str())),
        "s1 itself should not appear in results below its own sort_id",
    );
}
