//! Translation: `POST /api/v1/statuses/:id/translate`,
//! `/api/v1/instance/translation_languages` and `configuration.translation`,
//! against fake LibreTranslate and DeepL servers.

use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode as AxumStatus},
    response::{IntoResponse, Response},
    routing, Json, Router,
};
use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{tiny_png, TestContext};

/// What a fake translation server was asked, and how it is to answer.
#[derive(Default)]
struct Fake {
    /// Status code `/translate` answers with; 0 for success.
    translate_status: AtomicU16,
    translate_calls: AtomicUsize,
    language_calls: AtomicUsize,
    /// The last translate request body, as JSON (LibreTranslate) or as form
    /// pairs (DeepL).
    last_request: Mutex<Value>,
    last_auth: Mutex<Option<String>>,
}

async fn spawn(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

/// A LibreTranslate server that knows English, German and French, and
/// "translates" by prefixing `[target]`.
async fn spawn_libre_translate() -> (String, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    async fn languages(State(fake): State<Arc<Fake>>) -> Json<Value> {
        fake.language_calls.fetch_add(1, Ordering::SeqCst);
        Json(json!([
            {"code": "en", "name": "English", "targets": ["de", "en", "fr"]},
            {"code": "de", "name": "German", "targets": ["de", "en"]},
            {"code": "fr", "name": "French", "targets": ["en", "fr"]},
        ]))
    }
    async fn translate(State(fake): State<Arc<Fake>>, Json(body): Json<Value>) -> Response {
        fake.translate_calls.fetch_add(1, Ordering::SeqCst);
        *fake.last_request.lock().unwrap() = body.clone();
        let status = fake.translate_status.load(Ordering::SeqCst);
        if status != 0 {
            return (AxumStatus::from_u16(status).unwrap(), "nope").into_response();
        }
        let target = body["target"].as_str().unwrap_or("").to_string();
        let texts: Vec<Value> = body["q"]
            .as_array()
            .unwrap()
            .iter()
            .map(|q| json!(format!("[{target}] {}", q.as_str().unwrap())))
            .collect();
        let mut out = json!({ "translatedText": texts });
        if body["source"] == "auto" {
            out["detectedLanguage"] = json!(texts
                .iter()
                .map(|_| json!({"confidence": 90, "language": "fr"}))
                .collect::<Vec<_>>());
        }
        Json(out).into_response()
    }
    let router = Router::new()
        .route("/languages", routing::get(languages))
        .route("/translate", routing::post(translate))
        .with_state(fake.clone());
    (spawn(router).await, fake)
}

/// A DeepL API with English, German and Japanese.
async fn spawn_deepl() -> (String, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    async fn languages(
        State(fake): State<Arc<Fake>>,
        axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    ) -> Json<Value> {
        fake.language_calls.fetch_add(1, Ordering::SeqCst);
        if q.get("type").map(String::as_str) == Some("target") {
            Json(json!([
                {"language": "DE", "name": "German"},
                {"language": "EN-GB", "name": "English (British)"},
                {"language": "JA", "name": "Japanese"},
            ]))
        } else {
            Json(json!([
                {"language": "DE", "name": "German"},
                {"language": "EN", "name": "English"},
                {"language": "JA", "name": "Japanese"},
            ]))
        }
    }
    async fn translate(
        State(fake): State<Arc<Fake>>,
        headers: HeaderMap,
        body: String,
    ) -> Response {
        fake.translate_calls.fetch_add(1, Ordering::SeqCst);
        *fake.last_auth.lock().unwrap() = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let pairs: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect();
        *fake.last_request.lock().unwrap() = json!(pairs);
        let status = fake.translate_status.load(Ordering::SeqCst);
        if status != 0 {
            return (AxumStatus::from_u16(status).unwrap(), "nope").into_response();
        }
        let target = pairs
            .iter()
            .find(|(k, _)| k == "target_lang")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let translations: Vec<Value> = pairs
            .iter()
            .filter(|(k, _)| k == "text")
            .map(|(_, t)| json!({"detected_source_language": "EN", "text": format!("[{target}] {t}")}))
            .collect();
        Json(json!({ "translations": translations })).into_response()
    }
    let router = Router::new()
        .route("/v2/languages", routing::get(languages))
        .route("/v2/translate", routing::post(translate))
        .with_state(fake.clone());
    (spawn(router).await, fake)
}

async fn libre_context(label: &str) -> (TestContext, Arc<Fake>) {
    let (endpoint, fake) = spawn_libre_translate().await;
    let ctx = TestContext::with_instance_config(label, move |instance| {
        instance.translation.libre_translate_endpoint = Some(endpoint);
    })
    .await;
    (ctx, fake)
}

async fn translate(ctx: &TestContext, status_id: &str, lang: &str) -> reqwest::Response {
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{status_id}/translate"),
            Some(&ctx.bob_token),
            &json!({ "lang": lang }),
        )
        .await
}

