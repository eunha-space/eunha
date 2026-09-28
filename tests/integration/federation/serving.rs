//! What changed when serving moved to ojak: content negotiation, cursor
//! pages, collections named by the account's own URI, and discovery.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

async fn numeric(ctx: &TestContext) {
    sqlx::query("UPDATE accounts SET id_scheme = 1 WHERE id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
}

/// The path and query of a URL the server handed out, to ask it again.
fn path_of(url: &str) -> String {
    let url = url::Url::parse(url).unwrap();
    match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    }
}

/// An actor's or a status's URI opened in a browser leads to its page, as
/// in Mastodon; only a request for ActivityPub gets the document.
#[tokio::test]
async fn test_a_browser_is_sent_to_the_page() {
    let ctx = TestContext::new("serving-browser").await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for (path, page) in [
        ("/users/alice".to_owned(), "/@alice".to_owned()),
        (format!("/ap/users/{}", ctx.alice_id), "/@alice".to_owned()),
        ("/users/alice/statuses/1".to_owned(), "/@alice/1".to_owned()),
    ] {
        let resp = client
            .get(ctx.api.url(&path))
            .header("host", &ctx.domain)
            .header("accept", "text/html")
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_redirection(), "{path}: {}", resp.status());
        assert_eq!(resp.headers()["location"], page.as_str(), "{path}");
    }
    let resp = ctx.api.ap_get("/users/alice", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["vary"], "Accept");
}

/// The outbox links its pages by cursor, newest first, each item the
/// `Create` that posted it.
#[tokio::test]
async fn test_the_outbox_is_paged_by_cursor() {
    let ctx = TestContext::new("serving-outbox").await;
    for text in ["first", "second"] {
        let resp = ctx
            .api
            .post_json(
                "/api/v1/statuses",
                Some(&ctx.alice_token),
                &json!({ "status": text, "visibility": "public" }),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let outbox: Value = ctx
        .api
        .ap_get("/users/alice/outbox", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(outbox["type"], "OrderedCollection");
    assert_eq!(outbox["totalItems"], 2);
    let page: Value = ctx
        .api
        .ap_get(&path_of(outbox["first"].as_str().unwrap()), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(page["type"], "OrderedCollectionPage");
    assert_eq!(page["partOf"], outbox["id"]);
    let items = page["orderedItems"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|item| item["type"] == "Create"));
    assert!(items[0]["object"]["content"]
        .as_str()
        .unwrap()
        .contains("second"));

    // Past the oldest status there is nothing.
    let older: Value = ctx
        .api
        .ap_get(&path_of(page["next"].as_str().unwrap()), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(older["orderedItems"], json!([]));
}

/// Mastodon serves an account under both of its URI schemes, and names its
/// collections by the one it uses, whichever it was asked under.
#[tokio::test]
async fn test_collections_are_named_by_the_accounts_own_uri() {
    let ctx = TestContext::new("serving-own-uri").await;
    numeric(&ctx).await;
    let own = format!("https://{}/ap/users/{}", ctx.domain, ctx.alice_id);
    for suffix in [
        "/followers",
        "/following",
        "/outbox",
        "/collections/featured",
    ] {
        let collection: Value = ctx
            .api
            .ap_get(&format!("/users/alice{suffix}"), None)
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(collection["id"], format!("{own}{suffix}"), "{suffix}");
    }
    let actor: Value = ctx
        .api
        .ap_get("/users/alice", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(actor["id"], own);
}

/// An account that hides its collections shows how many and not who.
#[tokio::test]
async fn test_hidden_collections_show_only_their_count() {
    let ctx = TestContext::new("serving-hidden").await;
    sqlx::query("UPDATE accounts SET hide_collections = true WHERE id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let followers: Value = ctx
        .api
        .ap_get("/users/alice/followers", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(followers["totalItems"], 0);
    assert_eq!(followers.get("first"), None);
}

/// The instance actor answers WebFinger as `acct:{domain}@{domain}`, as
/// Mastodon's does, and NodeInfo is served in both versions.
#[tokio::test]
async fn test_discovery_covers_the_instance_actor_and_nodeinfo() {
    let ctx = TestContext::new("serving-discovery").await;
    let resource = format!("acct:{0}@{0}", ctx.domain);
    let jrd: Value = ctx
        .api
        .get(
            &format!(
                "/.well-known/webfinger?resource={}",
                urlencoding::encode(&resource)
            ),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(jrd["subject"], resource.as_str());
    assert_eq!(
        jrd["links"][0]["href"],
        format!("https://{}/actor", ctx.domain)
    );

    let alice: Value = ctx
        .api
        .get(
            &format!("/.well-known/webfinger?resource=acct:alice@{}", ctx.domain),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    let links = alice["links"].as_array().unwrap();
    assert!(links
        .iter()
        .any(|link| link["rel"] == "http://ostatus.org/schema/1.0/subscribe"));

    let links: Value = ctx
        .api
        .get("/.well-known/nodeinfo", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(links["links"].as_array().unwrap().len(), 2);
    let nodeinfo: Value = ctx
        .api
        .get("/nodeinfo/2.1", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(nodeinfo["software"]["name"], "eunha");
    assert_eq!(nodeinfo["version"], "2.1");
}
