//! The instance actor served at `/actor`.

use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::TestContext;

/// GET /actor returns an Application actor carrying a stable public key.
#[tokio::test]
async fn test_instance_actor_served_with_public_key() {
    let ctx = TestContext::new("inst-actor").await;

    let resp = ctx.api.ap_get("/actor", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let actor: Value = resp.json().await.unwrap();

    let actor_url = format!("https://{}/actor", ctx.domain);
    assert_eq!(actor["type"].as_str(), Some("Application"));
    assert_eq!(actor["id"].as_str(), Some(actor_url.as_str()));
    assert_eq!(
        actor["preferredUsername"].as_str(),
        Some(ctx.domain.as_str())
    );
    assert_eq!(
        actor["publicKey"]["id"].as_str(),
        Some(format!("{actor_url}#main-key").as_str())
    );
    assert_eq!(
        actor["publicKey"]["owner"].as_str(),
        Some(actor_url.as_str())
    );

    let pem = actor["publicKey"]["publicKeyPem"].as_str().unwrap_or("");
    assert!(
        pem.contains("BEGIN PUBLIC KEY"),
        "expected a PEM public key, got: {pem:?}"
    );

    // The key is persisted, so a second fetch returns the same one.
    let actor2: Value = ctx.api.ap_get("/actor", None).await.json().await.unwrap();
    assert_eq!(actor2["publicKey"]["publicKeyPem"].as_str(), Some(pem));
}