async fn post(ctx: &TestContext, body: Value) -> Value {
    let resp = ctx
        .api
        .post_json("/api/v1/statuses", Some(&ctx.alice_token), &body)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    resp.json().await.unwrap()
}

#[tokio::test]
async fn unconfigured_instance_offers_no_translation() {
    let ctx = TestContext::new("translate-off").await;
    let languages: Value = ctx
        .api
        .get("/api/v1/instance/translation_languages", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(languages, json!({}));

    let instance: Value = ctx
        .api
        .get("/api/v2/instance", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(instance["configuration"]["translation"]["enabled"], false);

    let status = post(&ctx, json!({"status": "Hello", "language": "en"})).await;
    let resp = translate(&ctx, status["id"].as_str().unwrap(), "de").await;
    // `TranslationService::NotConfiguredError` is rescued with `not_found`.
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({"error": "Not Found"})
    );
}

#[tokio::test]
async fn libre_translate_languages_are_listed_and_cached() {
    let (ctx, fake) = libre_context("translate-langs").await;
    for _ in 0..2 {
        let languages: Value = ctx
            .api
            .get("/api/v1/instance/translation_languages", None)
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(
            languages,
            json!({
                "en": ["de", "fr"],
                "de": ["en"],
                "fr": ["en"],
                "und": ["de", "en", "fr"],
            })
        );
    }
    assert_eq!(fake.language_calls.load(Ordering::SeqCst), 1);

    let instance: Value = ctx
        .api
        .get("/api/v2/instance", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(instance["configuration"]["translation"]["enabled"], true);
}

#[tokio::test]
async fn libre_translate_translates_every_text_and_caches() {
    let (ctx, fake) = libre_context("translate-libre").await;
    let status = post(
        &ctx,
        json!({
            "status": "Which do you prefer?",
            "spoiler_text": "Pets <3",
            "language": "en",
            "poll": {"options": ["Cats", "Dogs"], "expires_in": 86400},
        }),
    )
    .await;
    let id = status["id"].as_str().unwrap();

    let resp = translate(&ctx, id, "de").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["language"], "de");
    assert_eq!(body["provider"], "LibreTranslate");
    // The service is told the source, so it detects nothing and the status's
    // own language stands.
    assert_eq!(body["detected_source_language"], "en");
    assert_eq!(body["content"], "[de] <p>Which do you prefer?</p>");
    assert_eq!(body["spoiler_text"], "[de] Pets <3");
    assert_eq!(body["poll"]["id"], status["poll"]["id"]);
    assert_eq!(
        body["poll"]["options"],
        json!([{"title": "[de] Cats"}, {"title": "[de] Dogs"}])
    );
    assert_eq!(body["media_attachments"], json!([]));

    let request = fake.last_request.lock().unwrap().clone();
    assert_eq!(request["source"], "en");
    assert_eq!(request["target"], "de");
    assert_eq!(request["format"], "html");
    assert_eq!(request["api_key"], Value::Null);
    // Spoiler text, options and descriptions go as escaped HTML.
    assert_eq!(
        request["q"],
        json!(["<p>Which do you prefer?</p>", "Pets &lt;3", "Cats", "Dogs",])
    );

    // A second request in the day is answered from the cache.
    let again: Value = translate(&ctx, id, "de").await.json().await.unwrap();
    assert_eq!(again, body);
    assert_eq!(fake.translate_calls.load(Ordering::SeqCst), 1);

    // An edit changes the texts, and so the cache key.
    let resp = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.alice_token),
            &json!({"status": "Which one?", "language": "en"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let edited: Value = translate(&ctx, id, "de").await.json().await.unwrap();
    assert_eq!(edited["content"], "[de] <p>Which one?</p>");
    assert_eq!(fake.translate_calls.load(Ordering::SeqCst), 2);

    // Descriptions are translated as escaped text, and come back decoded.
    let upload: Value = ctx
        .api
        .post_multipart_file(
            "/api/v1/media",
            &ctx.alice_token,
            "test.png",
            "image/png",
            tiny_png(),
            &[("description", "a cat & a dog")],
        )
        .await
        .json()
        .await
        .unwrap();
    let media_id = upload["id"].as_str().unwrap();
    let pictured = post(
        &ctx,
        json!({"status": "Look", "language": "en", "media_ids": [media_id]}),
    )
    .await;
    let body: Value = translate(&ctx, pictured["id"].as_str().unwrap(), "fr")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        fake.last_request.lock().unwrap()["q"],
        json!(["<p>Look</p>", "a cat &amp; a dog"])
    );
    assert_eq!(
        body["media_attachments"],
        json!([{"id": media_id, "description": "[fr] a cat & a dog"}])
    );
}

#[tokio::test]
async fn target_follows_locale_and_region_falls_back() {
    let (ctx, fake) = libre_context("translate-locale").await;
    let status = post(&ctx, json!({"status": "Bonjour", "language": "fr"})).await;
    let id = status["id"].as_str().unwrap();

    // With no `lang`, and no locale of bob's own, the `Accept-Language`
    // header picks the target.
    let resp = ctx
        .api
        .http
        .post(ctx.api.url(&format!("/api/v1/statuses/{id}/translate")))
        .header("host", &ctx.domain)
        .header("accept-language", "en-US,en;q=0.9")
        .bearer_auth(&ctx.bob_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["language"], "en");
    assert_eq!(fake.last_request.lock().unwrap()["target"], "en");

    // A regional locale English has no target for falls back to its
    // language alone.
    let hello = post(&ctx, json!({"status": "Hello", "language": "en"})).await;
    let body: Value = translate(&ctx, hello["id"].as_str().unwrap(), "fr-CA")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["language"], "fr");
}

