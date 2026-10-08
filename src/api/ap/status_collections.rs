//! The collections of a local post, and of a local thread, that other
//! servers fetch: its replies (`ActivityPub::RepliesController`), who liked
//! and who boosted it (`ActivityPub::LikesController`,
//! `ActivityPub::SharesController`), and the posts of the conversation it
//! started (`ActivityPub::ContextsController`). Each is written as
//! `ActivityPub::CollectionSerializer` writes it, and paged by the query
//! Mastodon pages it by, which is already in the documents that link it.

use ojak::federation::CollectionDocument;
use serde_json::{json, Value};
use url::Url;

use super::objects::{AccountRef, Reader};
use crate::db::models::Account;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// `ActivityPub::RepliesController::DESCENDANTS_LIMIT`, and
/// `ActivityPub::ContextsController::DESCENDANTS_LIMIT`.
const DESCENDANTS_LIMIT: i64 = 60;

const ACTIVITY_STREAMS: &str = "https://www.w3.org/ns/activitystreams";

/// The query parameters a request was made with, as the collection reads
/// them: `truthy_param?` and the `min_id` to page from.
pub struct Query {
    /// Every parameter, decoded, in the order given.
    pairs: Vec<(String, String)>,
}

impl Query {
    #[must_use]
    pub fn parse(query: Option<&str>) -> Self {
        Self {
            pairs: url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect(),
        }
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// `truthy_param?`: `ActiveModel::Type::Boolean` of the value, which is
    /// true unless it is missing, blank, or one of Rails' false values.
    pub fn truthy(&self, name: &str) -> bool {
        self.get(name).is_some_and(|value| {
            !value.is_empty()
                && !matches!(value, "0" | "f" | "F" | "false" | "FALSE" | "off" | "OFF")
        })
    }

    /// The `min_id` to page from, when it is a number.
    fn min_id(&self) -> Option<i64> {
        self.number("min_id")
    }

    /// The parameter `name`, when it is a number.
    #[must_use]
    pub fn number(&self, name: &str) -> Option<i64> {
        self.get(name).and_then(|id| id.trim().parse().ok())
    }
}

/// `url` with `params` as its query, sorted by name as Rails' `to_query`
/// sorts them.
fn with_query(url: &Url, params: &[(&str, &str)]) -> Url {
    let mut params = params.to_vec();
    params.sort_by(|a, b| a.0.cmp(b.0));
    let mut url = url.clone();
    url.set_query(None);
    if !params.is_empty() {
        url.query_pairs_mut().extend_pairs(params);
    }
    url
}

fn parse(uri: &str) -> AppResult<Url> {
    Url::parse(uri).map_err(|error| AppError::Internal(error.into()))
}

/// A local post's own URI, its author, and whether it is distributable,
/// when `reader` may be shown it (`@account.statuses.find` and
/// `authorize @status, :show?`).
async fn servable(
    state: &AppState,
    who: AccountRef<'_>,
    id: i64,
    reader: Option<&Reader>,
) -> AppResult<(Account, String, bool)> {
    let servable = super::objects::servable_status(state, who, id, reader).await?;
    let distributable = servable.distributable();
    let account = servable.account;
    let uri = crate::federation::tag::status_uri(
        &state.instance.domain,
        account.id,
        account.id_scheme,
        &account.username,
        id,
    );
    Ok((account, uri, distributable))
}

/// `TagManager#uri_for` a local post: beneath its author's actor.
async fn local_status_uri(state: &AppState, id: i64) -> AppResult<Option<String>> {
    let author = sqlx::query!(
        r#"SELECT a.id, a.id_scheme, a.username FROM statuses s
           JOIN accounts a ON a.id = s.account_id WHERE s.id = $1"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?;
    Ok(author.map(|a| {
        crate::federation::tag::status_uri(
            &state.instance.domain,
            a.id,
            a.id_scheme,
            &a.username,
            id,
        )
    }))
}

/// The items of a page of replies or of a thread: a local post embedded, as
/// `ActivityPub::NoteSerializer` writes it, and a remote one by its URI.
/// Says whether any was embedded, whose context the document then takes.
async fn embedded_or_uri(
    state: &AppState,
    rows: &[(i64, bool, Option<String>)],
    embed: bool,
) -> AppResult<(Vec<Value>, bool)> {
    let domain = &state.instance.domain;
    let mut items = Vec::with_capacity(rows.len());
    let mut embedded = false;
    for (id, local, uri) in rows {
        if *local {
            if embed {
                if let Some(bundle) = super::note::build_note(state, domain, *id).await? {
                    items.push(bundle.note);
                    embedded = true;
                    continue;
                }
            }
            if let Some(uri) = local_status_uri(state, *id).await? {
                items.push(json!(uri));
            }
        } else if let Some(uri) = uri {
            items.push(json!(uri));
        }
    }
    Ok((items, embedded))
}

/// `ActivityPub::RepliesController#index`: the replies to a local post. The
/// author's own come first, sixty to a page; the page after the last of
/// them goes on to everyone else's (`only_other_accounts`), by accounts that
/// are not suspended. A local reply is embedded, a remote one named. Says
/// too whether the post is distributable, which its caching depends on.
pub async fn replies(
    state: &AppState,
    who: AccountRef<'_>,
    status_id: i64,
    query: &Query,
    reader: Option<&Reader>,
) -> AppResult<(Value, bool)> {
    let (account, status_uri, distributable) = servable(state, who, status_id, reader).await?;
    let replies_uri = parse(&format!("{status_uri}/replies"))?;
    let only_other_accounts = query.truthy("only_other_accounts");
    let min_id = query.min_id();

    let rows = sqlx::query!(
        r#"SELECT s.id, s.account_id, (COALESCE(s.local, false) OR s.uri IS NULL) AS "local!", s.uri
           FROM statuses s JOIN accounts a ON a.id = s.account_id
           WHERE s.in_reply_to_id = $1 AND s.deleted_at IS NULL
             AND s.visibility IN (0, 1) /* distributable_visibility */
             AND CASE WHEN $2 THEN s.account_id <> $3 AND a.suspended_at IS NULL
                      ELSE s.account_id = $3 END
             AND ($4::bigint IS NULL OR s.id > $4)
           ORDER BY s.id
           LIMIT $5"#,
        status_id,
        only_other_accounts,
        account.id,
        min_id,
        DESCENDANTS_LIMIT,
    )
    .fetch_all(&state.db)
    .await?;
    let full = rows.len() as i64 >= DESCENDANTS_LIMIT;
    let last = rows.last().map(|row| (row.id, row.account_id));

    // `next_page`: the author's own replies run on while there are sixty
    // to a page, and then everyone else's start.
    let next = if only_other_accounts {
        full.then(|| {
            let min_id = last.map(|(id, _)| id.to_string()).unwrap_or_default();
            with_query(
                &replies_uri,
                &[
                    ("min_id", &min_id),
                    ("only_other_accounts", "true"),
                    ("page", "true"),
                ],
            )
        })
    } else {
        let next_only_other_accounts = last.map(|(_, author)| author) != Some(account.id) || !full;
        Some(if next_only_other_accounts {
            with_query(
                &replies_uri,
                &[("only_other_accounts", "true"), ("page", "true")],
            )
        } else {
            let min_id = last.map(|(id, _)| id.to_string()).unwrap_or_default();
            with_query(&replies_uri, &[("min_id", &min_id), ("page", "true")])
        })
    };

    let rows: Vec<(i64, bool, Option<String>)> = rows
        .into_iter()
        .map(|row| (row.id, row.local, row.uri))
        .collect();
    let (items, embedded) = embedded_or_uri(state, &rows, true).await?;

    // `page_params`: the parameters it was asked with, and `page`.
    let mut page_params: Vec<(&str, &str)> = ["only_other_accounts", "min_id"]
        .into_iter()
        .filter_map(|name| query.get(name).map(|value| (name, value)))
        .collect();
    page_params.push(("page", "true"));
    let page = CollectionDocument {
        id: Some(with_query(&replies_uri, &page_params)),
        // `part_of: account_status_replies_url(@account, @status)`: the
        // username route, whichever scheme the account uses.
        part_of: Some(parse(&format!(
            "https://{}/users/{}/statuses/{status_id}/replies",
            state.instance.domain, account.username
        ))?),
        next,
        items: Some(items),
        ..CollectionDocument::default()
    };
    let mut document = if query.truthy("page") {
        page.to_value()
    } else {
        CollectionDocument {
            id: Some(replies_uri),
            first: Some(page.to_value()),
            ..CollectionDocument::default()
        }
        .to_value()
    };
    document["@context"] = if embedded {
        super::note::note_context()
    } else {
        json!(ACTIVITY_STREAMS)
    };
    Ok((document, distributable))
}

/// Who liked a local post, or who boosted it: only how many
/// (`ActivityPub::LikesController`, `ActivityPub::SharesController`).
pub async fn interactions(
    state: &AppState,
    who: AccountRef<'_>,
    status_id: i64,
    which: Interactions,
    reader: Option<&Reader>,
) -> AppResult<(Value, bool)> {
    let (_, status_uri, distributable) = servable(state, who, status_id, reader).await?;
    let counts = sqlx::query!(
        r#"SELECT GREATEST(favourites_count, 0) AS "favourites!",
                  GREATEST(reblogs_count, 0) AS "reblogs!"
           FROM status_stats WHERE status_id = $1"#,
        status_id,
    )
    .fetch_optional(&state.db)
    .await?;
    let (suffix, total) = match which {
        Interactions::Likes => ("likes", counts.as_ref().map_or(0, |c| c.favourites)),
        Interactions::Shares => ("shares", counts.as_ref().map_or(0, |c| c.reblogs)),
    };
    let mut document = CollectionDocument {
        id: Some(parse(&format!("{status_uri}/{suffix}"))?),
        total_items: Some(u64::try_from(total).unwrap_or(0)),
        ..CollectionDocument::default()
    }
    .to_value();
    document["@context"] = json!(ACTIVITY_STREAMS);
    Ok((document, distributable))
}

/// Which of a post's interactions.
#[derive(Clone, Copy)]
pub enum Interactions {
    Likes,
    Shares,
}

/// The local conversation `/contexts/{account}-{status}` names: one of ours
/// (no `uri`) started by that post. When there is none,
/// `ActivityPub::ContextsController` fails on the `nil` it found
/// (`@conversation.statuses`), a 500, and so does this; an id that is not
/// two numbers is not routed there, a 404.
async fn local_conversation(state: &AppState, id: &str) -> AppResult<(i64, i64, i64)> {
    let (account_id, status_id) = id
        .split_once('-')
        .and_then(|(a, s)| Some((a.parse::<i64>().ok()?, s.parse::<i64>().ok()?)))
        .ok_or(AppError::NotFound)?;
    let conversation = sqlx::query_scalar!(
        r#"SELECT id FROM conversations
           WHERE uri IS NULL AND parent_account_id = $1 AND parent_status_id = $2
           ORDER BY id LIMIT 1"#,
        account_id,
        status_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!(
            "no local conversation {account_id}-{status_id}, as Mastodon fails on it"
        ))
    })?;
    Ok((conversation, account_id, status_id))
}

