//! `Admin::RulesController`.

use super::*;

const MANAGE_RULES: i64 = 1 << 12;

fn texts(rules: &Value) -> Vec<String> {
    rules
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["text"].as_str().unwrap().to_owned())
        .collect()
}

/// Rules: `manage_rules`, validated as `Rule` and `RuleTranslation` are,
/// moved as `Rule#move!` moves them, discarded rather than deleted, and served
/// to everyone in their new order; none of it logged.
#[tokio::test]
async fn test_rules() {
    let ctx = TestContext::new("srv-rules").await;
    crate::helpers::grant_admin_scopes(&ctx.db, id(&ctx.bob_id)).await;
    let refused = ctx
        .api
        .post_json(
            "/api/v1/admin/rules",
            Some(&ctx.bob_token),
            &json!({"text": "Be kind"}),
        )
        .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    give_role(&ctx, &ctx.bob_id, 10, MANAGE_RULES).await;
    let post = |body: Value| {
        let api = &ctx.api;
        let token = ctx.bob_token.clone();
        async move {
            api.post_json("/api/v1/admin/rules", Some(&token), &body)
                .await
        }
    };

    assert_eq!(
        error_of(
            post(json!({"text": " "})).await,
            StatusCode::UNPROCESSABLE_ENTITY
        )
        .await,
        "Validation failed: Text can't be blank"
    );
    assert_eq!(
        error_of(
            post(json!({"text": "x".repeat(301)})).await,
            StatusCode::UNPROCESSABLE_ENTITY
        )
        .await,
        "Validation failed: Text is too long (maximum is 300 characters)"
    );
    assert_eq!(
        error_of(
            post(json!({"text": "Be kind", "translations_attributes": [
                {"language": "ko", "text": "친절하게"},
                {"language": "ko", "text": "다정하게"},
            ]}))
            .await,
            StatusCode::UNPROCESSABLE_ENTITY
        )
        .await,
        "Validation failed: Translations language has already been taken"
    );

    let first = json_ok(
        post(json!({
            "text": "Be kind",
            "hint": "To everyone",
            "translations_attributes": [
                {"language": "ko", "text": "친절하게", "hint": "모두에게"},
                // Rejected for its blank text, as `reject_if` rejects it.
                {"language": "ja", "text": ""},
            ],
        }))
        .await,
    )
    .await;
    assert_eq!(first["translations"].as_array().unwrap().len(), 1);
    let second = json_ok(post(json!({"text": "No spam"})).await).await;
    let third = json_ok(post(json!({"text": "No ads"})).await).await;

    let mv = |rule: &Value, direction: &'static str| {
        let api = &ctx.api;
        let token = ctx.bob_token.clone();
        let path = format!(
            "/api/v1/admin/rules/{}/{direction}",
            rule["id"].as_str().unwrap()
        );
        async move { api.post_json(&path, Some(&token), &json!({})).await }
    };
    let moved = json_ok(mv(&first, "move_down").await).await;
    assert_eq!(texts(&moved), ["No spam", "Be kind", "No ads"]);
    // The first rule moved up goes to the end, where Ruby's `insert(-1, …)`
    // puts it.
    let moved = json_ok(mv(&second, "move_up").await).await;
    assert_eq!(texts(&moved), ["Be kind", "No ads", "No spam"]);
    // The last one moved down stays where it is.
    let moved = json_ok(mv(&second, "move_down").await).await;
    assert_eq!(texts(&moved), ["Be kind", "No ads", "No spam"]);

    // Editing drops a translation marked to destroy and adds another.
    let ko = first["translations"][0]["id"].as_str().unwrap();
    let edited = json_ok(
        ctx.api
            .patch_json(
                &format!("/api/v1/admin/rules/{}", first["id"].as_str().unwrap()),
                Some(&ctx.bob_token),
                &json!({"translations_attributes": [
                    {"id": ko, "_destroy": "1"},
                    {"language": "ja", "text": "親切に"},
                ]}),
            )
            .await,
    )
    .await;
    assert_eq!(edited["text"], "Be kind");
    assert_eq!(edited["translations"].as_array().unwrap().len(), 1);
    assert_eq!(edited["translations"][0]["language"], "ja");

    json_ok(
        ctx.api
            .delete(
                &format!("/api/v1/admin/rules/{}", third["id"].as_str().unwrap()),
                &ctx.bob_token,
            )
            .await,
    )
    .await;
    let public = json_ok(ctx.api.get("/api/v1/instance/rules", None).await).await;
    assert_eq!(texts(&public), ["Be kind", "No spam"]);
    assert_eq!(public[0]["translations"]["ja"]["text"], "親切に");
    let deleted_at: Option<chrono::NaiveDateTime> =
        sqlx::query_scalar("SELECT deleted_at FROM rules WHERE id = $1")
            .bind(id(third["id"].as_str().unwrap()))
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(deleted_at.is_some());
    assert!(logs(&ctx).await.is_empty());
}
