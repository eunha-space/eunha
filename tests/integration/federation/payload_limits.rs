//! Activities inflated past what any server sends are refused at the inbox,
//! by ojak's payload limits, before eunha resolves a single mention.
//!
//! Modelled on a real troll post: thousands of `Mention`s of distinct actors,
//! every one also in `to`. About 2,500 fit under the megabyte an inbox reads;
//! Mastodon would fetch each actor it does not know, inline.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

#[tokio::test]
async fn test_a_flood_of_mentions_is_refused_before_anything_is_done() {
    let ctx = TestContext::new("payload-limits").await;
    let actor = "https://flood.invalid/users/troll";
    let mentioned: Vec<String> = (0..2_500)
        .map(|i| format!("https://s{}.invalid/users/u{i}", i % 300))
        .collect();
    let tag: Vec<Value> = mentioned
        .iter()
        .map(|href| json!({"type": "Mention", "href": href}))
        .collect();
    let activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://flood.invalid/notes/1/activity",
        "type": "Create",
        "actor": actor,
        "to": mentioned,
        "object": {
            "id": "https://flood.invalid/notes/1",
            "type": "Note",
            "attributedTo": actor,
            "content": "<p>flood</p>",
            "tag": tag,
            "to": mentioned,
        },
    });
    assert!(serde_json::to_vec(&activity).unwrap().len() < ojak::federation::MAX_INBOX_BODY);

    let resp = ctx.api.post_json("/inbox", None, &activity).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        resp.text().await.unwrap(),
        "refused: 2500 recipients, more than 1000"
    );

    let count = |sql: &'static str| {
        let db = ctx.db.clone();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        count("SELECT count(*) FROM statuses WHERE uri = 'https://flood.invalid/notes/1'").await,
        0
    );
    assert_eq!(
        count("SELECT count(*) FROM accounts WHERE domain LIKE 's%.invalid'").await,
        0,
        "no mentioned actor was resolved"
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM eunha.jobs \
             WHERE kind = 'ActivityPub::ProcessingWorker' OR kind LIKE '%Mention%'"
        )
        .await,
        0,
        "nothing queued"
    );
}

/// eunha allows 100 mentions, fewer than ojak's own 500: one more is refused,
/// and 100 go on to be authenticated (and, unsigned, refused for that).
#[tokio::test]
async fn test_more_than_a_hundred_mentions_are_refused() {
    let ctx = TestContext::new("payload-limits-mentions").await;
    let note = |mentions: usize| {
        let tag: Vec<Value> = (0..mentions)
            .map(|i| json!({"type": "Mention", "href": format!("https://s{i}.invalid/users/u{i}")}))
            .collect();
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("https://flood.invalid/notes/{mentions}/activity"),
            "type": "Create",
            "actor": "https://flood.invalid/users/troll",
            "to": "https://www.w3.org/ns/activitystreams#Public",
            "object": {
                "id": format!("https://flood.invalid/notes/{mentions}"),
                "type": "Note",
                "attributedTo": "https://flood.invalid/users/troll",
                "content": "<p>flood</p>",
                "tag": tag,
            },
        })
    };
    assert_eq!(eunha::federation::MAX_MENTIONS, 100);

    let resp = ctx.api.post_json("/inbox", None, &note(101)).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        resp.text().await.unwrap(),
        "refused: 101 mentions, more than 100"
    );
    let resp = ctx.api.post_json("/inbox", None, &note(100)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "within the limits");
}
