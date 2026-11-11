//! An instance whose handles are on one domain and whose pages on another:
//! Mastodon's `LOCAL_DOMAIN` and `WEB_DOMAIN`, eunha's `domain` and
//! `web_domain`.

use serde_json::Value;

use crate::helpers::{ApiClient, TestContext};

/// A context whose handles are `@user@{ctx.domain}` and whose URLs are on
/// `social.{ctx.domain}`, with that web domain. Alice and bob were seeded
/// with their ids on the local domain; they are moved to the web domain, as
/// an instance that always had one would have given them.
async fn split(label: &str) -> (TestContext, String) {
    let ctx = TestContext::with_instance_config(label, |instance| {
        instance.web_domain = Some(format!("social.{}", instance.local_domain));
    })
    .await;
    let web = format!("social.{}", ctx.domain);
    sqlx::query(
        "UPDATE accounts SET uri = replace(uri, $1, $2), url = replace(url, $1, $2),
                inbox_url = replace(inbox_url, $1, $2), outbox_url = replace(outbox_url, $1, $2)
          WHERE domain IS NULL",
    )
    .bind(format!("https://{}/", ctx.domain))
    .bind(format!("https://{web}/"))
    .execute(&ctx.db)
    .await
    .unwrap();
    (ctx, web)
}

async fn webfinger(api: &ApiClient, resource: &str) -> reqwest::Response {
    api.get(
        &format!(
            "/.well-known/webfinger?resource={}",
            urlencoding::encode(resource)
        ),
        None,
    )
    .await
}

/// WebFinger answers for a handle on either domain, on either host, names
/// the account by its handle on the local domain, and points at its actor on
/// the web domain.
#[tokio::test]
async fn webfinger_names_the_handle_on_the_local_domain() {
    let (ctx, web) = split("web-domain-webfinger").await;
    let on_web = ApiClient::new(ctx.api.base_url.clone(), web.clone());

    for api in [&ctx.api, &on_web] {
        for resource in [
            format!("acct:alice@{}", ctx.domain),
            format!("acct:alice@{web}"),
            format!("https://{web}/users/alice"),
            format!("https://{web}/@alice"),
        ] {
            let resp = webfinger(api, &resource).await;
            assert_eq!(resp.status(), 200, "{resource} on {}", api.host);
            let jrd: Value = resp.json().await.unwrap();
            assert_eq!(
                jrd["subject"],
                format!("acct:alice@{}", ctx.domain),
                "{resource}"
            );
            assert_eq!(
                jrd["links"][0]["href"],
                format!("https://{web}/users/alice"),
                "{resource}"
            );
        }
    }

    // `WebfingerResource#account_from_acct`: either domain as a username
    // names the instance actor.
    for username in [&ctx.domain, &web] {
        let jrd: Value = webfinger(&ctx.api, &format!("acct:{username}@{}", ctx.domain))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(jrd["links"][0]["href"], format!("https://{web}/actor"));
    }

    let resp = webfinger(&ctx.api, "acct:alice@elsewhere.invalid").await;
    assert_eq!(resp.status(), 404);

    // host-meta sends WebFinger to the web domain, whichever host it is
    // asked on.
    let xrd = ctx
        .api
        .get("/.well-known/host-meta", None)
        .await
        .text()
        .await
        .unwrap();
    assert!(
        xrd.contains(&format!("https://{web}/.well-known/webfinger")),
        "{xrd}"
    );
}

/// The actor's id and URLs are on the web domain; the handle it gives in
/// `webfinger` is on the local one.
#[tokio::test]
async fn the_actor_is_on_the_web_domain() {
    let (ctx, web) = split("web-domain-actor").await;
    let actor: Value = ctx
        .api
        .ap_get("/users/alice", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(actor["id"], format!("https://{web}/users/alice"));
    assert_eq!(actor["inbox"], format!("https://{web}/users/alice/inbox"));
    assert_eq!(actor["url"], format!("https://{web}/@alice"));
    assert_eq!(actor["webfinger"], format!("alice@{}", ctx.domain));
}

/// `InstancePresenter#domain` is the local domain; the streaming API and
/// every link are on the web domain.
#[tokio::test]
async fn the_instance_is_named_by_its_local_domain() {
    let (ctx, web) = split("web-domain-instance").await;
    let v2: Value = ctx
        .api
        .get("/api/v2/instance", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v2["domain"], ctx.domain.as_str());
    assert_eq!(
        v2["configuration"]["urls"]["streaming"],
        format!("wss://{web}")
    );
    let v1: Value = ctx
        .api
        .get("/api/v1/instance", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v1["uri"], ctx.domain.as_str());
    assert_eq!(v1["urls"]["streaming_api"], format!("wss://{web}"));
}

/// A mention of `@bob@{local domain}` is a mention of the local bob
/// (`TagManager#local_domain?`), linked to his profile on the web domain.
#[tokio::test]
async fn a_mention_on_the_local_domain_is_local() {
    let (ctx, web) = split("web-domain-mention").await;

    let status = ctx
        .api
        .post_status(
            &ctx.alice_token,
            &format!("hello @bob@{} #news", ctx.domain),
            "public",
        )
        .await;
    let mentions = status["mentions"].as_array().unwrap();
    assert_eq!(mentions.len(), 1, "{status}");
    assert_eq!(mentions[0]["id"], ctx.bob_id.as_str());
    assert_eq!(mentions[0]["acct"], "bob");
    assert_eq!(mentions[0]["url"], format!("https://{web}/@bob"));
    let content = status["content"].as_str().unwrap();
    assert!(
        content.contains(&format!("href=\"https://{web}/@bob\"")),
        "{content}"
    );
    assert!(
        content.contains(&format!("href=\"https://{web}/tags/news\"")),
        "{content}"
    );
    assert!(
        status["uri"]
            .as_str()
            .unwrap()
            .starts_with(&format!("https://{web}/users/alice/statuses/")),
        "{status}"
    );
}
