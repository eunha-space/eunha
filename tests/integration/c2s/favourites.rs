use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::TestContext;

/// Parse the `max_id` value from the `rel="next"` Link header entry.
fn link_next_max_id(link: &str) -> Option<String> {
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

/// GET /api/v1/favourites returns statuses the user has favourited.
#[tokio::test]
async fn test_favourites_returns_favourited_statuses() {
    let ctx = TestContext::new("favs-list").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "fav listing test", "public")
        .await;
    let status_id = status["id"].as_str().unwrap().to_string();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{status_id}/favourite"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;

    let resp = ctx
        .api
        .get("/api/v1/favourites", Some(&ctx.bob_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Vec<Value> = resp.json().await.unwrap();
    let ids: Vec<&str> = body.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        ids.contains(&status_id.as_str()),
        "favourited status missing from /api/v1/favourites"
    );
}

/// GET /api/v1/favourites is empty when nothing has been favourited.
#[tokio::test]
async fn test_favourites_empty_when_none() {
    let ctx = TestContext::new("favs-empty").await;

    let resp = ctx
        .api
        .get("/api/v1/favourites", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Vec<Value> = resp.json().await.unwrap();
    assert!(body.is_empty());
}

/// GET /api/v1/favourites requires authentication.
#[tokio::test]
async fn test_favourites_requires_auth() {
    let ctx = TestContext::new("favs-unauth").await;

    let resp = ctx.api.get("/api/v1/favourites", None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Unfavouriting a status removes it from GET /api/v1/favourites.
#[tokio::test]
async fn test_favourites_unfavourite_removes_from_list() {
    let ctx = TestContext::new("favs-unfav").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "unfav listing test", "public")
        .await;
    let status_id = status["id"].as_str().unwrap().to_string();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{status_id}/favourite"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{status_id}/unfavourite"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;

    let resp = ctx
        .api
        .get("/api/v1/favourites", Some(&ctx.bob_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Vec<Value> = resp.json().await.unwrap();
    let ids: Vec<&str> = body.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        !ids.contains(&status_id.as_str()),
        "unfavourited status still in favourites list"
    );
}

/// max_id pagination excludes the given sort_id and newer ones.
#[tokio::test]
async fn test_favourites_max_id_pagination() {
    let ctx = TestContext::new("favs-maxid").await;

    let s1 = ctx
        .api
        .post_status(&ctx.alice_token, "fav maxid first", "public")
        .await;
    let s2 = ctx
        .api
        .post_status(&ctx.alice_token, "fav maxid second", "public")
        .await;
    let s1_id = s1["id"].as_str().unwrap().to_string();
    let s2_id = s2["id"].as_str().unwrap().to_string();

    // Favourite s1 then s2; s2 gets the higher sort_id.
    for id in [&s1_id, &s2_id] {
        ctx.api
            .post_json(
                &format!("/api/v1/statuses/{id}/favourite"),
                Some(&ctx.bob_token),
                &serde_json::json!({}),
            )
            .await;
    }

    // Fetch just the newest favourite (s2) to get its sort_id cursor.
    let first_resp = ctx
        .api
        .get("/api/v1/favourites?limit=1", Some(&ctx.bob_token))
        .await;
    let link_header = first_resp
        .headers()
        .get("link")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // The `next` link carries max_id=<s2's sort_id>.
    let cursor = link_next_max_id(&link_header)
        .expect("Link header with next cursor expected when two favourites exist");

    let paged: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/favourites?max_id={cursor}"),
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
        "max_id favourite should be excluded",
    );
    assert!(
        paged
            .iter()
            .any(|s| s["id"].as_str() == Some(s1_id.as_str())),
        "s1 should appear when max_id=s2's sort_id",
    );
}

/// since_id pagination returns only favourites newer than the given sort_id cursor.
#[tokio::test]
async fn test_favourites_since_id_pagination() {
    let ctx = TestContext::new("favs-since").await;

    let s1 = ctx
        .api
        .post_status(&ctx.alice_token, "fav since first", "public")
        .await;
    let s2 = ctx
        .api
        .post_status(&ctx.alice_token, "fav since second", "public")
        .await;
    let s1_id = s1["id"].as_str().unwrap().to_string();
    let s2_id = s2["id"].as_str().unwrap().to_string();

    // Favourite s1 then s2; s1 gets the lower sort_id.
    for id in [&s1_id, &s2_id] {
        ctx.api
            .post_json(
                &format!("/api/v1/statuses/{id}/favourite"),
                Some(&ctx.bob_token),
                &serde_json::json!({}),
            )
            .await;
    }

    // The `next` link on the full page carries max_id=<s1's sort_id> (oldest).
    let all_resp = ctx
        .api
        .get("/api/v1/favourites", Some(&ctx.bob_token))
        .await;
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
            &format!("/api/v1/favourites?since_id={s1_sort_id}"),
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
        "since_id favourite should be excluded",
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
async fn test_favourites_limit_param() {
    let ctx = TestContext::new("favs-limit").await;

    for i in 0..3 {
        let s = ctx
            .api
            .post_status(&ctx.alice_token, &format!("fav limit {i}"), "public")
            .await;
        let sid = s["id"].as_str().unwrap().to_string();
        ctx.api
            .post_json(
                &format!("/api/v1/statuses/{sid}/favourite"),
                Some(&ctx.bob_token),
                &serde_json::json!({}),
            )
            .await;
    }

    let resp = ctx
        .api
        .get("/api/v1/favourites?limit=2", Some(&ctx.bob_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 2, "limit=2 should return exactly 2 items");
}

/// min_id pagination returns the favourites just newer than the anchor, newest first.
#[tokio::test]
async fn test_favourites_min_id_pagination() {
    let ctx = TestContext::new("favs-min-id").await;

    let s1 = ctx
        .api
        .post_status(&ctx.alice_token, "min-id fav 1", "public")
        .await;
    let s2 = ctx
        .api
        .post_status(&ctx.alice_token, "min-id fav 2", "public")
        .await;
    let s3 = ctx
        .api
        .post_status(&ctx.alice_token, "min-id fav 3", "public")
        .await;
    let s1_id = s1["id"].as_str().unwrap().to_string();
    let s2_id = s2["id"].as_str().unwrap().to_string();
    let s3_id = s3["id"].as_str().unwrap().to_string();

    // Favourite in order s1, s2, s3; s1 gets the lowest sort_id.
    for id in [&s1_id, &s2_id, &s3_id] {
        ctx.api
            .post_json(
                &format!("/api/v1/statuses/{id}/favourite"),
                Some(&ctx.bob_token),
                &serde_json::json!({}),
            )
            .await;
    }

    // The `next` link on the full page carries max_id=<s1's sort_id> (oldest).
    let all_resp = ctx
        .api
        .get("/api/v1/favourites", Some(&ctx.bob_token))
        .await;
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
            &format!("/api/v1/favourites?min_id={s1_sort_id}"),
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
