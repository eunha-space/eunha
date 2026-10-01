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
    for tag in ["alpha", "beta", "gamma"] {
        ctx.api
            .post_status(&ctx.alice_token, &format!("trending #{tag}"), "public")
            .await;
    }

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
