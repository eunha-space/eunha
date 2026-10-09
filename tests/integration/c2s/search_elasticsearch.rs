//! Search with Elasticsearch or OpenSearch.
//!
//! These run only when `EUNHA_TEST_ELASTICSEARCH_URL` names a cluster
//! (`http://127.0.0.1:9200`, security off); without it each test returns at
//! once. docs/operating/search.md says how to start one. Every test gets
//! indexes of its own under a unique `prefix`, and deletes them when done.

use reqwest::StatusCode;
use serde_json::{json, Value};

use eunha::search::elasticsearch::{deploy, indexing, Index};

use crate::helpers::TestContext;

struct Search {
    ctx: TestContext,
    prefix: String,
}

impl Search {
    /// A context searching the test cluster, its indexes deployed; `None`
    /// when no cluster is configured.
    async fn new(label: &str) -> Option<Self> {
        let url = std::env::var("EUNHA_TEST_ELASTICSEARCH_URL").ok()?;
        let parsed = url::Url::parse(&url).expect("EUNHA_TEST_ELASTICSEARCH_URL is a URL");
        let prefix = format!(
            "eunhatest_{}_{}",
            label.replace('-', "_"),
            eunha::snowflake::next_id()
        );
        let host = format!(
            "{}://{}",
            parsed.scheme(),
            parsed.host_str().expect("a host")
        );
        let port = parsed.port_or_known_default().unwrap_or(9200);
        let index_prefix = prefix.clone();
        let ctx = TestContext::with_instance_config(label, move |instance| {
            instance.elasticsearch.enabled = true;
            instance.elasticsearch.host = host;
            instance.elasticsearch.port = port;
            instance.elasticsearch.prefix = Some(index_prefix);
        })
        .await;
        let search = Search { ctx, prefix };
        search.deploy().await;
        Some(search)
    }

    fn client(&self) -> &eunha::search::elasticsearch::Client {
        self.ctx.state.search.as_ref().expect("search is enabled")
    }

    async fn deploy(&self) -> deploy::Report {
        deploy::deploy(&self.ctx.state, &deploy::Options::default(), |_| {})
            .await
            .expect("deploy")
    }

    /// What the scheduler does once a minute, then a refresh so that the
    /// writes are searchable now rather than in thirty seconds.
    async fn index(&self) {
        indexing::drain(&self.ctx.state).await.expect("drain");
        indexing::sync_instances(&self.ctx.state)
            .await
            .expect("sync instances");
        self.refresh().await;
    }

    async fn refresh(&self) {
        for index in Index::ALL {
            self.client()
                .send(
                    reqwest::Method::POST,
                    &format!("{}/_refresh", self.client().index_name(index.base_name())),
                    None,
                )
                .await
                .expect("refresh");
        }
    }

    async fn search(&self, query: &str, token: Option<&str>) -> Value {
        let resp = self
            .ctx
            .api
            .get(
                &format!("/api/v2/search?q={}", urlencoding::encode(query)),
                token,
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK, "{query}");
        resp.json().await.unwrap()
    }

