//! What each index holds for a row, as the `field` declarations in
//! *app/chewy/* compute it, and which rows the index scope admits.
//!
//! [`bulk_lines`] is Chewy's `import!(ids)`: a row in the index scope is
//! indexed, and an id that is not (deleted, suspended, no longer public, no
//! longer interacted with) is deleted from the index.

use std::sync::LazyLock;

use serde_json::{json, Value};

use super::Index;
use crate::state::AppState;

/// `DatetimeClampingConcern#clamp_date`, then the JSON form Chewy sends.
fn date(value: chrono::NaiveDateTime) -> String {
    let min = chrono::NaiveDate::from_ymd_opt(0, 1, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .unwrap_or_default();
    let max = chrono::NaiveDate::from_ymd_opt(9999, 12, 31)
        .and_then(|d| d.and_hms_opt(23, 59, 59))
        .unwrap_or_default();
    value
        .clamp(min, max)
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// `PlainTextFormatter#to_s`: a local text as it is; a remote one's HTML
/// with a line break after each run of `<br>` and `</p>`, its tags stripped
/// and entities decoded, less one trailing newline.
pub fn plain_text(text: &str, local: bool) -> String {
    static NEWLINE_TAGS: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?:<br />|<br>|</p>)+").unwrap());
    if local {
        return text.to_string();
    }
    let with_newlines = NEWLINE_TAGS.replace_all(text, "$0\n");
    let mut plain: String = scraper::Html::parse_fragment(&with_newlines)
        .root_element()
        .text()
        .collect();
    // `chomp`.
    if plain.ends_with("\r\n") {
        plain.truncate(plain.len() - 2);
    } else if plain.ends_with('\n') || plain.ends_with('\r') {
        plain.pop();
    }
    plain
}

#[derive(sqlx::FromRow)]
struct AccountRow {
    id: i64,
    username: String,
    domain: Option<String>,
    display_name: String,
    note: String,
    discoverable: Option<bool>,
    bot: bool,
    verified: bool,
    following_count: i64,
    followers_count: i64,
    last_status_at: chrono::NaiveDateTime,
}

/// `AccountsIndex`, for the rows of `Account.searchable` among `ids`.
async fn accounts(state: &AppState, ids: &[i64]) -> anyhow::Result<Vec<(i64, Value)>> {
    let rows = sqlx::query_as::<_, AccountRow>(
        "SELECT a.id, a.username, a.domain, a.display_name, a.note, a.discoverable, \
           coalesce(a.actor_type IN ('Application', 'Service'), false) AS bot, \
           EXISTS (SELECT 1 FROM jsonb_array_elements( \
                     CASE WHEN jsonb_typeof(a.fields) = 'array' THEN a.fields ELSE '[]'::jsonb END) f \
                   WHERE coalesce(f->>'verified_at', '') <> '') AS verified, \
           coalesce(s.following_count, 0) AS following_count, \
           coalesce(s.followers_count, 0) AS followers_count, \
           coalesce(s.last_status_at, a.created_at) AS last_status_at \
         FROM accounts a \
         LEFT JOIN account_stats s ON s.account_id = a.id \
         LEFT JOIN users u ON u.account_id = a.id \
         WHERE a.id = ANY($1) \
           AND a.suspended_at IS NULL AND a.moved_to_account_id IS NULL \
           AND (a.domain IS NOT NULL OR (u.approved AND u.confirmed_at IS NOT NULL))",
    )
    .bind(ids)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|a| {
            let discoverable = a.discoverable == Some(true);
            // `searchable_properties`.
            let mut properties = vec![];
            if a.bot {
                properties.push("bot");
            }
            if a.verified {
                properties.push("verified");
            }
            if discoverable {
                properties.push("discoverable");
            }
            let username = match &a.domain {
                Some(domain) => format!("{}@{domain}", a.username),
                None => a.username.clone(),
            };
            // `searchable_text`: only a discoverable account's bio.
            let text = discoverable.then(|| plain_text(&a.note, a.domain.is_none()));
            (
                a.id,
                json!({
                    "id": a.id,
                    "following_count": a.following_count,
                    "followers_count": a.followers_count,
                    "properties": properties,
                    "last_status_at": date(a.last_status_at),
                    "display_name": a.display_name,
                    "username": username,
                    "text": text,
                }),
            )
        })
        .collect())
}

#[derive(sqlx::FromRow)]
struct TagRow {
    id: i64,
    display_name: String,
    reviewed: bool,
    last_status_at: chrono::NaiveDateTime,
}