/// A page of a conversation's public and unlisted posts, sixty from
/// `min_id`, each by its URI, and the page after it.
async fn conversation_page(
    state: &AppState,
    conversation: i64,
    items_uri: &Url,
    min_id: Option<i64>,
) -> AppResult<(Vec<Value>, Option<Url>)> {
    let rows = sqlx::query!(
        r#"SELECT id, (COALESCE(local, false) OR uri IS NULL) AS "local!", uri
           FROM statuses
           WHERE conversation_id = $1 AND deleted_at IS NULL
             AND visibility IN (0, 1) /* distributable_visibility */
             AND ($2::bigint IS NULL OR id > $2)
           ORDER BY id
           LIMIT $3"#,
        conversation,
        min_id,
        DESCENDANTS_LIMIT,
    )
    .fetch_all(&state.db)
    .await?;
    let next = (rows.len() as i64 >= DESCENDANTS_LIMIT)
        .then(|| rows.last().map(|row| row.id.to_string()))
        .flatten()
        .map(|min_id| with_query(items_uri, &[("min_id", &min_id), ("page", "true")]));
    let rows: Vec<(i64, bool, Option<String>)> = rows
        .into_iter()
        .map(|row| (row.id, row.local, row.uri))
        .collect();
    let (items, _) = embedded_or_uri(state, &rows, false).await?;
    Ok((items, next))
}

