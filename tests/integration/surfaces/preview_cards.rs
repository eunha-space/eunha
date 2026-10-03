//! Preview cards, fetched as Mastodon's `FetchLinkCardService` fetches them,
//! from pages served by a local stand-in for the web.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use serde_json::{json, Value};

use crate::helpers::TestContext;

struct Site {
    base: String,
    creator: String,
    article_hits: AtomicUsize,
}

fn cover_png() -> Vec<u8> {
    let image = image::RgbImage::from_fn(1280, 720, |x, y| {
        image::Rgb([(x / 5) as u8, (y / 3) as u8, 90])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

fn html(body: String) -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
}

async fn article(State(site): State<Arc<Site>>) -> Response {
    site.article_hits.fetch_add(1, Ordering::SeqCst);
    html(format!(
        r#"<!doctype html><html lang="ko-KR"><head>
           <title>Ignored</title>
           <link rel="canonical" href="/article">
           <meta property="og:title" content="A long read &amp;amp; more">
           <meta property="og:description" content="What happened">
           <meta property="og:image" content="/cover.png">
           <meta property="og:image:alt" content="The cover">
           <meta property="og:type" content="article">
           <meta property="og:site_name" content="The Daily">
           <meta property="article:published_time" content="2024-02-03T04:05:06+00:00">
           <meta property="og:author" content="Ann Writer">
           <meta name="fediverse:creator" content="{}">
           </head><body><p>Hello</p></body></html>"#,
        site.creator
    ))
}

async fn video(State(site): State<Arc<Site>>) -> Response {
    let page = format!("{}/video", site.base);
    html(format!(
        r#"<html><head><title>Video page</title>
           <link rel="alternate" type="application/json+oembed"
                 href="/oembed?format=json&amp;url={}">
           </head></html>"#,
        urlencoding::encode(&page)
    ))
}

async fn oembed(Query(q): Query<std::collections::HashMap<String, String>>) -> Response {
    assert!(q.contains_key("url"));
    axum::Json(json!({
        "version": "1.0",
        "type": "video",
        "title": "A video",
        "author_name": "Vid Maker",
        "author_url": "/u/vid",
        "provider_name": "Tube",
        "provider_url": "/",
        "width": 480,
        "height": "270",
        "thumbnail_url": "/cover.png",
        "html": "<iframe src=\"https://player.example/embed/1\" width=\"480\" height=\"270\" allow=\"autoplay\"></iframe><script>alert(1)</script>",
    }))
    .into_response()
}

async fn spawn_site(creator: &str) -> Arc<Site> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let site = Arc::new(Site {
        // A name rather than an address: the fetch refuses literal private
        // addresses before it ever resolves anything.
        base: format!("http://localhost:{port}"),
        creator: creator.to_owned(),
        article_hits: AtomicUsize::new(0),
    });
    let png = cover_png();
    let app = Router::new()
        .route("/short", get(|| async { Redirect::permanent("/article") }))
        .route("/article", get(article))
        .route("/video", get(video))
        .route("/oembed", get(oembed))
        .route(
            "/plain",
            get(|| async { html("<html><head></head><body></body></html>".into()) }),
        )
        .route(
            "/cover.png",
            get(move || {
                let png = png.clone();
                async move { ([(header::CONTENT_TYPE, "image/png")], png).into_response() }
            }),
        )
        .route(
            "/missing",
            get(|| async { StatusCode::NOT_FOUND.into_response() }),
        )
        .with_state(site.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    site
}

/// The test's AppState, fetching with a client that may reach the local
/// stand-in, as an operator allows a private network, which the default
/// SSRF-guarded one rightly refuses.
fn local_state(ctx: &TestContext) -> eunha::state::AppState {
    let mut state = ctx.state.clone();
    state.fetch = ojak::client::Client::new(ojak::client::ClientConfig {
        allow_private: vec!["127.0.0.0/8".parse().unwrap(), "::1/128".parse().unwrap()],
        ..ojak::client::ClientConfig::default()
    })
    .unwrap();
    state
}

async fn status_json(ctx: &TestContext, id: &str, token: Option<&str>) -> Value {
    ctx.api
        .get(&format!("/api/v1/statuses/{id}"), token)
        .await
        .json()
        .await
        .unwrap()
}

/// An OpenGraph page, reached through a redirect, becomes a card kept under
/// the URL the redirect ended at, with its image stored where Paperclip
/// keeps it, and the post shows the card under the URL it linked to.
#[tokio::test]
async fn test_opengraph_card_through_a_redirect() {
    let ctx = TestContext::new("cards-og").await;
    let site = spawn_site(&format!("@alice@{}", ctx.domain)).await;
    let state = local_state(&ctx);
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "worth reading", "public")
        .await;
    let status_id: i64 = status["id"].as_str().unwrap().parse().unwrap();
    let short = format!("{}/short", site.base);

    let card_id = eunha::preview_card::fetch_link_card(&state, status_id, Some(short.clone()))
        .await
        .expect("a card is attached");

    let row = sqlx::query!(
        r#"SELECT url, title, description, type AS card_type, link_type, language,
                  provider_name, author_name, image_description, published_at,
                  image_file_name, image_content_type, image_file_size,
                  image_storage_schema_version, image_updated_at, width, height, blurhash,
                  author_account_id, unverified_author_account_id, html
           FROM preview_cards WHERE id = $1"#,
        card_id
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(row.url, format!("{}/article", site.base));
    assert_eq!(row.title, "A long read & more");
    assert_eq!(row.description, "What happened");
    assert_eq!(row.card_type, 0);
    assert_eq!(row.link_type, Some(1));
    assert_eq!(row.language.as_deref(), Some("ko"));
    assert_eq!(row.provider_name, "The Daily");
    assert_eq!(row.author_name, "Ann Writer");
    assert_eq!(row.image_description, "The cover");
    assert_eq!(
        row.published_at.map(|t| t.to_string()).as_deref(),
        Some("2024-02-03 04:05:06")
    );
    assert_eq!(row.html, "");
    let file_name = row.image_file_name.expect("the image was stored");
    assert!(
        file_name.ends_with(".png") && file_name.len() == 20,
        "{file_name}"
    );
    assert_eq!(row.image_content_type.as_deref(), Some("image/png"));
    assert!(row.image_file_size.unwrap() > 0);
    assert_eq!(row.image_storage_schema_version, Some(1));
    assert!(row.image_updated_at.is_some());
    assert_eq!(
        (row.width, row.height),
        (640, 360),
        "a link takes the size of its stored image, shrunk to 230,400 pixels"
    );
    assert!(row.blurhash.is_some());
    // Alice is local and names no attribution domains: the card is hers to
    // claim, not hers yet.
    assert_eq!(row.author_account_id, None);
    assert_eq!(
        row.unverified_author_account_id,
        Some(ctx.alice_id.parse().unwrap())
    );

    let joined: Option<String> = sqlx::query_scalar!(
        "SELECT url FROM preview_cards_statuses WHERE status_id = $1",
        status_id
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(joined.as_deref(), Some(short.as_str()));

    let shown = status_json(&ctx, &status_id.to_string(), Some(&ctx.alice_token)).await;
    let card = &shown["card"];
    assert_eq!(card["url"], short, "the link as the post gave it");
    assert_eq!(card["title"], "A long read & more");
    assert_eq!(card["language"], "ko");
    assert_eq!(card["type"], "link");
    assert_eq!(card["image_description"], "The cover");
    assert_eq!(card["published_at"], "2024-02-03T04:05:06.000Z");
    assert_eq!(card["width"], 640);
    assert!(card["blurhash"].is_string());
    let image = card["image"].as_str().expect("an image URL");
    assert!(
        image.ends_with(&format!(
            "/cache/preview_cards/images/000/000/{card_id:03}/original/{file_name}"
        )),
        "{image}"
    );
    assert_eq!(
        card["authors"],
        json!([{"name": "Ann Writer", "url": "", "account": null}])
    );
    assert_eq!(card["missing_attribution"], true);

    let anonymous = status_json(&ctx, &status_id.to_string(), None).await;
    assert!(
        anonymous["card"].get("missing_attribution").is_none(),
        "only a signed-in viewer is told"
    );
    let bob = status_json(&ctx, &status_id.to_string(), Some(&ctx.bob_token)).await;
    assert_eq!(bob["card"]["missing_attribution"], false);

    // A fresh card with its image is reused rather than fetched again, and a
    // post that already has a card keeps it.
    let hits = site.article_hits.load(Ordering::SeqCst);
    let second = ctx
        .api
        .post_status(&ctx.bob_token, "same link", "public")
        .await;
    let second_id: i64 = second["id"].as_str().unwrap().parse().unwrap();
    let article = format!("{}/article", site.base);
    assert_eq!(
        eunha::preview_card::fetch_link_card(&state, second_id, Some(article)).await,
        Some(card_id)
    );
    assert_eq!(site.article_hits.load(Ordering::SeqCst), hits);
    assert_eq!(
        eunha::preview_card::fetch_link_card(&state, status_id, Some(short)).await,
        None
    );
}

/// An account that lists the site among its attribution domains is the
/// card's author outright.
#[tokio::test]
async fn test_fediverse_creator_with_attribution_domain() {
    let ctx = TestContext::new("cards-attr").await;
    let site = spawn_site(&format!("@alice@{}", ctx.domain)).await;
    let state = local_state(&ctx);
    let alice: i64 = ctx.alice_id.parse().unwrap();
    sqlx::query!(
        "UPDATE accounts SET attribution_domains = ARRAY['localhost'] WHERE id = $1",
        alice
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let status = ctx.api.post_status(&ctx.bob_token, "read", "public").await;
    let status_id: i64 = status["id"].as_str().unwrap().parse().unwrap();

    let card_id = eunha::preview_card::fetch_link_card(
        &state,
        status_id,
        Some(format!("{}/article", site.base)),
    )
    .await
    .unwrap();
    let row = sqlx::query!(
        "SELECT author_account_id, unverified_author_account_id FROM preview_cards WHERE id = $1",
        card_id
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(row.author_account_id, Some(alice));
    assert_eq!(row.unverified_author_account_id, None);

    let shown = status_json(&ctx, &status_id.to_string(), Some(&ctx.alice_token)).await;
    let author = &shown["card"]["authors"][0];
    assert_eq!(author["name"], "Ann Writer");
    assert_eq!(author["account"]["id"], ctx.alice_id);
    assert_eq!(shown["card"]["missing_attribution"], false);
}

/// A page with an oEmbed endpoint becomes the card the endpoint describes:
/// its HTML sanitized, its size its own, and the endpoint remembered for the
/// domain.
#[tokio::test]
async fn test_oembed_video_card() {
    let ctx = TestContext::new("cards-oembed").await;
    let site = spawn_site("").await;
    let state = local_state(&ctx);
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "watch", "public")
        .await;
    let status_id: i64 = status["id"].as_str().unwrap().parse().unwrap();

    let card_id = eunha::preview_card::fetch_link_card(
        &state,
        status_id,
        Some(format!("{}/video", site.base)),
    )
    .await
    .expect("a card is attached");
    let row = sqlx::query!(
        r#"SELECT type AS card_type, title, author_name, author_url, provider_name,
                  provider_url, html, width, height, image_file_name, link_type
           FROM preview_cards WHERE id = $1"#,
        card_id
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(row.card_type, 2);
    assert_eq!(row.title, "A video");
    assert_eq!(row.author_name, "Vid Maker");
    assert_eq!(row.author_url, format!("{}/u/vid", site.base));
    assert_eq!(row.provider_name, "Tube");
    assert_eq!(row.provider_url, format!("{}/", site.base));
    assert_eq!(
        row.html,
        r#"<iframe src="https://player.example/embed/1" width="480" height="270" sandbox="allow-scripts allow-same-origin allow-popups allow-popups-to-escape-sandbox allow-forms"></iframe>"#
    );
    assert_eq!(
        (row.width, row.height),
        (480, 270),
        "a video keeps the size its provider gave"
    );
    assert!(row.image_file_name.is_some(), "the thumbnail is the image");
    assert_eq!(row.link_type, None, "oEmbed says nothing of articles");

    let mut redis = ctx.state.redis.clone();
    let cached: Option<String> = redis::cmd("GET")
        .arg(ctx.state.redis_keys.key("oembed_endpoint:localhost"))
        .query_async(&mut redis)
        .await
        .unwrap();
    let cached: Value = serde_json::from_str(&cached.expect("the endpoint is cached")).unwrap();
    assert_eq!(cached["format"], "json");
    assert_eq!(
        cached["endpoint"],
        format!("{}/oembed?format=json&url={{url}}", site.base)
    );
}

/// A page with neither a title nor an embed makes no card, and a post with
/// media gets none.
#[tokio::test]
async fn test_no_card_without_a_title_or_for_media() {
    let ctx = TestContext::new("cards-none").await;
    let site = spawn_site("").await;
    let state = local_state(&ctx);
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "plain", "public")
        .await;
    let status_id: i64 = status["id"].as_str().unwrap().parse().unwrap();
    let plain = format!("{}/plain", site.base);
    assert_eq!(
        eunha::preview_card::fetch_link_card(&state, status_id, Some(plain.clone())).await,
        None
    );
    let count: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM preview_cards WHERE url = $1"#,
        plain
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        eunha::preview_card::fetch_link_card(
            &state,
            status_id,
            Some(format!("{}/missing", site.base))
        )
        .await,
        None
    );

    let alice: i64 = ctx.alice_id.parse().unwrap();
    sqlx::query!(
        "INSERT INTO media_attachments (id, account_id, status_id, type, created_at, updated_at)
         VALUES ($1, $2, $3, 0, now(), now())",
        eunha::snowflake::next_id(),
        alice,
        status_id
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        eunha::preview_card::fetch_link_card(
            &state,
            status_id,
            Some(format!("{}/article", site.base))
        )
        .await,
        None,
        "a post with media has no card"
    );
}