/// `TagsIndex`, for the rows of `Tag.listable` among `ids`.
async fn tags(state: &AppState, ids: &[i64]) -> anyhow::Result<Vec<(i64, Value)>> {
    let rows = sqlx::query_as::<_, TagRow>(
        "SELECT id, coalesce(display_name, name) AS display_name, \
           reviewed_at IS NOT NULL AS reviewed, \
           coalesce(last_status_at, created_at) AS last_status_at \
         FROM tags WHERE id = ANY($1) AND (listable = TRUE OR listable IS NULL)",
    )
    .bind(ids)
    .fetch_all(&state.db)
    .await?;
    let mut docs = Vec::with_capacity(rows.len());
    for t in rows {
        // `tag.history.aggregate(7.days.ago.to_date..0.days.ago.to_date).accounts`.
        let usage = crate::moderation::history::aggregate_accounts(state, "tags", t.id, 7).await;
        docs.push((
            t.id,
            json!({
                "name": t.display_name,
                "reviewed": t.reviewed,
                "usage": usage,
                "last_status_at": date(t.last_status_at),
            }),
        ));
    }
    Ok(docs)
}

#[derive(sqlx::FromRow)]
struct StatusRow {
    id: i64,
    account_id: i64,
    local: bool,
    text: String,
    spoiler_text: String,
    language: Option<String>,
    sensitive: bool,
    reply: bool,
    created_at: chrono::NaiveDateTime,
    poll_options: Option<Vec<String>>,
    media_descriptions: Vec<Option<String>>,
    media_types: Vec<i32>,
    tags: Vec<String>,
    with_card: bool,
    card_is_video: bool,
    with_quote: bool,
    searchable_by: Vec<i64>,
}

/// The statuses among `ids` in an index's scope, with what their documents
/// need. `public` is `PublicStatusesIndex`'s scope (`kept.indexable`), and
/// otherwise `StatusesIndex`'s (`kept.without_reblogs`).
async fn status_rows(
    state: &AppState,
    ids: &[i64],
    public: bool,
) -> anyhow::Result<Vec<StatusRow>> {
    Ok(sqlx::query_as::<_, StatusRow>(
        "SELECT s.id, s.account_id, coalesce(s.local, s.uri IS NULL) AS local, s.text, s.spoiler_text, \
           s.language, s.sensitive, s.in_reply_to_id IS NOT NULL AS reply, s.created_at, \
           p.options::text[] AS poll_options, \
           coalesce((SELECT array_agg(m.description ORDER BY \
                       coalesce(array_position(s.ordered_media_attachment_ids, m.id), 0), m.id) \
                     FROM media_attachments m WHERE m.status_id = s.id \
                       AND (s.ordered_media_attachment_ids IS NULL OR m.id = ANY(s.ordered_media_attachment_ids))), \
                    '{}') AS media_descriptions, \
           coalesce((SELECT array_agg(m.type) \
                     FROM media_attachments m WHERE m.status_id = s.id \
                       AND (s.ordered_media_attachment_ids IS NULL OR m.id = ANY(s.ordered_media_attachment_ids))), \
                    '{}') AS media_types, \
           coalesce((SELECT array_agg(coalesce(t.display_name, t.name)::text ORDER BY t.id) \
                     FROM statuses_tags st JOIN tags t ON t.id = st.tag_id WHERE st.status_id = s.id), \
                    '{}') AS tags, \
           EXISTS (SELECT 1 FROM preview_cards_statuses pcs WHERE pcs.status_id = s.id) AS with_card, \
           EXISTS (SELECT 1 FROM preview_cards_statuses pcs JOIN preview_cards pc ON pc.id = pcs.preview_card_id \
                   WHERE pcs.status_id = s.id AND pc.type = 2) AS card_is_video, \
           EXISTS (SELECT 1 FROM quotes q WHERE q.status_id = s.id) AS with_quote, \
           ARRAY( \
             SELECT s.account_id WHERE coalesce(s.local, s.uri IS NULL) \
             UNION SELECT m.account_id FROM mentions m JOIN accounts ma ON ma.id = m.account_id \
               WHERE m.status_id = s.id AND NOT m.silent AND ma.domain IS NULL \
             UNION SELECT f.account_id FROM favourites f JOIN accounts fa ON fa.id = f.account_id \
               WHERE f.status_id = s.id AND fa.domain IS NULL \
             UNION SELECT r.account_id FROM statuses r JOIN accounts ra ON ra.id = r.account_id \
               WHERE r.reblog_of_id = s.id AND r.deleted_at IS NULL AND ra.domain IS NULL \
             UNION SELECT b.account_id FROM bookmarks b JOIN accounts ba ON ba.id = b.account_id \
               WHERE b.status_id = s.id AND ba.domain IS NULL \
             UNION SELECT v.account_id FROM poll_votes v JOIN accounts va ON va.id = v.account_id \
               WHERE v.poll_id = s.poll_id AND va.domain IS NULL \
           ) AS searchable_by \
         FROM statuses s \
         JOIN accounts a ON a.id = s.account_id \
         LEFT JOIN polls p ON p.id = s.poll_id \
         WHERE s.id = ANY($1) AND s.deleted_at IS NULL AND s.reblog_of_id IS NULL \
           AND (NOT $2 OR (s.visibility = 0 AND a.indexable))",
    )
    .bind(ids)
    .bind(public)
    .fetch_all(&state.db)
    .await?)
}

