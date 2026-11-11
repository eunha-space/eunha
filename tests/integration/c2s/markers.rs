use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::TestContext;

/// Set the home marker and get it back.
#[tokio::test]
async fn test_markers_home() {
    let ctx = TestContext::new("markers-home").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "marker test", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let set_resp = ctx
        .api
        .post_form(
            "/api/v1/markers",
            Some(&ctx.alice_token),
            &[("home[last_read_id]", id)],
        )
        .await;
    assert_eq!(set_resp.status(), StatusCode::OK);
    let markers: Value = set_resp.json().await.unwrap();
    assert_eq!(markers["home"]["last_read_id"].as_str(), Some(id));

    let get_resp = ctx
        .api
        .get("/api/v1/markers?timeline[]=home", Some(&ctx.alice_token))
        .await;
    assert_eq!(get_resp.status(), StatusCode::OK);
    let markers2: Value = get_resp.json().await.unwrap();
    assert_eq!(markers2["home"]["last_read_id"].as_str(), Some(id));
}

/// Set the notifications marker and get it back.
#[tokio::test]
async fn test_markers_notifications() {
    let ctx = TestContext::new("markers-notif").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    let notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    let notif_id = notifs[0]["id"].as_str().unwrap();

    let set_resp = ctx
        .api
        .post_form(
            "/api/v1/markers",
            Some(&ctx.bob_token),
            &[("notifications[last_read_id]", notif_id)],
        )
        .await;
    assert_eq!(set_resp.status(), StatusCode::OK);
    let markers: Value = set_resp.json().await.unwrap();
    assert_eq!(
        markers["notifications"]["last_read_id"].as_str(),
        Some(notif_id)
    );
}

/// The updated_at field on a marker is an ISO 8601 timestamp (RFC 3339 format).
#[tokio::test]
async fn test_marker_updated_at_is_iso8601() {
    let ctx = TestContext::new("markers-ts").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "ts marker test", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp: serde_json::Value = ctx
        .api
        .post_form(
            "/api/v1/markers",
            Some(&ctx.alice_token),
            &[("home[last_read_id]", id)],
        )
        .await
        .json()
        .await
        .unwrap();

    let ts = resp["home"]["updated_at"]
        .as_str()
        .expect("updated_at missing");
    // RFC 3339 ends with 'Z' or a numeric offset like +00:00.
    assert!(
        ts.contains('T') && (ts.ends_with('Z') || ts.contains('+')),
        "updated_at is not ISO 8601: {ts}",
    );
}

/// Updating the home marker increments the version.
#[tokio::test]
async fn test_marker_version_increments() {
    let ctx = TestContext::new("markers-ver").await;

    let s1 = ctx
        .api
        .post_status(&ctx.alice_token, "marker v1", "public")
        .await;
    let s2 = ctx
        .api
        .post_status(&ctx.alice_token, "marker v2", "public")
        .await;
    let id1 = s1["id"].as_str().unwrap();
    let id2 = s2["id"].as_str().unwrap();

    ctx.api
        .post_form(
            "/api/v1/markers",
            Some(&ctx.alice_token),
            &[("home[last_read_id]", id1)],
        )
        .await;
    let m: Value = ctx
        .api
        .post_form(
            "/api/v1/markers",
            Some(&ctx.alice_token),
            &[("home[last_read_id]", id2)],
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        m["home"]["version"].as_i64().unwrap_or(0) >= 2,
        "version should be ≥ 2 after two updates"
    );
}

/// Markers can be set with a JSON body, not only a form-encoded one.
///
/// Rails parses either transparently, so a client that posts JSON — as several
/// do — reaches Mastodon's controller with the same parameters. eunha read only
/// the bracket notation of a form post, and answered a JSON one with an empty
/// object and no error, so the marker silently never moved.
#[tokio::test]
async fn test_markers_accept_a_json_body() {
    let ctx = TestContext::new("markers-json").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "something to mark", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let response = ctx
        .api
        .post_json(
            "/api/v1/markers",
            Some(&ctx.alice_token),
            &serde_json::json!({"home": {"last_read_id": id}}),
        )
        .await;
    assert_eq!(response.status().as_u16(), 200);

    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["home"]["last_read_id"].as_str(),
        Some(id),
        "a JSON body should set the marker: {body}"
    );

    // And it is still there on the next read.
    let fetched: serde_json::Value = ctx
        .api
        .get("/api/v1/markers?timeline[]=home", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(fetched["home"]["last_read_id"].as_str(), Some(id));
}

/// An id sent unquoted works too, since clients send both.
#[tokio::test]
async fn test_markers_accept_a_numeric_json_id() {
    let ctx = TestContext::new("markers-json-num").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "something to mark", "public")
        .await;
    let id: i64 = status["id"].as_str().unwrap().parse().unwrap();

    let response = ctx
        .api
        .post_json(
            "/api/v1/markers",
            Some(&ctx.alice_token),
            &serde_json::json!({"home": {"last_read_id": id}}),
        )
        .await;
    assert_eq!(response.status().as_u16(), 200);

    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["home"]["last_read_id"].as_str(),
        Some(id.to_string().as_str())
    );
}
