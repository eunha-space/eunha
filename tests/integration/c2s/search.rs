use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

/// Search by username finds the matching account.
#[tokio::test]
async fn test_search_accounts() {
    let ctx = TestContext::new("search-acct").await;

    let resp = ctx
        .api
        .get(
            "/api/v2/search?q=alice&type=accounts",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    let accounts = body["accounts"].as_array().unwrap();
    assert!(accounts
        .iter()
        .any(|a| a["username"].as_str() == Some("alice")));
}

/// Without Elasticsearch, posts are not searched by their text at all
/// (`SearchService#status_searchable?` is `Chewy.enabled? && ...`): only a
/// URL resolves to one. tests/integration/c2s/search_elasticsearch.rs covers
/// post search with Elasticsearch.
#[tokio::test]
async fn test_search_statuses_need_elasticsearch() {
    let ctx = TestContext::new("search-status").await;

    ctx.api
        .post_status(&ctx.alice_token, "uniqueterm12345", "public")
        .await;

    for path in [
        "/api/v2/search?q=uniqueterm12345&type=statuses",
        "/api/v2/search?q=uniqueterm12345",
    ] {
        let resp = ctx.api.get(path, Some(&ctx.alice_token)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = resp.json().await.unwrap();
        assert!(
            body["statuses"].as_array().unwrap().is_empty(),
            "{path}: no post search without Elasticsearch",
        );
    }
}

/// Search without a type param returns accounts, statuses, and hashtags.
#[tokio::test]
async fn test_search_all_types() {
    let ctx = TestContext::new("search-all").await;

    ctx.api
        .post_status(&ctx.alice_token, "searching #alltype999 here", "public")
        .await;

    let body: Value = ctx
        .api
        .get("/api/v2/search?q=alltype999", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();

    for key in ["accounts", "statuses", "hashtags", "collections"] {
        assert!(body[key].is_array(), "{key} missing from search result");
    }
    assert_eq!(body["hashtags"][0]["name"], "alltype999");
}

/// `params.require(:q)`: a missing or blank query is a 400, and
/// `require_valid_pagination_options!` refuses a negative limit or offset.
#[tokio::test]
async fn test_search_parameter_validation() {
    let ctx = TestContext::new("search-params").await;

    for path in [
        "/api/v2/search",
        "/api/v2/search?q=",
        "/api/v2/search?q=%20%20",
        "/api/v2/search?q=alice&limit=-1",
        "/api/v2/search?q=alice&type=accounts&offset=-1",
    ] {
        let resp = ctx.api.get(path, Some(&ctx.alice_token)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{path}");
    }

    // `limit=0` is a valid request for nothing.
    let body: Value = ctx
        .api
        .get("/api/v2/search?q=alice&limit=0", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(body["accounts"].as_array().unwrap().is_empty());
}

/// `SearchService`: the offset applies only to a search of one type — a
/// mixed search always starts from the first result.
#[tokio::test]
async fn test_search_offset_needs_a_type() {
    let ctx = TestContext::new("search-offset").await;

    let mixed: Value = ctx
        .api
        .get("/api/v2/search?q=alice&offset=5", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        mixed["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["username"] == "alice"),
        "an untyped search ignores the offset: {mixed}",
    );

    let typed: Value = ctx
        .api
        .get(
            "/api/v2/search?q=alice&type=accounts&offset=5",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        typed["accounts"].as_array().unwrap().is_empty(),
        "a typed search pages past what there is: {typed}",
    );
}

/// `AccountSearchService::MIN_QUERY_LENGTH`: an anonymous query shorter than
/// three characters ranks nothing, a signed-in one does, and anonymous
/// `following` is ignored rather than refused.
#[tokio::test]
async fn test_search_accounts_anonymous_rules() {
    let ctx = TestContext::new("search-anon-acct").await;

    let anon: Value = ctx
        .api
        .get("/api/v2/search?q=al&type=accounts", None)
        .await
        .json()
        .await
        .unwrap();
    assert!(anon["accounts"].as_array().unwrap().is_empty(), "{anon}");

    let signed_in: Value = ctx
        .api
        .get("/api/v2/search?q=al&type=accounts", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        signed_in["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["username"] == "alice"),
        "{signed_in}",
    );

    let resp = ctx
        .api
        .get("/api/v2/search?q=alice&type=accounts&following=true", None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["accounts"][0]["username"], "alice");
}

/// The exact match for a complete handle comes first and is found whatever
/// state the account is in (`Account.find_local`), while the ranked results
/// leave suspended accounts out.
#[tokio::test]
async fn test_search_exact_match_for_a_complete_handle() {
    let ctx = TestContext::new("search-exact").await;

    let handle = format!("bob@{}", ctx.domain);
    let body: Value = ctx
        .api
        .get(
            &format!("/api/v2/search?q=%40{handle}&type=accounts"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["accounts"][0]["id"], ctx.bob_id.as_str(), "{body}");

    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();

    let exact: Value = ctx
        .api
        .get(
            &format!("/api/v2/search?q={handle}&type=accounts"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(exact["accounts"][0]["id"], ctx.bob_id.as_str(), "{exact}");

    let ranked: Value = ctx
        .api
        .get("/api/v2/search?q=bob&type=accounts", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        ranked["accounts"].as_array().unwrap().is_empty(),
        "{ranked}"
    );
}

/// GET /api/v2/search?following=true only returns accounts the viewer follows.
#[tokio::test]
async fn test_search_following_filter() {
    let ctx = TestContext::new("search-following").await;

    // Without following Bob, searching with following=true should return no results.
    let no_follow: Value = ctx
        .api
        .get(
            "/api/v2/search?q=bob&type=accounts&following=true",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        no_follow["accounts"].as_array().unwrap().is_empty(),
        "following=true should return empty when not following bob",
    );

    // Now follow Bob.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let after_follow: Value = ctx
        .api
        .get(
            "/api/v2/search?q=bob&type=accounts&following=true",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        after_follow["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())),
        "following=true should include bob after following",
    );
}

/// GET /api/v2/search?type=hashtags finds tags created by posting.
#[tokio::test]
async fn test_search_hashtags() {
    let ctx = TestContext::new("search-hash").await;

    ctx.api
        .post_status(&ctx.alice_token, "I enjoy #searchhash999", "public")
        .await;

    let body: Value = ctx
        .api
        .get(
            "/api/v2/search?q=searchhash999&type=hashtags",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let hashtags = body["hashtags"].as_array().unwrap();
    assert!(
        hashtags
            .iter()
            .any(|t| t["name"].as_str() == Some("searchhash999")),
        "hashtag not found in search results",
    );
}

/// `Tag.search_for` matches the normalized name (`HashtagNormalizer`), and
/// `REST::TagSerializer` says whether a signed-in user follows and features
/// each tag.
#[tokio::test]
async fn test_search_hashtags_normalized_with_relationships() {
    let ctx = TestContext::new("search-hash-norm").await;

    ctx.api
        .post_status(&ctx.alice_token, "I enjoy #Blahajnorm", "public")
        .await;
    ctx.api
        .post_json(
            "/api/v1/tags/blahajnorm/follow",
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // Full-width, accented and with a leading hash: all the same tag.
    for q in [
        "%23BL%C3%85HAJ",
        "%EF%BC%A2%EF%BD%8C%EF%BD%81%EF%BD%88%EF%BD%81%EF%BD%8A",
    ] {
        let body: Value = ctx
            .api
            .get(
                &format!("/api/v2/search?q={q}&type=hashtags"),
                Some(&ctx.alice_token),
            )
            .await
            .json()
            .await
            .unwrap();
        let tag = &body["hashtags"][0];
        assert!(
            tag["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case("blahajnorm")),
            "{q}: {body}"
        );
        assert_eq!(tag["following"], true, "{body}");
        assert_eq!(tag["featuring"], false, "{body}");
        assert!(tag["history"].is_array());
    }

    let anon: Value = ctx
        .api
        .get("/api/v2/search?q=blahajnorm&type=hashtags", None)
        .await
        .json()
        .await
        .unwrap();
    assert!(anon["hashtags"][0].get("following").is_none(), "{anon}");
}

/// Anonymous search cannot paginate (Mastodon returns 401 when offset/min_id/
/// max_id are supplied without a token).
#[tokio::test]
async fn test_search_anonymous_pagination_unauthorized() {
    let ctx = TestContext::new("search-anon-page").await;

    let resp = ctx.api.get("/api/v2/search?q=alice&offset=20", None).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "anon pagination must be 401"
    );

    // Without pagination, anonymous search still works.
    let resp = ctx.api.get("/api/v2/search?q=alice", None).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "anon search without pagination should be 200"
    );
}

/// Anonymous search cannot resolve remote resources (Mastodon returns 401 when
/// resolve=true without a token).
#[tokio::test]
async fn test_search_anonymous_resolve_unauthorized() {
    let ctx = TestContext::new("search-anon-resolve").await;

    let resp = ctx
        .api
        .get("/api/v2/search?q=alice&resolve=true", None)
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "anon resolve must be 401"
    );
}

/// A URL is resolved rather than searched for, and a status's own `url`
/// resolves back to it — `ResolveURLService#process_local_url`, which routes a
/// URL on our own domain by its shape instead of fetching it.
#[tokio::test]
async fn test_search_resolves_a_local_status_url() {
    let ctx = TestContext::new("search-url-status").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "findable by its address", "public")
        .await;
    let url = status["url"].as_str().expect("status has a url");

    let body: Value = ctx
        .api
        .get(
            &format!("/api/v2/search?q={url}&resolve=true"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    let statuses = body["statuses"].as_array().unwrap();
    assert_eq!(statuses.len(), 1, "a URL resolves to exactly one thing");
    assert_eq!(statuses[0]["id"], status["id"]);
    assert!(
        body["accounts"].as_array().unwrap().is_empty(),
        "a URL query runs no other search",
    );
}

/// A local status resolves from its ActivityPub URI as well as its web URL:
/// both are routes we serve. The URI is built here rather than read off the
/// status, because `convert::local_domain` is a process-wide `OnceLock` and a
/// test process runs many instances — the serialized `uri` carries whichever
/// domain was initialised first, while `url` comes from the row.
#[tokio::test]
async fn test_search_resolves_a_local_status_uri() {
    let ctx = TestContext::new("search-uri-status").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "findable by its uri", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    // Both actor-URI schemes: `username_ap_id`, and the numeric one whose
    // sub-resources hang off `/ap/users/{account_id}`.
    for uri in [
        format!("https://{}/users/alice/statuses/{id}", ctx.domain),
        format!(
            "https://{}/ap/users/{}/statuses/{id}",
            ctx.domain, ctx.alice_id
        ),
    ] {
        let body: Value = ctx
            .api
            .get(
                &format!("/api/v2/search?q={uri}&resolve=true"),
                Some(&ctx.alice_token),
            )
            .await
            .json()
            .await
            .unwrap();

        assert_eq!(body["statuses"][0]["id"], status["id"], "{uri}");
    }
}

/// A profile URL resolves to the account.
#[tokio::test]
async fn test_search_resolves_a_local_account_url() {
    let ctx = TestContext::new("search-url-acct").await;

    let url = format!("https://{}/@alice", ctx.domain);
    let body: Value = ctx
        .api
        .get(
            &format!("/api/v2/search?q={url}&resolve=true"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    let accounts = body["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["id"], ctx.alice_id);
}

/// A local URL that names no status — a profile's sub-page, or an id that is
/// not one — resolves to nothing rather than to whatever the digits happen to
/// match.
#[tokio::test]
async fn test_search_url_that_names_nothing_resolves_to_nothing() {
    let ctx = TestContext::new("search-url-nothing").await;

    for path in ["@alice/following", "@alice/media", "tags/art"] {
        let url = format!("https://{}/{path}", ctx.domain);
        let body: Value = ctx
            .api
            .get(
                &format!("/api/v2/search?q={url}&resolve=true"),
                Some(&ctx.alice_token),
            )
            .await
            .json()
            .await
            .unwrap();
        assert!(
            body["statuses"].as_array().unwrap().is_empty()
                && body["accounts"].as_array().unwrap().is_empty(),
            "{url} should resolve to nothing",
        );
    }
}

/// `SearchService#url_query?` is `@resolve && url?`: without `resolve` a URL is
/// an ordinary query, and full-text search does not match a status by its
/// address.
#[tokio::test]
async fn test_search_url_without_resolve_is_an_ordinary_query() {
    let ctx = TestContext::new("search-url-noresolve").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "not findable by address", "public")
        .await;
    let url = status["url"].as_str().unwrap();

    let body: Value = ctx
        .api
        .get(&format!("/api/v2/search?q={url}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();

    assert!(
        body["statuses"].as_array().unwrap().is_empty(),
        "an unresolved URL query should not resolve the URL",
    );
}

/// Resolving a URL does not hand over a status its viewer may not see:
/// `ResolveURLService` authorizes what it resolved against the caller.
#[tokio::test]
async fn test_search_url_of_a_private_status_is_authorized() {
    let ctx = TestContext::new("search-url-private").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "for followers only", "private")
        .await;
    let url = status["url"].as_str().unwrap();

    let body: Value = ctx
        .api
        .get(
            &format!("/api/v2/search?q={url}&resolve=true"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["statuses"][0]["id"], status["id"],
        "the author can resolve her own private status",
    );

    let body: Value = ctx
        .api
        .get(
            &format!("/api/v2/search?q={url}&resolve=true"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        body["statuses"].as_array().unwrap().is_empty(),
        "a stranger who knows the address still may not read it",
    );
}

/// `Api::V1::Peers::SearchController`: domains that start with the
/// normalized query, ten at most, never a blocked one, and `null` for a
/// blank query.
#[tokio::test]
async fn test_peers_search() {
    let ctx = TestContext::new("search-peers").await;
    for (i, domain) in [
        "peer-one.example",
        "peer-two.example",
        "other.example",
        "peer-blocked.example",
    ]
    .iter()
    .enumerate()
    {
        sqlx::query(
            "INSERT INTO accounts (id, username, domain, uri, protocol, created_at, updated_at) \
             VALUES ($1, 'someone', $2, $3, 1, now(), now())",
        )
        .bind(9_000_000 + i as i64)
        .bind(domain)
        .bind(format!("https://{domain}/users/someone"))
        .execute(&ctx.db)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO domain_blocks (domain, severity, created_at, updated_at) \
         VALUES ('peer-blocked.example', 1, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let mut found: Vec<String> = ctx
        .api
        .get("/api/v1/peers/search?q=PEER-", None)
        .await
        .json()
        .await
        .unwrap();
    found.sort();
    assert_eq!(found, ["peer-one.example", "peer-two.example"]);

    let blank: Value = ctx
        .api
        .get("/api/v1/peers/search?q=", None)
        .await
        .json()
        .await
        .unwrap();
    assert!(blank.is_null(), "{blank}");
}

/// With Elasticsearch on, a search server that fails is an unrescued error,
/// as `Api::V1::Peers::SearchController` queries the index without a rescue;
/// accounts still fall back to the database as upstream's do.
#[tokio::test]
async fn test_peers_search_answers_500_when_the_cluster_fails() {
    let ctx = TestContext::with_instance_config("search-peers-down", |instance| {
        instance.elasticsearch.enabled = true;
        instance.elasticsearch.host = "http://127.0.0.1".into();
        // Nothing listens on the discard port.
        instance.elasticsearch.port = 9;
    })
    .await;
    let resp = ctx.api.get("/api/v1/peers/search?q=peer", None).await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        json!({ "status": 500, "error": "Internal Server Error" })
    );

    let resp = ctx
        .api
        .get(
            "/api/v2/search?q=alice&type=accounts",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}
