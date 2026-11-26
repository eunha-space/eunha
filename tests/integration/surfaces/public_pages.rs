//! What a browser, a crawler, a feed reader or another site's embed gets from
//! the paths the web app used to swallow.

use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::TestContext;

/// `/health` says OK without the web app, whatever state the app is in.
#[tokio::test]
async fn health_is_plain_ok() {
    let ctx = TestContext::new("web-health").await;
    let resp = ctx.api.get("/health", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/plain"));
    assert_eq!(resp.text().await.unwrap(), "OK");
}

#[tokio::test]
async fn manifest_describes_the_instance() {
    let ctx = TestContext::new("web-manifest").await;
    for path in ["/manifest", "/manifest.json"] {
        let resp = ctx.api.get(path, None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["cache-control"], "max-age=180, public");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["display"], "standalone");
        assert_eq!(body["icons"].as_array().unwrap().len(), 9);
        assert_eq!(body["prefer_related_applications"], false);
        assert!(body.get("related_applications").is_none());
    }
}

#[tokio::test]
async fn old_paths_redirect_as_mastodon_routes_them() {
    let ctx = TestContext::new("web-redirects").await;
    for (path, status, location) in [
        (
            "/.well-known/change-password",
            301,
            "/account/password".to_owned(),
        ),
        (
            "/.well-known/proxy?uri=a%40b.example",
            301,
            "/authorize_interaction?uri=a%40b.example".to_owned(),
        ),
        (
            "/authorize_follow?acct=a%40b.example",
            301,
            "/authorize_interaction?acct=a%40b.example".to_owned(),
        ),
        (
            "/intent?uri=web%2Bmastodon%3A%2F%2Ffollow%3Furi%3Dacct%3Aa%40b.example",
            302,
            "/authorize_interaction?uri=a%40b.example".to_owned(),
        ),
        (
            "/intent?uri=web%2Bmastodon%3A%2F%2Fshare%3Ftext%3Dhi%2520there",
            302,
            "/share?text=hi%20there".to_owned(),
        ),
    ] {
        let resp = ctx.api.get(path, None).await;
        assert_eq!(resp.status().as_u16(), status, "{path}");
        assert_eq!(resp.headers()["location"], location.as_str(), "{path}");
    }
    for path in [
        "/intent",
        "/intent?uri=https%3A%2F%2Fexample.com",
        "/intent?uri=web%2Bmastodon%3A%2F%2Fnope",
    ] {
        let resp = ctx.api.get(path, None).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

/// The address oEmbed hands out answers, framed by anyone, with the post in it.
#[tokio::test]
async fn embed_page_draws_a_public_post() {
    let ctx = TestContext::new("web-embed").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "Embed <me> please", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let url = format!("https://{}/@alice/{id}", ctx.domain);
    let oembed: Value = ctx
        .api
        .get(
            &format!("/api/oembed?url={}", urlencoding::encode(&url)),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    let html = oembed["html"].as_str().unwrap();
    assert!(
        html.contains(&format!(r#"data-embed-url="{url}/embed""#)),
        "{html}"
    );
    assert!(html.contains("/embed.js"), "{html}");

    let resp = ctx.api.get(&format!("/@alice/{id}/embed"), None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("x-frame-options").is_none());
    assert_eq!(resp.headers()["cache-control"], "max-age=180, public");
    let page = resp.text().await.unwrap();
    assert!(page.contains("Embed &lt;me&gt; please"), "{page}");
    assert!(page.contains(r#"<meta name="robots" content="noindex" />"#));
    assert!(page.contains("setHeight"));
}

#[tokio::test]
async fn embed_page_refuses_what_is_hidden_or_boosted() {
    let ctx = TestContext::new("web-embed-refused").await;
    let private = ctx
        .api
        .post_status(&ctx.alice_token, "Followers only", "private")
        .await;
    let public = ctx
        .api
        .post_status(&ctx.alice_token, "Boost me", "public")
        .await;
    let boost: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{}/reblog", public["id"].as_str().unwrap()),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    for id in [
        private["id"].as_str().unwrap(),
        boost["id"].as_str().unwrap(),
        "999999999",
    ] {
        let resp = ctx.api.get(&format!("/@alice/{id}/embed"), None).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{id}");
    }
    let resp = ctx
        .api
        .get(
            &format!("/@bob/{}/embed", public["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn account_feed_carries_public_posts_and_leaves_the_rest_out() {
    let ctx = TestContext::new("web-rss-account").await;
    ctx.api
        .post_status(&ctx.alice_token, "A public <post> #rusty", "public")
        .await;
    ctx.api
        .post_status(&ctx.alice_token, "An unlisted post", "unlisted")
        .await;
    ctx.api
        .post_status(&ctx.alice_token, "A followers post", "private")
        .await;
    ctx.api
        .post_status(&ctx.alice_token, "A direct post", "direct")
        .await;

    let resp = ctx.api.get("/@alice.rss", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()["content-type"],
        "application/rss+xml; charset=utf-8"
    );
    assert_eq!(resp.headers()["cache-control"], "max-age=60, public");
    let feed = resp.text().await.unwrap();
    roxmltree::Document::parse(&feed).expect("a feed is well-formed XML");
    assert!(feed.contains(r#"<rss version="2.0""#), "{feed}");
    assert!(feed.contains("A public"), "{feed}");
    assert!(feed.contains("<category>rusty</category>"), "{feed}");
    assert!(feed.contains("An unlisted post"), "{feed}");
    assert!(!feed.contains("A followers post"), "{feed}");
    assert!(!feed.contains("A direct post"), "{feed}");

    let feed = ctx
        .api
        .get("/@alice/tagged/rusty.rss", None)
        .await
        .text()
        .await
        .unwrap();
    assert!(feed.contains("A public"), "{feed}");
    assert!(!feed.contains("An unlisted post"), "{feed}");

    let feed = ctx
        .api
        .get("/@alice.rss?limit=1", None)
        .await
        .text()
        .await
        .unwrap();
    assert_eq!(feed.matches("<item>").count(), 1);

    let resp = ctx.api.get("/@nobody.rss", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn account_feed_leaves_out_replies_to_others_until_asked() {
    let ctx = TestContext::new("web-rss-replies").await;
    let bobs = ctx
        .api
        .post_status(&ctx.bob_token, "From bob", "public")
        .await;
    let reply: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &serde_json::json!({
                "status": "@bob a reply to bob",
                "visibility": "public",
                "in_reply_to_id": bobs["id"],
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(reply["id"].is_string(), "{reply}");

    let feed = ctx.api.get("/@alice.rss", None).await.text().await.unwrap();
    assert!(!feed.contains("a reply to bob"), "{feed}");
    let feed = ctx
        .api
        .get("/@alice/with_replies.rss", None)
        .await
        .text()
        .await
        .unwrap();
    assert!(feed.contains("a reply to bob"), "{feed}");
}

#[tokio::test]
async fn tag_feed_carries_only_public_posts() {
    let ctx = TestContext::new("web-rss-tag").await;
    ctx.api
        .post_status(&ctx.alice_token, "Public #feedtag", "public")
        .await;
    ctx.api
        .post_status(&ctx.alice_token, "Unlisted #feedtag", "unlisted")
        .await;

    let resp = ctx.api.get("/tags/feedtag.rss", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["cache-control"], "max-age=0, public");
    let feed = resp.text().await.unwrap();
    roxmltree::Document::parse(&feed).expect("a feed is well-formed XML");
    assert!(feed.contains("<title>#feedtag</title>"), "{feed}");
    assert!(feed.contains("Public"), "{feed}");
    assert!(!feed.contains("Unlisted"), "{feed}");

    let resp = ctx.api.get("/tags/nosuchtagatall.rss", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A closed instance tells a visitor nothing at these addresses.
#[tokio::test]
async fn limited_federation_shows_visitors_nothing() {
    let ctx = TestContext::with_instance("web-limited", |instance| {
        instance.limited_federation_mode = true;
    })
    .await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "Inside only #closed", "public")
        .await;
    let id = status["id"].as_str().unwrap();
    for path in [
        "/@alice.rss".to_owned(),
        "/tags/closed.rss".to_owned(),
        format!("/@alice/{id}/embed"),
    ] {
        let resp = ctx.api.get(&path, None).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

/// The heads themselves, which need no built web app to check.
#[tokio::test]
async fn account_and_post_heads_say_what_crawlers_read() {
    use eunha::web::pages::{
        account_head, owner, readable_status, status_head, tag_head, Owner, Tab,
    };

    let ctx = TestContext::new("web-heads").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "A post with <brackets>", "public")
        .await;
    let id: i64 = status["id"].as_str().unwrap().parse().unwrap();

    let Owner::Found(account) = owner(&ctx.state, "ALICE").await.unwrap() else {
        panic!("alice has an account");
    };
    let head = account_head(&ctx.state, &account, &Tab::Posts).await;
    assert!(head
        .title
        .as_deref()
        .unwrap()
        .contains(&format!("@alice@{}", ctx.domain)));
    assert!(head.head.contains(r#"type="application/rss+xml""#));
    assert!(head
        .head
        .contains(&format!(r#"href="https://{}/@alice.rss""#, ctx.domain)));
    assert!(head
        .head
        .contains(r#"property="og:type" content="profile""#));
    assert!(head.link.as_deref().unwrap().contains(r#"rel="lrdd""#));

    let row = readable_status(&ctx.state, account.id, id)
        .await
        .unwrap()
        .unwrap();
    let entity = eunha::api::mastodon::statuses::serialize_status(&ctx.state, &row, None)
        .await
        .unwrap();
    let head = status_head(&ctx.state, &account, &row, &entity)
        .await
        .unwrap();
    assert!(head
        .head
        .contains(r#"property="og:type" content="article""#));
    assert!(
        head.head.contains("A post with &lt;brackets&gt;"),
        "{}",
        head.head
    );
    assert!(!head.head.contains("<brackets>"), "{}", head.head);
    assert!(head.head.contains("application/json+oembed"));
    assert!(head.head.contains(r#"<script type="application/ld+json">"#));
    assert!(head
        .head
        .contains(r#"property="twitter:card" content="summary""#));

    let head = tag_head(
        &ctx.state,
        "rusty",
        &format!("https://{}/tags/rusty", ctx.domain),
    )
    .await;
    assert!(head
        .head
        .contains(r#"<meta name="robots" content="noindex" />"#));
    assert!(head.head.contains("/tags/rusty.rss"));
}

/// The schema.org JSON-LD a head carries.
fn seo_schema(head: &str) -> Value {
    let start = head
        .find(r#"<script type="application/ld+json">"#)
        .expect("a schema")
        + r#"<script type="application/ld+json">"#.len();
    let end = start + head[start..].find("</script>").unwrap();
    serde_json::from_str(&head[start..end]).unwrap()
}

/// `SEO::ProfilePageSerializer` on a profile, and on a post a full author, the
/// post it replies to and the one it quotes, as Mastodon 5.0's do.
#[tokio::test]
async fn heads_carry_schema_org_profiles_replies_and_quotes() {
    use eunha::web::pages::{account_head, owner, readable_status, status_head, Owner, Tab};

    let ctx = TestContext::new("web-seo").await;
    let Owner::Found(account) = owner(&ctx.state, "alice").await.unwrap() else {
        panic!("alice has an account");
    };
    let head = account_head(&ctx.state, &account, &Tab::Posts).await;
    let profile = seo_schema(&head.head);
    assert_eq!(profile["@context"], "https://schema.org");
    assert_eq!(profile["@type"], "ProfilePage");
    assert!(profile["datePublished"].as_str().unwrap().ends_with('Z'));
    let person = &profile["mainEntity"];
    assert_eq!(person["@type"], "Person");
    assert_eq!(person["identifier"], format!("alice@{}", ctx.domain));
    assert_eq!(person["url"], format!("https://{}/@alice", ctx.domain));
    assert_eq!(
        person["agentInteractionStatistic"][0]["interactionType"],
        "https://schema.org/WriteAction"
    );
    assert!(person["image"].as_str().unwrap().starts_with("http"));

    let first = ctx
        .api
        .post_status(&ctx.alice_token, "The first", "public")
        .await;
    let first_id = first["id"].as_str().unwrap();
    let reply: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &serde_json::json!({
                "status": "A reply that quotes",
                "visibility": "public",
                "in_reply_to_id": first_id,
                "quoted_status_id": first_id,
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let id: i64 = reply["id"].as_str().unwrap().parse().unwrap();
    let row = readable_status(&ctx.state, account.id, id)
        .await
        .unwrap()
        .unwrap();
    let entity = eunha::api::mastodon::statuses::serialize_status(&ctx.state, &row, None)
        .await
        .unwrap();
    let head = status_head(&ctx.state, &account, &row, &entity)
        .await
        .unwrap();
    let posting = seo_schema(&head.head);
    let first_url = format!("https://{}/@alice/{first_id}", ctx.domain);
    assert_eq!(posting["@type"], "SocialMediaPosting");
    assert_eq!(
        posting["parentItem"],
        serde_json::json!({"@type": "SocialMediaPosting", "url": first_url})
    );
    assert_eq!(
        posting["sharedContent"],
        serde_json::json!({"@type": "SocialMediaPosting", "url": first_url})
    );
    assert_eq!(posting["author"]["@type"], "Person");
    assert_eq!(
        posting["author"]["agentInteractionStatistic"][0]["userInteractionCount"],
        2
    );
    assert!(posting["datePublished"].as_str().unwrap().ends_with('Z'));
    assert_eq!(posting["dateModified"], Value::Null);

    // Opted out of indexing, a profile carries no schema.
    sqlx::query("UPDATE users SET settings = '{\"noindex\":true}' WHERE account_id = $1")
        .bind(account.id)
        .execute(&ctx.db)
        .await
        .unwrap();
    let head = account_head(&ctx.state, &account, &Tab::Posts).await;
    assert!(
        !head.head.contains(r#"<script type="application/ld+json">"#),
        "{}",
        head.head
    );
}

/// Pages for what a visitor may not see answer as Mastodon's do, the web app
/// loading underneath so a signed-in member can still read it.
#[tokio::test]
async fn pages_answer_with_mastodons_statuses() {
    let ctx = TestContext::new("web-statuses").await;
    let private = ctx
        .api
        .post_status(&ctx.alice_token, "Followers only", "private")
        .await;
    let public = ctx
        .api
        .post_status(&ctx.alice_token, "Boost me", "public")
        .await;
    let boost: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{}/reblog", public["id"].as_str().unwrap()),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await
        .json()
        .await
        .unwrap();

    for path in [
        "/@nobody".to_owned(),
        format!("/@alice/{}", private["id"].as_str().unwrap()),
        "/@alice/999999999".to_owned(),
        "/tags/nosuchtagatall".to_owned(),
    ] {
        let resp = ctx.api.get(&path, None).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(resp.headers()["vary"], "Accept, Accept-Language, Cookie");
    }

    // A boost's page is the original's.
    let resp = ctx
        .api
        .get(&format!("/@bob/{}", boost["id"].as_str().unwrap()), None)
        .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert!(resp.headers()["location"]
        .to_str()
        .unwrap()
        .ends_with(&format!("/@alice/{}", public["id"].as_str().unwrap())));

    // Cached, and with its headers, whether or not the web app is built.
    let resp = ctx
        .api
        .get(&format!("/@alice/{}", public["id"].as_str().unwrap()), None)
        .await;
    assert_eq!(resp.headers()["cache-control"], "max-age=10, public");
    assert!(resp.headers()["link"]
        .to_str()
        .unwrap()
        .contains("activity+json"));
}