#[tokio::test]
async fn untranslatable_statuses_are_refused() {
    let (ctx, _fake) = libre_context("translate-refuse").await;

    // A followers-only status bob can see is not `distributable?`.
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    let private = post(
        &ctx,
        json!({"status": "Hi", "language": "en", "visibility": "private"}),
    )
    .await;
    let resp = translate(&ctx, private["id"].as_str().unwrap(), "de").await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({"error": "This action is not allowed"})
    );

    let public = post(&ctx, json!({"status": "Hi", "language": "en"})).await;
    let id = public["id"].as_str().unwrap();
    // No translating a status into its own language, or into one the
    // service lacks.
    assert_eq!(
        translate(&ctx, id, "en").await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        translate(&ctx, id, "ja").await.status(),
        StatusCode::FORBIDDEN
    );

    // A direct message bob is not part of is not there at all.
    let direct = post(
        &ctx,
        json!({"status": "secret", "language": "en", "visibility": "direct"}),
    )
    .await;
    let resp = translate(&ctx, direct["id"].as_str().unwrap(), "de").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        translate(&ctx, "1", "de").await.status(),
        StatusCode::NOT_FOUND
    );

    // `read:statuses` is required.
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/translate"),
            None,
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn service_errors_are_service_unavailable() {
    let (ctx, fake) = libre_context("translate-errors").await;
    let status = post(&ctx, json!({"status": "Hello", "language": "en"})).await;
    let id = status["id"].as_str().unwrap();

    for (code, message) in [
        (
            429,
            "There have been too many requests to the translation service recently.",
        ),
        (
            403,
            "The server-wide usage quota for the translation service has been exceeded.",
        ),
        (500, "Service Unavailable"),
    ] {
        fake.translate_status.store(code, Ordering::SeqCst);
        let resp = translate(&ctx, id, "de").await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "for {code}");
        assert_eq!(
            resp.json::<Value>().await.unwrap(),
            json!({"error": message})
        );
    }
}