/// `Status#searchable_text`.
fn status_text(s: &StatusRow) -> String {
    let mut parts = vec![s.spoiler_text.clone(), plain_text(&s.text, s.local)];
    if let Some(options) = &s.poll_options {
        parts.push(options.join("\n\n"));
    }
    parts.push(
        s.media_descriptions
            .iter()
            .map(|description| description.clone().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n\n"),
    );
    parts.join("\n\n")
}

/// `Status#searchable_properties`. Media types: image 0, gifv 1, video 2,
/// unknown 3, audio 4.
fn status_properties(s: &StatusRow) -> Vec<&'static str> {
    let mut properties = vec![];
    let has = |kind: i32| s.media_types.contains(&kind);
    if has(0) {
        properties.push("image");
    }
    if has(2) {
        properties.push("video");
    }
    if has(4) {
        properties.push("audio");
    }
    if !s.media_types.is_empty() {
        properties.push("media");
    }
    if s.poll_options.is_some() {
        properties.push("poll");
    }
    if s.with_card {
        properties.push("link");
    }
    if s.card_is_video {
        properties.push("embed");
    }
    if s.sensitive {
        properties.push("sensitive");
    }
    if s.reply {
        properties.push("reply");
    }
    if s.with_quote {
        properties.push("quote");
    }
    properties
}

fn status_doc(s: &StatusRow, with_searchable_by: bool) -> Value {
    let mut doc = json!({
        "id": s.id,
        "account_id": s.account_id,
        "text": status_text(s),
        "tags": s.tags,
        "language": s.language,
        "properties": status_properties(s),
        "created_at": date(s.created_at),
    });
    if with_searchable_by {
        doc["searchable_by"] = json!(s.searchable_by);
    }
    doc
}

/// The documents for `ids` in `index`: `(id, Some(doc))` to index and
/// `(id, None)` to delete.
pub async fn documents(
    state: &AppState,
    index: Index,
    ids: &[i64],
) -> anyhow::Result<Vec<(i64, Option<Value>)>> {
    let found: Vec<(i64, Value)> = match index {
        Index::Accounts => accounts(state, ids).await?,
        Index::Tags => tags(state, ids).await?,
        Index::PublicStatuses => status_rows(state, ids, true)
            .await?
            .iter()
            .map(|s| (s.id, status_doc(s, false)))
            .collect(),
        // `delete_if: ->(status) { status.searchable_by.empty? }`.
        Index::Statuses => status_rows(state, ids, false)
            .await?
            .iter()
            .filter(|s| !s.searchable_by.is_empty())
            .map(|s| (s.id, status_doc(s, true)))
            .collect(),
        Index::Instances => anyhow::bail!("instances are indexed by domain"),
    };
    let mut by_id: std::collections::HashMap<i64, Value> = found.into_iter().collect();
    Ok(ids.iter().map(|id| (*id, by_id.remove(id))).collect())
}

/// `_bulk` lines for `documents`.
pub fn bulk_lines<I: std::fmt::Display>(documents: Vec<(I, Option<Value>)>) -> Vec<Value> {
    let mut lines = Vec::with_capacity(documents.len() * 2);
    for (id, doc) in documents {
        match doc {
            Some(doc) => {
                lines.push(json!({ "index": { "_id": id.to_string() } }));
                lines.push(doc);
            }
            None => lines.push(json!({ "delete": { "_id": id.to_string() } })),
        }
    }
    lines
}

/// `Instance.searchable`, as `InstancesIndex` documents keyed by domain:
/// the domains accounts are on and the allowed ones, less the blocked.
pub async fn instances(state: &AppState) -> anyhow::Result<Vec<(String, Option<Value>)>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "WITH domain_counts AS ( \
           SELECT domain, count(*) AS accounts_count FROM accounts \
           WHERE domain IS NOT NULL GROUP BY domain \
         ) \
         SELECT i.domain, coalesce(c.accounts_count, 0) AS accounts_count \
         FROM (SELECT domain FROM domain_counts UNION SELECT domain FROM domain_allows) i \
         LEFT JOIN domain_counts c ON c.domain = i.domain \
         WHERE i.domain NOT IN (SELECT domain FROM domain_blocks)",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(domain, count)| {
            let doc = json!({ "domain": domain, "accounts_count": count });
            (domain, Some(doc))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_as_plain_text_formatter() {
        assert_eq!(
            plain_text("<p>a</p><p>b &amp; c<br>d</p>", false),
            "a\nb & c\nd"
        );
        assert_eq!(
            plain_text("*kept* <b>as is</b>", true),
            "*kept* <b>as is</b>"
        );
    }

    #[test]
    fn dates_are_clamped_and_in_utc() {
        let d = chrono::NaiveDate::from_ymd_opt(2024, 10, 31)
            .unwrap()
            .and_hms_milli_opt(23, 47, 20, 5)
            .unwrap();
        assert_eq!(date(d), "2024-10-31T23:47:20.005Z");
    }

    #[test]
    fn bulk_lines_index_and_delete() {
        let lines = bulk_lines(vec![(1, Some(json!({ "a": 1 }))), (2, None)]);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["index"]["_id"], "1");
        assert_eq!(lines[2]["delete"]["_id"], "2");
    }
}