    async fn status_texts(&self, query: &str, token: &str) -> Vec<String> {
        self.search(query, Some(token)).await["statuses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["content"].as_str().unwrap_or("").to_string())
            .collect()
    }

    async fn set_indexable(&self, account_id: &str, indexable: bool) {
        let id: i64 = account_id.parse().unwrap();
        sqlx::query("UPDATE accounts SET indexable = $1 WHERE id = $2")
            .bind(indexable)
            .bind(id)
            .execute(&self.ctx.db)
            .await
            .unwrap();
        indexing::account_indexable_changed(&self.ctx.state, id).await;
    }

    async fn teardown(self) {
        for index in Index::ALL {
            let _ = self
                .client()
                .send(
                    reqwest::Method::DELETE,
                    &self.client().index_name(index.base_name()),
                    None,
                )
                .await;
        }
        let _ = &self.prefix;
    }
}

/// `in:` and `searchable_by`: an author finds her own posts, someone who
/// interacted with a post finds it, and a public post is found by everyone
/// only once its author is `indexable`.
#[tokio::test]
async fn test_post_search_privacy() {
    let Some(s) = Search::new("es-privacy").await else {
        return;
    };
    let ctx = &s.ctx;
    let public = ctx
        .api
        .post_status(&ctx.alice_token, "zebrafinch public words", "public")
        .await;
    ctx.api
        .post_status(&ctx.alice_token, "zebrafinch private words", "private")
        .await;
    s.index().await;

    let mine = s.status_texts("zebrafinch", &ctx.alice_token).await;
    assert_eq!(mine.len(), 2, "the author finds both: {mine:?}");
    assert!(
        s.status_texts("zebrafinch", &ctx.bob_token)
            .await
            .is_empty(),
        "a stranger finds nothing of an account that is not indexable"
    );

    // Bob favourites the public post: it is in his library now.
    ctx.api
        .post_json(
            &format!(
                "/api/v1/statuses/{}/favourite",
                public["id"].as_str().unwrap()
            ),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    s.index().await;
    let library = s.status_texts("zebrafinch", &ctx.bob_token).await;
    assert_eq!(library.len(), 1, "{library:?}");
    assert!(library[0].contains("public"));

    // Alice becomes indexable: her public post is in the public index.
    s.set_indexable(&ctx.alice_id, true).await;
    s.index().await;
    let public_only = s.status_texts("zebrafinch in:public", &ctx.bob_token).await;
    assert_eq!(public_only.len(), 1, "{public_only:?}");
    assert!(
        s.status_texts("zebrafinch private", &ctx.bob_token)
            .await
            .iter()
            .all(|t| !t.contains("private")),
        "a private post never reaches the public index"
    );

    // A block hides it again (`StatusFilter`).
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.alice_id),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert!(s
        .status_texts("zebrafinch", &ctx.bob_token)
        .await
        .is_empty());
    s.teardown().await;
}

/// The operators of `SearchQueryTransformer`.
#[tokio::test]
async fn test_post_search_operators() {
    let Some(s) = Search::new("es-operators").await else {
        return;
    };
    let ctx = &s.ctx;
    let first = ctx
        .api
        .post_status(&ctx.alice_token, "quokka sunrise #Marsupial", "public")
        .await;
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "quokka sunset reply",
                "visibility": "public",
                "in_reply_to_id": first["id"],
                "language": "de",
            }),
        )
        .await;
    ctx.api
        .post_status(&ctx.bob_token, "quokka from bob", "public")
        .await;
    s.set_indexable(&ctx.bob_id, true).await;
    s.index().await;
    let token = &ctx.alice_token;

    let count = |v: Vec<String>| v.len();
    assert_eq!(count(s.status_texts("quokka", token).await), 3);
    assert_eq!(count(s.status_texts("quokka from:me", token).await), 2);
    assert_eq!(
        count(
            s.status_texts(&format!("quokka from:bob@{}", ctx.domain), token)
                .await
        ),
        1
    );
    assert_eq!(count(s.status_texts("quokka from:nobody", token).await), 0);
    assert_eq!(count(s.status_texts("quokka is:reply", token).await), 1);
    assert_eq!(count(s.status_texts("quokka -is:reply", token).await), 2);
    assert_eq!(count(s.status_texts("quokka -sunset", token).await), 2);
    assert_eq!(count(s.status_texts("\"quokka sunrise\"", token).await), 1);
    assert_eq!(count(s.status_texts("#marsupial", token).await), 1);
    assert_eq!(count(s.status_texts("quokka language:de", token).await), 1);
    assert_eq!(count(s.status_texts("quokka in:library", token).await), 2);
    assert_eq!(
        count(s.status_texts("quokka before:2000-01-01", token).await),
        0
    );
    assert_eq!(
        count(s.status_texts("quokka after:2000-01-01", token).await),
        3
    );

    // `account_id` is `from:` that account; one that does not exist is 404.
    let resp = ctx
        .api
        .get(
            &format!(
                "/api/v2/search?q=quokka&type=statuses&account_id={}",
                ctx.bob_id
            ),
            Some(token),
        )
        .await;
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["statuses"].as_array().unwrap().len(), 1, "{body}");
    let resp = ctx
        .api
        .get(
            "/api/v2/search?q=quokka&type=statuses&account_id=1",
            Some(token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // An invalid date is `Date::Error`, a 422.
    let resp = ctx
        .api
        .get("/api/v2/search?q=quokka%20before:abc", Some(token))
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    // A query that does not parse finds nothing.
    assert_eq!(count(s.status_texts(": x", token).await), 0);
    // A clause the transformer raises on is an unrescued exception, a 500.
    for raises in ["%3Ablobcat%3A", "%22%22"] {
        let resp = ctx
            .api
            .get(&format!("/api/v2/search?q={raises}"), Some(token))
            .await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR, "{raises}");
    }

    // Deleted, it is gone from the index.
    let resp = ctx
        .api
        .delete(
            &format!("/api/v1/statuses/{}", first["id"].as_str().unwrap()),
            token,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    s.index().await;
    assert_eq!(count(s.status_texts("quokka sunrise", token).await), 0);
    s.teardown().await;
}

/// Accounts, hashtags and peers come from their indexes.
#[tokio::test]
async fn test_accounts_tags_and_peers() {
    let Some(s) = Search::new("es-accounts").await else {
        return;
    };
    let ctx = &s.ctx;
    ctx.api
        .post_status(&ctx.alice_token, "about #Wombatology", "public")
        .await;
    sqlx::query(
        "INSERT INTO accounts (id, username, domain, uri, protocol, created_at, updated_at) \
         VALUES (9100000, 'peer', 'wombat-peer.example', 'https://wombat-peer.example/u/peer', 1, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    // Imported by a fresh deploy, as `tootctl search deploy` would.
    let report = s.deploy().await;
    assert!(report.indexed > 0, "{report:?}");
    s.refresh().await;

    let body = s.search("bob", Some(&ctx.alice_token)).await;
    assert_eq!(body["accounts"][0]["username"], "bob", "{body}");
    let body = s.search("wombatolo", Some(&ctx.alice_token)).await;
    assert!(
        body["hashtags"][0]["name"]
            .as_str()
            .is_some_and(|n| n.eq_ignore_ascii_case("wombatology")),
        "{body}"
    );
    let peers: Value = ctx
        .api
        .get("/api/v1/peers/search?q=wombat", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(peers, json!(["wombat-peer.example"]));
    s.teardown().await;
}

/// A deploy creates the indexes as Mastodon would and, run again, finds
/// nothing to change; `--resume` carries on where an import stopped.
#[tokio::test]
async fn test_deploy_is_idempotent() {
    let Some(s) = Search::new("es-deploy").await else {
        return;
    };
    for index in Index::ALL {
        assert!(
            !deploy::specification_changed(s.client(), index)
                .await
                .unwrap(),
            "{} should match what was created",
            index.base_name()
        );
    }
    let mapping = s
        .client()
        .send(
            reqwest::Method::GET,
            &format!("{}/_mapping", s.client().index_name("accounts")),
            None,
        )
        .await
        .unwrap();
    assert!(mapping.to_string().contains("edge_ngram"), "{mapping}");

    // A mapping that differs is noticed, and the next deploy re-creates it.
    s.client()
        .send(
            reqwest::Method::PUT,
            &format!("{}/_mapping", s.client().index_name("tags")),
            Some(&json!({ "properties": { "extra": { "type": "keyword" } } })),
        )
        .await
        .unwrap();
    assert!(deploy::specification_changed(s.client(), Index::Tags)
        .await
        .unwrap());
    s.deploy().await;
    assert!(!deploy::specification_changed(s.client(), Index::Tags)
        .await
        .unwrap());

    let resumed = deploy::deploy(
        &s.ctx.state,
        &deploy::Options {
            resume: true,
            batch_size: 1,
            ..Default::default()
        },
        |_| {},
    )
    .await
    .unwrap();
    assert!(resumed.indexed > 0);
    s.teardown().await;
}
