//! The API under limited federation mode and
//! `disallow_unauthenticated_api_access`: who may call it, what it hides, and
//! what taking a domain off the allow list does.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

async fn limited(label: &str) -> TestContext {
    TestContext::with_instance(label, |instance| instance.limited_federation_mode = true).await
}

async fn error(response: reqwest::Response) -> (StatusCode, String) {
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    (
        status,
        body["error"].as_str().unwrap_or_default().to_owned(),
    )
}

/// Limited federation mode refuses the API to whoever is not signed in, but
/// for what a client needs first: the instance, registering an app, signing
/// up.
#[tokio::test]
async fn limited_federation_requires_an_authenticated_user() {
    let ctx = limited("lf-api").await;
    assert_eq!(
        error(ctx.api.get("/api/v1/timelines/public", None).await).await,
        (
            StatusCode::UNAUTHORIZED,
            "This method requires an authenticated user".into()
        )
    );
    assert_eq!(
        ctx.api
            .get("/api/v1/timelines/public", Some(&ctx.alice_token))
            .await
            .status(),
        StatusCode::OK
    );
    // `Api::V1::Instances::BaseController` requires a user in this mode.
    assert_eq!(
        ctx.api.get("/api/v1/instance/rules", None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        ctx.api
            .get("/api/v1/instance/rules", Some(&ctx.alice_token))
            .await
            .status(),
        StatusCode::OK
    );

    // The instance itself stays open, and says the mode it is in.
    let instance = ctx.api.get("/api/v2/instance", None).await;
    assert_eq!(instance.status(), StatusCode::OK);
    let instance: Value = instance.json().await.unwrap();
    assert_eq!(instance["configuration"]["limited_federation"], json!(true));
    assert_eq!(
        ctx.api.get("/api/v1/instance", None).await.status(),
        StatusCode::OK
    );
    let app = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &json!({"client_name": "test", "redirect_uris": "urn:ietf:wg:oauth:2.0:oob"}),
        )
        .await;
    assert_eq!(app.status(), StatusCode::OK);
}

/// In limited federation mode a signed-in user who cannot use the account is
/// refused everywhere, the instance included, as `require_functional!` does.
#[tokio::test]
async fn limited_federation_requires_a_functional_user() {
    let ctx = limited("lf-functional").await;
    sqlx::query!(
        "UPDATE users SET confirmed_at = NULL WHERE account_id = $1",
        ctx.bob_id.parse::<i64>().unwrap()
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    for path in [
        "/api/v2/instance".to_owned(),
        format!("/api/v1/accounts/{}", ctx.alice_id),
    ] {
        assert_eq!(
            error(ctx.api.get(&path, Some(&ctx.bob_token)).await).await,
            (
                StatusCode::FORBIDDEN,
                "Your login is missing a confirmed e-mail address".into()
            ),
            "{path}"
        );
    }

    // Outside the mode, an unconfirmed user reads what anyone may.
    let open = TestContext::new("lf-functional-off").await;
    sqlx::query!(
        "UPDATE users SET confirmed_at = NULL WHERE account_id = $1",
        open.bob_id.parse::<i64>().unwrap()
    )
    .execute(&open.db)
    .await
    .unwrap();
    assert_eq!(
        open.api
            .get("/api/v2/instance", Some(&open.bob_token))
            .await
            .status(),
        StatusCode::OK
    );
}

/// The peers, peer search and activity APIs are not there in limited
/// federation mode.
#[tokio::test]
async fn limited_federation_hides_peers_and_activity() {
    let ctx = limited("lf-peers").await;
    for path in [
        "/api/v1/instance/peers",
        "/api/v1/instance/activity",
        "/api/v1/peers/search?q=a",
    ] {
        assert_eq!(
            ctx.api.get(path, Some(&ctx.alice_token)).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    assert_eq!(
        ctx.api.get("/api/v1/peers/search?q=a", None).await.status(),
        StatusCode::UNAUTHORIZED,
        "a user is asked for before the API is found missing"
    );

    let open = TestContext::new("lf-peers-off").await;
    for path in [
        "/api/v1/instance/peers",
        "/api/v1/instance/activity",
        "/api/v1/peers/search?q=a",
    ] {
        assert_eq!(
            open.api.get(path, None).await.status(),
            StatusCode::OK,
            "{path}"
        );
    }
}

/// `disallow_unauthenticated_api_access` asks for a user as limited federation
/// mode does, but keeps the instance's own endpoints open and federates as
/// usual.
#[tokio::test]
async fn disallowing_unauthenticated_access_requires_a_user() {
    let ctx = TestContext::with_instance("disallow-api", |instance| {
        instance.disallow_unauthenticated_api_access = true
    })
    .await;
    assert_eq!(
        ctx.api.get("/api/v1/timelines/public", None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    for path in [
        "/api/v2/instance",
        "/api/v1/instance/rules",
        "/api/v1/instance/peers",
    ] {
        assert_eq!(
            ctx.api.get(path, None).await.status(),
            StatusCode::OK,
            "{path}"
        );
    }
    let instance: Value = ctx
        .api
        .get("/api/v2/instance", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        instance["configuration"]["limited_federation"],
        json!(false)
    );
    assert_eq!(
        ctx.api.ap_get("/users/alice", None).await.status(),
        StatusCode::OK,
        "fetches stay unsigned"
    );
}

/// Taking a domain off the allow list in limited federation mode suspends its
/// accounts and then deletes them; outside the mode the allow just goes.
#[tokio::test]
async fn unallowing_a_domain_deletes_its_accounts() {
    for limited_mode in [true, false] {
        let ctx = TestContext::with_instance("lf-unallow", |instance| {
            instance.limited_federation_mode = limited_mode
        })
        .await;
        crate::helpers::make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;
        let allow: Value = ctx
            .api
            .post_json(
                "/api/v1/admin/domain_allows",
                Some(&ctx.alice_token),
                &json!({"domain": "former.invalid"}),
            )
            .await
            .json()
            .await
            .unwrap();
        let account_id = eunha::snowflake::next_id();
        sqlx::query!(
            r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri,
                                     inbox_url, outbox_url, created_at, updated_at)
               VALUES ($1, 'gone', 'former.invalid', 'gone', '',
                       'https://former.invalid/@gone', 'https://former.invalid/users/gone',
                       'https://former.invalid/users/gone/inbox',
                       'https://former.invalid/users/gone/outbox', now(), now())"#,
            account_id,
        )
        .execute(&ctx.db)
        .await
        .unwrap();

        let deleted = ctx
            .api
            .delete(
                &format!(
                    "/api/v1/admin/domain_allows/{}",
                    allow["id"].as_str().unwrap()
                ),
                &ctx.alice_token,
            )
            .await;
        assert_eq!(deleted.status(), StatusCode::OK);
        let remaining: i64 = sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!" FROM accounts WHERE id = $1"#,
            account_id
        )
        .fetch_one(&ctx.db)
        .await
        .unwrap();
        assert_eq!(
            remaining,
            i64::from(!limited_mode),
            "limited: {limited_mode}"
        );
    }
}
