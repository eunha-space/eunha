//! Trends paginate by offset, and say so in a `Link` header.
//!
//! Mastodon's `Api::V1::Trends::*Controller` runs `insert_pagination_headers`,
//! so a client scrolling trends follows `rel="next"` exactly as it does on a
//! timeline. eunha accepted `offset` and `limit` but never emitted the header,
//! so a client had no way to ask for the second page.
//!
//! The conditions are Mastodon's, quirks included: `next` only when the page
//! came back full, and `prev` only when the offset is more than one page in —
//! which means no `prev` on the second page.

use crate::helpers::TestContext;

/// Every link the header carries, as `(rel, url)`.
fn links(value: &str) -> Vec<(String, String)> {
    value
        .split(',')
        .filter_map(|part| {
            let (url, rel) = part.trim().split_once(">;")?;
            let url = url.trim_start_matches('<').to_string();
            let rel = rel
                .trim()
                .trim_start_matches("rel=")
                .trim_matches('"')
                .to_string();
            Some((rel, url))
        })
        .collect()
}

async fn trends_link(ctx: &TestContext, query: &str) -> Option<String> {
    let response = ctx
        .api
        .get(
            &format!("/api/v1/trends/tags?{query}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(response.status().as_u16(), 200);
    response
        .headers()
        .get("link")
        .map(|v| v.to_str().unwrap().to_string())
}

/// A full page offers the next one.
#[tokio::test]
async fn test_a_full_page_of_trends_links_to_the_next() {
    let ctx = TestContext::new("trends-page-next").await;
    crate::helpers::open_trends(&ctx.db).await;

    // Three tags, asked for one at a time, so the page comes back full.
    crate::helpers::posted_by_crowd(&ctx, "trending #alpha #beta #gamma").await;
    crate::helpers::refresh_trends(&ctx).await;

    // A full page must produce the header; an empty response here would make
    // the rest of this test prove nothing.
    let tags: serde_json::Value = ctx
        .api
        .get("/api/v1/trends/tags?limit=1", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        tags.as_array().map(Vec::len),
        Some(1),
        "expected a full page of one tag: {tags}"
    );

    let header = trends_link(&ctx, "limit=1")
        .await
        .expect("a full page must carry a Link header");
    let links = links(&header);
    let next = links.iter().find(|(rel, _)| rel == "next");
    assert!(
        next.is_some(),
        "a full page should link to the next: {header}"
    );
    assert!(
        next.unwrap().1.contains("offset=1"),
        "next should advance by the limit: {header}"
    );
    assert!(
        !links.iter().any(|(rel, _)| rel == "prev"),
        "the first page should not link back: {header}"
    );
}

/// A short page is the last one, and offers nothing further.
#[tokio::test]
async fn test_a_short_page_of_trends_does_not_link_onward() {
    let ctx = TestContext::new("trends-page-last").await;
    crate::helpers::open_trends(&ctx.db).await;

    let header = trends_link(&ctx, "limit=40&offset=0").await;
    if let Some(header) = header {
        assert!(
            !links(&header).iter().any(|(rel, _)| rel == "next"),
            "a page short of the limit is the last: {header}"
        );
    }
}

/// `pagination_params(offset:)`: a link names the endpoint with the
/// request's `limit` only when it gave one, the new `offset`, and nothing
/// else of the query.
#[tokio::test]
async fn test_trends_links_keep_only_a_given_limit() {
    let ctx = TestContext::new("trends-page-limit").await;
    crate::helpers::open_trends(&ctx.db).await;
    for n in 0..25 {
        let card_id: i64 = sqlx::query_scalar(
            r#"INSERT INTO preview_cards (url, title, type, trendable, created_at, updated_at)
               VALUES ($1, $2, 0, true, now(), now()) RETURNING id"#,
        )
        .bind(format!("https://links.example/{n}"))
        .bind(format!("link {n}"))
        .fetch_one(&ctx.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO preview_card_trends (id, preview_card_id, allowed, score, rank)
             VALUES ($1, $1, true, $2, $3)",
        )
        .bind(card_id)
        .bind(100.0 - n as f64)
        .bind(n + 1)
        .execute(&ctx.db)
        .await
        .unwrap();
    }
    let header = |query: &'static str| {
        let ctx = &ctx;
        async move {
            let response = ctx
                .api
                .get(&format!("/api/v1/trends/links?{query}"), None)
                .await;
            links(response.headers()["link"].to_str().unwrap())
        }
    };

    let page = header("foo=bar").await;
    assert_eq!(page.len(), 1, "{page:?}");
    assert_eq!(page[0].0, "next");
    assert!(
        page[0].1.ends_with("/api/v1/trends/links?offset=10"),
        "{page:?}"
    );

    let page = header("limit=5&foo=bar&offset=10").await;
    assert_eq!(page[0].0, "next");
    assert!(
        page[0]
            .1
            .ends_with("/api/v1/trends/links?limit=5&offset=15"),
        "{page:?}"
    );
    assert_eq!(page[1].0, "prev");
    assert!(
        page[1].1.ends_with("/api/v1/trends/links?limit=5&offset=5"),
        "{page:?}"
    );
}