#[tokio::test]
async fn unreachable_service_is_service_unavailable() {
    // A port nothing listens on.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let ctx = TestContext::with_instance_config("translate-down", move |instance| {
        instance.translation.libre_translate_endpoint = Some(endpoint);
    })
    .await;
    let status = post(&ctx, json!({"status": "Hello", "language": "en"})).await;
    let resp = translate(&ctx, status["id"].as_str().unwrap(), "de").await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({"error": "Remote data could not be fetched"})
    );
    let resp = ctx
        .api
        .get("/api/v1/instance/translation_languages", None)
        .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn deepl_is_preferred_and_translates() {
    let (libre, libre_fake) = spawn_libre_translate().await;
    let (deepl, fake) = spawn_deepl().await;
    let ctx = TestContext::with_instance_config("translate-deepl", move |instance| {
        instance.translation.libre_translate_endpoint = Some(libre);
        instance.translation.deepl_api_key = Some("secret-key".into());
        instance.translation.deepl_endpoint = Some(deepl);
    })
    .await;

    let languages: Value = ctx
        .api
        .get("/api/v1/instance/translation_languages", None)
        .await
        .json()
        .await
        .unwrap();
    // EN and PT are always targets; each source loses itself.
    assert_eq!(
        languages,
        json!({
            "de": ["en", "pt", "en-GB", "ja"],
            "en": ["pt", "de", "en-GB", "ja"],
            "ja": ["en", "pt", "de", "en-GB"],
            "und": ["en", "pt", "de", "en-GB", "ja"],
        })
    );
    assert_eq!(libre_fake.language_calls.load(Ordering::SeqCst), 0);

    let status = post(
        &ctx,
        json!({"status": "Good morning", "spoiler_text": "greeting", "language": "en"}),
    )
    .await;
    let id = status["id"].as_str().unwrap();
    let resp = translate(&ctx, id, "ja").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["provider"], "DeepL.com");
    assert_eq!(body["detected_source_language"], "en");
    assert_eq!(body["content"], "[ja] <p>Good morning</p>");
    assert_eq!(body["spoiler_text"], "[ja] greeting");
    assert_eq!(body["poll"], Value::Null);
    assert_eq!(body["media_attachments"], json!([]));
    assert_eq!(
        fake.last_auth.lock().unwrap().as_deref(),
        Some("DeepL-Auth-Key secret-key")
    );
    assert_eq!(
        fake.last_request.lock().unwrap().clone(),
        json!([
            ["text", "<p>Good morning</p>"],
            ["text", "greeting"],
            ["source_lang", "EN"],
            ["target_lang", "ja"],
            ["tag_handling", "html"],
        ])
    );

    // DeepL's 456 is its quota.
    let other = post(&ctx, json!({"status": "Good night", "language": "en"})).await;
    fake.translate_status.store(456, Ordering::SeqCst);
    let resp = translate(&ctx, other["id"].as_str().unwrap(), "de").await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({"error": "The server-wide usage quota for the translation service has been exceeded."})
    );
}

#[tokio::test]
async fn custom_emoji_shortcodes_are_kept_from_the_service() {
    let (ctx, fake) = libre_context("translate-emoji").await;
    sqlx::query(
        "INSERT INTO custom_emojis (id, shortcode, domain, disabled, uri, image_remote_url, created_at, updated_at)
         VALUES (nextval('custom_emojis_id_seq'), 'blobcat', NULL, false, $1, $2, now(), now())",
    )
    .bind(format!("https://{}/emojis/blobcat", ctx.domain))
    .bind(format!("https://{}/blobcat.png", ctx.domain))
    .execute(&ctx.db)
    .await
    .unwrap();
    let status = post(
        &ctx,
        json!({"status": "Hi :blobcat: there", "language": "en"}),
    )
    .await;
    let resp = translate(&ctx, status["id"].as_str().unwrap(), "de").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let request = fake.last_request.lock().unwrap().clone();
    assert_eq!(
        request["q"][0],
        "<p>Hi <span translate=\"no\">:blobcat:</span> there</p>"
    );
    let body: Value = resp.json().await.unwrap();
    // The span goes again once the service is done with it.
    assert_eq!(body["content"], "[de] <p>Hi :blobcat: there</p>");
}
