//! The errors eunha returns, against the ones Mastodon returns.
//!
//! A client branches on the status and shows the message, so both are part of
//! the API. Mastodon's are set in `Api::BaseController` and
//! `Api::ErrorHandling`; the ones checked here are those a client meets in
//! ordinary use.

use crate::helpers::TestContext;

async fn error_of(response: reqwest::Response) -> (u16, String) {
    let status = response.status().as_u16();
    let body: serde_json::Value = response.json().await.unwrap_or(serde_json::json!({}));
    (
        status,
        body["error"].as_str().unwrap_or_default().to_string(),
    )
}

/// A missing record is `404 Record not found`, from `Api::ErrorHandling`'s
/// `rescue_from ActiveRecord::RecordNotFound`.
#[tokio::test]
async fn test_a_missing_record_is_not_found() {
    let ctx = TestContext::new("errors-not-found").await;

    let (status, error) = error_of(
        ctx.api
            .get("/api/v1/statuses/1234567890123456", Some(&ctx.alice_token))
            .await,
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(error, "Record not found");
}

/// A token without the scope an endpoint needs is `403`, and Mastodon says
/// which kind of failure it was: `doorkeeper_forbidden_render_options`.
#[tokio::test]
async fn test_a_missing_scope_says_it_is_about_scopes() {
    let ctx = TestContext::new("errors-scope").await;

    // A token good for reading, used to write.
    let token =
        crate::helpers::seed_token_with_scopes(&ctx.db, ctx.alice_id.parse().unwrap(), "read")
            .await;
    let (status, error) = error_of(
        ctx.api
            .post_json(
                "/api/v1/statuses",
                Some(&token),
                &serde_json::json!({"status": "not allowed"}),
            )
            .await,
    )
    .await;

    assert_eq!(
        status, 403,
        "a scope failure is forbidden, not unauthorized"
    );
    assert_eq!(
        error, "This action is outside the authorized scopes",
        "Mastodon distinguishes a scope failure from a disallowed action"
    );
}

/// No token at all, where one is required, is `401`.
#[tokio::test]
async fn test_no_token_is_unauthorized() {
    let ctx = TestContext::new("errors-no-token").await;

    let (status, _) = error_of(ctx.api.get("/api/v1/timelines/home", None).await).await;
    assert_eq!(status, 401);
}