/// `ActivityPub::ContextsController#show`: a thread started here, as the
/// `context` its posts name, with its first page of posts embedded.
pub async fn context(state: &AppState, id: &str, query: &Query) -> AppResult<Value> {
    let (conversation, account_id, status_id) = local_conversation(state, id).await?;
    let domain = &state.instance.domain;
    let context_uri = parse(&format!(
        "https://{domain}/contexts/{account_id}-{status_id}"
    ))?;
    let items_uri = parse(&format!("{context_uri}/items"))?;
    let (items, next) = conversation_page(state, conversation, &items_uri, query.min_id()).await?;
    let first = CollectionDocument {
        part_of: Some(context_uri.clone()),
        next,
        items: Some(items),
        ..CollectionDocument::default()
    };
    // `attributed_to`: the account that started it, if it is still there.
    let attributed_to = sqlx::query!(
        "SELECT id, id_scheme, username, domain, uri FROM accounts WHERE id = $1",
        account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .map(|a| {
        super::collections::resolve_actor_uri(
            domain,
            a.uri,
            a.domain.is_none(),
            a.id,
            a.id_scheme,
            &a.username,
        )
    });
    let mut document = serde_json::Map::new();
    document.insert("@context".into(), json!(ACTIVITY_STREAMS));
    document.insert("id".into(), json!(context_uri.as_str()));
    document.insert("type".into(), json!("Collection"));
    document.insert("attributedTo".into(), json!(attributed_to));
    document.insert("first".into(), first.to_value());
    Ok(Value::Object(document))
}

/// `ActivityPub::ContextsController#items`: a thread's posts, sixty to a
/// page.
pub async fn context_items(state: &AppState, id: &str, query: &Query) -> AppResult<Value> {
    let (conversation, account_id, status_id) = local_conversation(state, id).await?;
    let domain = &state.instance.domain;
    let context_uri = parse(&format!(
        "https://{domain}/contexts/{account_id}-{status_id}"
    ))?;
    let items_uri = parse(&format!("{context_uri}/items"))?;
    let (items, next) = conversation_page(state, conversation, &items_uri, query.min_id()).await?;
    // `page_params`: `page` and `min_id`, as they were given.
    let page_params: Vec<(&str, &str)> = ["page", "min_id"]
        .into_iter()
        .filter_map(|name| query.get(name).map(|value| (name, value)))
        .collect();
    let page = CollectionDocument {
        id: Some(with_query(&items_uri, &page_params)),
        part_of: Some(context_uri),
        next,
        items: Some(items),
        ..CollectionDocument::default()
    };
    let mut document = if query.truthy("page") {
        page.to_value()
    } else {
        CollectionDocument {
            id: Some(items_uri),
            first: Some(page.to_value()),
            ..CollectionDocument::default()
        }
        .to_value()
    };
    document["@context"] = json!(ACTIVITY_STREAMS);
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthy_params_are_rails_booleans() {
        let query = Query::parse(Some("page=true&a=1&b=0&c=false&d=&e=off&f=yes"));
        assert!(query.truthy("page"));
        assert!(query.truthy("a"));
        assert!(query.truthy("f"));
        for name in ["b", "c", "d", "e", "missing"] {
            assert!(!query.truthy(name), "{name}");
        }
    }

    #[test]
    fn page_urls_sort_their_parameters() {
        let base = Url::parse("https://a.test/users/x/statuses/1/replies").unwrap();
        assert_eq!(
            with_query(
                &base,
                &[
                    ("page", "true"),
                    ("only_other_accounts", "true"),
                    ("min_id", "5")
                ]
            )
            .as_str(),
            "https://a.test/users/x/statuses/1/replies?min_id=5&only_other_accounts=true&page=true"
        );
    }
}
