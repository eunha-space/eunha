//! Status serialization / hydration.
//!
//! Turns database status rows into Mastodon API `Status` entities and batch-
//! hydrates the associated media, reblogs, quotes, polls, tags, mentions,
//! emojis, cards and counters. Split out of `accounts.rs`.
//!
//! Mastodon: app/serializers/rest/status_serializer.rb
//! Mastodon: app/serializers/rest/base_quote_serializer.rb
//! Mastodon: app/serializers/rest/quote_serializer.rb
//! Mastodon: app/serializers/rest/shallow_quote_serializer.rb
//! Mastodon: app/serializers/rest/shallow_status_serializer.rb
//! Mastodon: app/presenters/status_relationships_presenter.rb

use super::accounts::{
    batch_account_stats, fetch_account, fetch_account_emojis, fetch_account_roles,
};
use crate::db::models::Account;
use crate::error::AppResult;
use crate::state::AppState;

pub async fn batch_status_media(
    state: &AppState,
    status_ids: &[i64],
) -> AppResult<std::collections::HashMap<i64, Vec<crate::db::models::MediaAttachment>>> {
    if status_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    // `Status#ordered_media_attachments`: in `ordered_media_attachment_ids`'
    // order, and only those it names, or every attachment by id when it is
    // null; at most `MEDIA_ATTACHMENTS_LIMIT`.
    let rows = sqlx::query_as!(
        crate::db::models::MediaAttachment,
        r#"SELECT m.* FROM media_attachments m
           JOIN statuses s ON s.id = m.status_id
           WHERE m.status_id = ANY($1::bigint[])
             AND (s.ordered_media_attachment_ids IS NULL
                  OR m.id = ANY(s.ordered_media_attachment_ids))
           ORDER BY m.status_id,
                    array_position(s.ordered_media_attachment_ids, m.id),
                    m.id"#,
        status_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let mut map: std::collections::HashMap<i64, Vec<_>> = std::collections::HashMap::new();
    for m in rows {
        if let Some(sid) = m.status_id {
            let media = map.entry(sid).or_default();
            if media.len() < MEDIA_ATTACHMENTS_LIMIT {
                media.push(m);
            }
        }
    }
    Ok(map)
}

/// `Status::MEDIA_ATTACHMENTS_LIMIT`: how many of a status's attachments
/// `ordered_media_attachments` shows.
pub const MEDIA_ATTACHMENTS_LIMIT: usize = 4;

pub async fn batch_reblog_data(
    state: &AppState,
    statuses: &[crate::db::models::Status],
) -> AppResult<
    std::collections::HashMap<
        i64,
        (
            crate::db::models::Status,
            crate::db::models::Account,
            Vec<crate::db::models::MediaAttachment>,
        ),
    >,
> {
    use std::collections::{HashMap, HashSet};

    let reblog_ids: Vec<i64> = statuses
        .iter()
        .filter_map(|s| s.reblog_of_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    if reblog_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let reblog_statuses = sqlx::query_as!(
        crate::db::models::Status,
        "SELECT * FROM statuses WHERE id = ANY($1::bigint[]) AND deleted_at IS NULL",
        &reblog_ids,
    )
    .fetch_all(&state.db)
    .await?;

    let nested_reblog_ids: Vec<i64> = reblog_statuses
        .iter()
        .filter_map(|s| s.reblog_of_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let nested_reblog_statuses = if nested_reblog_ids.is_empty() {
        vec![]
    } else {
        sqlx::query_as!(
            crate::db::models::Status,
            "SELECT * FROM statuses WHERE id = ANY($1::bigint[]) AND deleted_at IS NULL",
            &nested_reblog_ids,
        )
        .fetch_all(&state.db)
        .await?
    };
    let nested_reblog_status_map: HashMap<i64, crate::db::models::Status> = nested_reblog_statuses
        .into_iter()
        .map(|s| (s.id, s))
        .collect();

    let resolved_reblog_status_map: HashMap<i64, crate::db::models::Status> = reblog_statuses
        .into_iter()
        .map(|s| {
            let resolved = s
                .reblog_of_id
                .and_then(|id| nested_reblog_status_map.get(&id).cloned())
                .unwrap_or_else(|| s.clone());
            (s.id, resolved)
        })
        .collect();

    let reblog_account_ids: Vec<i64> = resolved_reblog_status_map
        .values()
        .map(|s| s.account_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let reblog_accounts = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
        &reblog_account_ids,
    )
    .fetch_all(&state.db)
    .await?;

    let reblog_account_map: HashMap<i64, Account> =
        reblog_accounts.into_iter().map(|a| (a.id, a)).collect();

    let reblog_status_ids: Vec<i64> = resolved_reblog_status_map.values().map(|s| s.id).collect();
    let reblog_media = batch_status_media(state, &reblog_status_ids).await?;

    let mut result = HashMap::new();
    for s in statuses {
        if let Some(reblog_id) = s.reblog_of_id {
            if let Some(rs) = resolved_reblog_status_map.get(&reblog_id) {
                if let Some(ra) = reblog_account_map.get(&rs.account_id) {
                    let media = reblog_media.get(&rs.id).cloned().unwrap_or_default();
                    result.insert(s.id, (rs.clone(), ra.clone(), media));
                }
            }
        }
    }
    Ok(result)
}

/// `REST::StatusSerializer#quote` for every status built here and the post
/// each one boosts (`belongs_to :reblog, serializer: REST::StatusSerializer`
/// renders the boosted post whole, its quote included), as `viewer_id` sees
/// them. Called by [`hydrate_status_stats`], so every endpoint that renders
/// statuses carries their quotes.
async fn attach_quotes(
    state: &AppState,
    statuses: &mut [&mut super::types::Status],
    viewer_id: Option<i64>,
) {
    let mut ids: Vec<i64> = Vec::new();
    for s in statuses.iter() {
        ids.extend(s.id.parse::<i64>().ok());
        if let Some(rb) = s.reblog.as_deref() {
            ids.extend(rb.id.parse::<i64>().ok());
        }
    }
    if ids.is_empty() {
        return;
    }
    let Ok(quotes) = batch_quote_data(state, &ids, viewer_id).await else {
        return;
    };
    let quote_of = |s: &super::types::Status| {
        s.id.parse::<i64>()
            .ok()
            .and_then(|id| quotes.get(&id).cloned())
    };
    for s in statuses.iter_mut() {
        s.quote = quote_of(s);
        if let Some(rb) = s.reblog.as_deref_mut() {
            rb.quote = quote_of(rb);
        }
    }
}

/// What `REST::BaseQuoteSerializer` reads of one status's quote, for one
/// viewer.
struct ResolvedQuote {
    /// `quotes.id`.
    id: i64,
    /// `quotes.state`.
    state: i32,
    /// `object.quoted_status`, under `Status`'s `default_scope { kept }`:
    /// `None` when the quoted post is gone or was never known.
    quoted: Option<crate::db::models::Status>,
    /// `StatusFilter#filter_state_for_quote` for the viewer.
    filter: Option<&'static str>,
}

impl ResolvedQuote {
    /// `REST::BaseQuoteSerializer#state`.
    fn state(&self) -> String {
        use crate::db::models::quote_state;
        if self.state != quote_state::ACCEPTED {
            quote_state::to_str(self.state).to_owned()
        } else if self.quoted.is_none() {
            "deleted".to_owned()
        } else {
            self.filter.unwrap_or("accepted").to_owned()
        }
    }

    /// `REST::BaseQuoteSerializer#quoted_status`: the quoted post, when the
    /// quote is accepted (or the source was requested), the post is there,
    /// and the viewer may see it.
    fn shown(&self, source_requested: bool) -> Option<&crate::db::models::Status> {
        if self.state != crate::db::models::quote_state::ACCEPTED && !source_requested {
            return None;
        }
        let quoted = self.quoted.as_ref()?;
        (self.filter != Some("unauthorized")).then_some(quoted)
    }

    /// `REST::ShallowQuoteSerializer`: the quote of a quoted post, naming
    /// what it quotes by id.
    fn shallow(&self, source_requested: bool) -> super::types::QuoteInfo {
        super::types::QuoteInfo {
            state: self.state(),
            quoted_status: None,
            quoted_status_id: self.shown(source_requested).map(|q| q.id.to_string()),
            shallow: true,
        }
    }
}

/// How a quote is rendered.
#[derive(Debug, Clone, Copy)]
pub(crate) struct QuoteRendering {
    /// `REST::StatusSerializer#quote`'s `object.quote&.acceptable?`: a
    /// legacy quote never accepted is left out. `StatusEditSerializer`
    /// renders the status's quote whatever it is.
    pub acceptable_only: bool,
    /// `instance_options[:source_requested]`.
    pub source_requested: bool,
}

impl QuoteRendering {
    /// `REST::StatusSerializer`'s, as almost everything renders a status.
    pub const STATUS: Self = Self {
        acceptable_only: true,
        source_requested: false,
    };
}

/// The quotes `status_ids` make, each with its quoted post and how
/// `viewer_id` sees it. With `acceptable_only`, those
/// `REST::StatusSerializer#quote` shows (`object.quote if
/// object.quote&.acceptable?`: accepted, or not legacy).
async fn resolve_quotes(
    state: &AppState,
    status_ids: &[i64],
    viewer_id: Option<i64>,
    acceptable_only: bool,
) -> AppResult<std::collections::HashMap<i64, ResolvedQuote>> {
    use std::collections::{HashMap, HashSet};

    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut quote_rows = sqlx::query!(
        r#"SELECT id, status_id, quoted_status_id, state, legacy FROM quotes
           WHERE status_id = ANY($1::bigint[])"#,
        status_ids,
    )
    .fetch_all(&state.db)
    .await?;
    // `Quote#acceptable?`.
    if acceptable_only {
        quote_rows.retain(|r| r.state == crate::db::models::quote_state::ACCEPTED || !r.legacy);
    }
    if quote_rows.is_empty() {
        return Ok(HashMap::new());
    }

    let quote_ids: Vec<i64> = quote_rows
        .iter()
        .filter_map(|r| r.quoted_status_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let quoted_statuses: HashMap<i64, crate::db::models::Status> = sqlx::query_as!(
        crate::db::models::Status,
        "SELECT * FROM statuses WHERE id = ANY($1::bigint[]) AND deleted_at IS NULL",
        &quote_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|s| (s.id, s))
    .collect();

    // `StatusFilter#filter_state_for_quote` for each quoted status, as the
    // viewer sees it: nothing for their own post, `unauthorized` for one
    // `StatusPolicy#show?` hides from them, then `blocked_domain`,
    // `blocked_account` and `muted_account`.
    let mut filter_states: HashMap<i64, &'static str> = HashMap::new();
    if !quoted_statuses.is_empty() {
        let author_ids: Vec<i64> = quoted_statuses
            .values()
            .map(|s| s.account_id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let relations = sqlx::query!(
            r#"SELECT a.id,
                      (a.suspended_at IS NOT NULL OR a.requested_deletion_at IS NOT NULL) AS "unavailable!",
                      EXISTS (SELECT 1 FROM blocks WHERE account_id = a.id AND target_account_id = $2) AS "blocks_viewer!",
                      EXISTS (SELECT 1 FROM account_domain_blocks adb JOIN accounts v ON v.id = $2
                              WHERE adb.account_id = a.id AND adb.domain = v.domain) AS "blocks_viewer_domain!",
                      EXISTS (SELECT 1 FROM blocks WHERE account_id = $2 AND target_account_id = a.id) AS "blocked!",
                      EXISTS (SELECT 1 FROM account_domain_blocks
                              WHERE account_id = $2 AND domain = a.domain) AS "domain_blocked!",
                      EXISTS (SELECT 1 FROM mutes WHERE account_id = $2 AND target_account_id = a.id) AS "muted!",
                      EXISTS (SELECT 1 FROM follows WHERE account_id = $2 AND target_account_id = a.id) AS "following!"
               FROM accounts a WHERE a.id = ANY($1::bigint[])"#,
            &author_ids,
            viewer_id,
        )
        .fetch_all(&state.db)
        .await?;
        let relations: HashMap<i64, _> = relations.into_iter().map(|r| (r.id, r)).collect();
        let mentioned: HashSet<i64> = match viewer_id {
            Some(vid) => sqlx::query_scalar!(
                "SELECT status_id FROM mentions WHERE account_id = $1 AND status_id = ANY($2::bigint[])",
                vid,
                &quote_ids,
            )
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .collect(),
            None => HashSet::new(),
        };
        use crate::db::models::vis;
        for qs in quoted_statuses.values() {
            let Some(r) = relations.get(&qs.account_id) else {
                continue;
            };
            if viewer_id == Some(qs.account_id) {
                continue;
            }
            let shown = !r.unavailable
                && match qs.visibility {
                    vis::DIRECT | vis::LIMITED => mentioned.contains(&qs.id),
                    vis::PRIVATE => r.following || mentioned.contains(&qs.id),
                    _ => viewer_id.is_none() || (!r.blocks_viewer && !r.blocks_viewer_domain),
                };
            let filter = if !shown {
                Some("unauthorized")
            } else if viewer_id.is_some() && r.domain_blocked {
                Some("blocked_domain")
            } else if viewer_id.is_some() && r.blocked {
                Some("blocked_account")
            } else if viewer_id.is_some() && r.muted {
                Some("muted_account")
            } else {
                None
            };
            if let Some(filter) = filter {
                filter_states.insert(qs.id, filter);
            }
        }
    }

    Ok(quote_rows
        .into_iter()
        .map(|r| {
            let quoted = r
                .quoted_status_id
                .and_then(|id| quoted_statuses.get(&id).cloned());
            let filter = quoted
                .as_ref()
                .and_then(|q| filter_states.get(&q.id).copied());
            (
                r.status_id,
                ResolvedQuote {
                    id: r.id,
                    state: r.state,
                    quoted,
                    filter,
                },
            )
        })
        .collect())
}

/// `REST::StatusSerializer#quote` for each of `status_ids` that has one, as
/// `viewer_id` sees it: `REST::QuoteSerializer`, the quoted post embedded
/// when it is shown, and that post's own quote rendered shallow
/// (`REST::ShallowStatusSerializer`).
pub async fn batch_quote_data(
    state: &AppState,
    status_ids: &[i64],
    viewer_id: Option<i64>,
) -> AppResult<std::collections::HashMap<i64, super::types::QuoteInfo>> {
    Ok(
        render_quotes(state, status_ids, viewer_id, QuoteRendering::STATUS)
            .await?
            .into_iter()
            .map(|(status_id, (_, quote))| (status_id, quote))
            .collect(),
    )
}

/// [`batch_quote_data`] rendered as `how` says, each quote with its id. The
/// posts the quotes embed are not yet hydrated: [`hydrate_status_stats`]
/// does that for the statuses carrying them, or [`hydrate_without_quotes`]
/// for a caller rendering them elsewhere.
pub(crate) async fn render_quotes(
    state: &AppState,
    status_ids: &[i64],
    viewer_id: Option<i64>,
    how: QuoteRendering,
) -> AppResult<std::collections::HashMap<i64, (i64, super::types::QuoteInfo)>> {
    use std::collections::{HashMap, HashSet};

    let resolved = resolve_quotes(state, status_ids, viewer_id, how.acceptable_only).await?;
    if resolved.is_empty() {
        return Ok(HashMap::new());
    }

    let mut seen = HashSet::new();
    let quoted_statuses: Vec<crate::db::models::Status> = resolved
        .values()
        .filter_map(|r| r.shown(how.source_requested))
        .filter(|q| seen.insert(q.id))
        .cloned()
        .collect();

    // `REST::ShallowStatusSerializer` is a `REST::StatusSerializer`: the
    // quoted posts are rendered whole, for the viewer, with what
    // `StatusRelationshipsPresenter` gives them (their flags and `filtered`);
    // `hydrate_status_stats` adds their counts as it does the quoting posts'.
    let mut built =
        super::timelines::build_status_list_unhydrated(state, &quoted_statuses, viewer_id).await?;
    let filters = match viewer_id {
        Some(vid) => {
            super::timelines::compute_filter_results(&state.db, vid, &quoted_statuses).await
        }
        None => HashMap::new(),
    };

    // The quoted posts' own quotes, as `REST::ShallowQuoteSerializer` has
    // them, under the same rules for the same viewer.
    let qs_ids: Vec<i64> = quoted_statuses.iter().map(|s| s.id).collect();
    let nested = resolve_quotes(state, &qs_ids, viewer_id, true).await?;

    let mut qs_map: HashMap<i64, super::types::Status> = HashMap::new();
    for (qs, mut api) in quoted_statuses.iter().zip(built.drain(..)) {
        if let Some(serde_json::Value::Array(found)) = filters.get(&qs.id) {
            api.filtered = Some(found.clone());
        }
        api.quote = nested.get(&qs.id).map(|r| r.shallow(false));
        qs_map.insert(qs.id, api);
    }

    Ok(resolved
        .into_iter()
        .map(|(status_id, r)| {
            let quoted_status = r
                .shown(how.source_requested)
                .and_then(|q| qs_map.get(&q.id))
                .cloned()
                .map(Box::new);
            (
                status_id,
                (
                    r.id,
                    super::types::QuoteInfo {
                        state: r.state(),
                        quoted_status,
                        quoted_status_id: None,
                        shallow: false,
                    },
                ),
            )
        })
        .collect())
}

pub async fn fetch_status_poll(
    state: &AppState,
    status_id: i64,
    viewer_id: Option<i64>,
) -> AppResult<Option<super::types::Poll>> {
    Ok(batch_status_polls(state, &[status_id], viewer_id)
        .await?
        .remove(&status_id))
}

pub async fn fetch_status_media(
    state: &AppState,
    status_id: i64,
) -> AppResult<Vec<crate::db::models::MediaAttachment>> {
    Ok(batch_status_media(state, &[status_id])
        .await?
        .remove(&status_id)
        .unwrap_or_default())
}

pub async fn fetch_reblog_data(
    state: &AppState,
    status: &crate::db::models::Status,
) -> AppResult<
    Option<(
        crate::db::models::Status,
        Account,
        Vec<crate::db::models::MediaAttachment>,
    )>,
> {
    let Some(reblog_id) = status.reblog_of_id else {
        return Ok(None);
    };
    let reblog = sqlx::query_as!(
        crate::db::models::Status,
        "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        reblog_id,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(reblog) = reblog else {
        return Ok(None);
    };
    let reblog = if let Some(original_id) = reblog.reblog_of_id {
        sqlx::query_as!(
            crate::db::models::Status,
            "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
            original_id,
        )
        .fetch_optional(&state.db)
        .await?
        .unwrap_or(reblog)
    } else {
        reblog
    };
    let reblog_account = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = $1",
        reblog.account_id,
    )
    .fetch_one(&state.db)
    .await?;
    let reblog_media = fetch_status_media(state, reblog.id).await?;
    Ok(Some((reblog, reblog_account, reblog_media)))
}

pub async fn fetch_statuses_tags(
    state: &AppState,
    status_id: i64,
) -> AppResult<Vec<super::types::StatusTag>> {
    let domain = &state.instance.web_domain();
    let rows = sqlx::query!(
        r#"SELECT t.name
           FROM tags t
           JOIN statuses_tags st ON st.tag_id = t.id
           WHERE st.status_id = $1
           ORDER BY t.name ASC"#,
        status_id,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            // `REST::ShallowTagSerializer#url`: `tag_url(object)`, by the
            // tag's name as it is stored.
            super::types::StatusTag {
                url: crate::formatter::text::tag_url(domain, &r.name),
                name: r.name,
            }
        })
        .collect())
}

pub async fn fetch_status_mentions(
    state: &AppState,
    status_id: i64,
) -> AppResult<Vec<super::types::StatusMention>> {
    let rows = sqlx::query!(
        r#"SELECT a.id as account_id, a.username, a.domain, a.url
           FROM accounts a
           JOIN mentions m ON m.account_id = a.id
           WHERE m.status_id = $1 AND NOT m.silent
           ORDER BY m.id ASC"#,
        status_id,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| super::types::StatusMention {
            id: r.account_id.to_string(),
            acct: match &r.domain {
                Some(d) => format!("{}@{}", r.username, d),
                None => r.username.clone(),
            },
            // `ActivityPub::TagManager#url_for`.
            url: if r.domain.is_none() {
                format!("https://{}/@{}", state.urls.web_domain, r.username)
            } else {
                r.url.unwrap_or_default()
            },
            username: r.username,
        })
        .collect())
}

pub async fn batch_statuses_tags(
    state: &AppState,
    status_ids: &[i64],
) -> AppResult<std::collections::HashMap<i64, Vec<super::types::StatusTag>>> {
    if status_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let domain = &state.instance.web_domain();
    let rows = sqlx::query!(
        r#"SELECT st.status_id, t.name
           FROM tags t
           JOIN statuses_tags st ON st.tag_id = t.id
           WHERE st.status_id = ANY($1::bigint[])
           ORDER BY t.name ASC"#,
        status_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let mut map: std::collections::HashMap<i64, Vec<super::types::StatusTag>> =
        std::collections::HashMap::new();
    for r in rows {
        map.entry(r.status_id)
            .or_default()
            .push(super::types::StatusTag {
                url: crate::formatter::text::tag_url(domain, &r.name),
                name: r.name,
            });
    }
    Ok(map)
}

pub async fn batch_status_mentions(
    state: &AppState,
    status_ids: &[i64],
) -> AppResult<std::collections::HashMap<i64, Vec<super::types::StatusMention>>> {
    if status_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows = sqlx::query!(
        r#"SELECT m.status_id, a.id as account_id, a.username, a.domain, a.url
           FROM accounts a
           JOIN mentions m ON m.account_id = a.id
           WHERE m.status_id = ANY($1::bigint[]) AND NOT m.silent
           ORDER BY m.id ASC"#,
        status_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let mut map: std::collections::HashMap<i64, Vec<super::types::StatusMention>> =
        std::collections::HashMap::new();
    for r in rows {
        map.entry(r.status_id)
            .or_default()
            .push(super::types::StatusMention {
                id: r.account_id.to_string(),
                acct: match &r.domain {
                    Some(d) => format!("{}@{}", r.username, d),
                    None => r.username.clone(),
                },
                // `ActivityPub::TagManager#url_for`.
                url: if r.domain.is_none() {
                    format!("https://{}/@{}", state.urls.web_domain, r.username)
                } else {
                    r.url.unwrap_or_default()
                },
                username: r.username,
            });
    }
    Ok(map)
}

pub async fn batch_status_emojis(
    state: &AppState,
    statuses: &[crate::db::models::Status],
) -> AppResult<std::collections::HashMap<i64, Vec<super::types::CustomEmoji>>> {
    if statuses.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    // `CustomEmoji.from_text([spoiler_text, text].join(' '), account.domain)`.
    let account_ids: Vec<i64> = statuses.iter().map(|s| s.account_id).collect();
    let domains: std::collections::HashMap<i64, Option<String>> = sqlx::query!(
        "SELECT id, domain FROM accounts WHERE id = ANY($1)",
        &account_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| (r.id, r.domain))
    .collect();
    let texts: Vec<(i64, Option<String>, String)> = statuses
        .iter()
        .map(|s| {
            (
                s.id,
                domains.get(&s.account_id).cloned().flatten(),
                format!("{} {}", s.spoiler_text, s.text),
            )
        })
        .collect();
    Ok(super::convert::emojis_from_texts(state, &texts).await)
}

/// Batch-fetch polls for a list of status IDs. Returns map from status_id → Poll.
pub async fn batch_status_polls(
    state: &AppState,
    status_ids: &[i64],
    viewer_id: Option<i64>,
) -> AppResult<std::collections::HashMap<i64, super::types::Poll>> {
    if status_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let polls = sqlx::query_as!(
        crate::db::models::Poll,
        "SELECT * FROM polls WHERE status_id = ANY($1::bigint[])",
        status_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let mut serialized = super::polls::serialize_many(state, &polls, viewer_id).await?;
    Ok(polls
        .iter()
        .filter_map(|poll| Some((poll.status_id, serialized.remove(&poll.id)?)))
        .collect())
}

/// Batch-fetch preview cards for a list of status IDs. Returns map from status_id → PreviewCard.
/// `viewer_id` is the signed-in account, for `missing_attribution`.
pub async fn batch_status_cards(
    state: &AppState,
    status_ids: &[i64],
    viewer_id: Option<i64>,
) -> AppResult<std::collections::HashMap<i64, super::types::PreviewCard>> {
    super::preview_cards::for_statuses(state, status_ids, viewer_id).await
}

/// Builds a `Status` API object with tags and mentions populated from the DB.
pub async fn build_status(
    state: &AppState,
    s: &crate::db::models::Status,
    account: &Account,
    media: Vec<crate::db::models::MediaAttachment>,
    reblog: Option<(
        crate::db::models::Status,
        Account,
        Vec<crate::db::models::MediaAttachment>,
    )>,
    viewer_ctx: Option<super::convert::StatusViewerContext>,
) -> AppResult<super::types::Status> {
    build_status_with_app(state, s, account, media, reblog, viewer_ctx, None).await
}

pub async fn build_status_with_app(
    state: &AppState,
    s: &crate::db::models::Status,
    account: &Account,
    media: Vec<crate::db::models::MediaAttachment>,
    reblog: Option<(
        crate::db::models::Status,
        Account,
        Vec<crate::db::models::MediaAttachment>,
    )>,
    viewer_ctx: Option<super::convert::StatusViewerContext>,
    application: Option<super::types::Application>,
) -> AppResult<super::types::Status> {
    let viewer_account_id = viewer_ctx.as_ref().map(|c| c.account_id);

    // Mastodon shows which app posted a status — `show_application?` — to
    // everyone unless the author turned `show_application` off, and to the
    // author always. Fetched here rather than at each call site, so a caller
    // that does not already have it still gets it.
    let application = match (application, s.application_id) {
        (Some(app), _) => Some(app),
        (None, Some(_)) => fetch_status_applications(state, &[s.id], viewer_account_id)
            .await
            .remove(&s.id),
        (None, None) => None,
    };

    // Pre-fetch mentions and emojis for content rendering and API fields
    let mentions = fetch_status_mentions(state, s.id).await?;
    let status_emojis = fetch_status_emojis(state, s).await;
    let (reblog_mentions, reblog_emojis) = if let Some((ref rs, _, _)) = reblog {
        (
            fetch_status_mentions(state, rs.id).await?,
            fetch_status_emojis(state, rs).await,
        )
    } else {
        (vec![], vec![])
    };

    let mut api = super::convert::status_from_db_with_app(
        &state.urls,
        s,
        account,
        media,
        reblog,
        viewer_ctx,
        application,
        &mentions,
        &reblog_mentions,
    );
    let id: i64 = api.id.parse().unwrap_or(0);
    api.account.emojis = fetch_account_emojis(state, account).await;
    api.account
        .set_roles(fetch_account_roles(state, account.id).await);
    api.tags = fetch_statuses_tags(state, id).await?;
    api.mentions = mentions;
    api.emojis = status_emojis;
    api.poll = fetch_status_poll(state, id, viewer_account_id).await?;
    api.card = fetch_status_card(state, id, viewer_account_id).await;
    // The boosted status keeps its own attribution, which is the one a reader
    // cares about: the boost itself was made by a client, the post was written
    // by one.
    if let Some(ref mut rb) = api.reblog {
        if rb.application.is_none() {
            if let Ok(rid) = rb.id.parse::<i64>() {
                rb.application = fetch_status_applications(state, &[rid], viewer_account_id)
                    .await
                    .remove(&rid);
            }
        }
    }
    if let Some(ref mut rb) = api.reblog {
        let rid: i64 = rb.id.parse().unwrap_or(0);
        let rb_account_id: i64 = rb.account.id.parse().unwrap_or(0);
        if rb_account_id != 0 {
            if let Ok(rb_db_acct) = fetch_account(state, rb_account_id).await {
                rb.account.emojis = fetch_account_emojis(state, &rb_db_acct).await;
                rb.account
                    .set_roles(fetch_account_roles(state, rb_account_id).await);
            }
        }
        rb.tags = fetch_statuses_tags(state, rid).await?;
        rb.mentions = reblog_mentions;
        rb.emojis = reblog_emojis;
        rb.poll = fetch_status_poll(state, rid, None).await?;
        rb.card = fetch_status_card(state, rid, viewer_account_id).await;
    }
    hydrate_status_stats(state, std::iter::once(&mut api), viewer_account_id).await;
    Ok(api)
}

/// `Status#emojis`: the emojis of the status's author's domain that its
/// spoiler text and text use.
async fn fetch_status_emojis(
    state: &AppState,
    s: &crate::db::models::Status,
) -> Vec<super::types::CustomEmoji> {
    batch_status_emojis(state, std::slice::from_ref(s))
        .await
        .unwrap_or_default()
        .remove(&s.id)
        .unwrap_or_default()
}

/// Batch-fetch `status_stats` for the given status ids.
/// Returns a map of `status_id` → `(replies_count, reblogs_count, favourites_count, quotes_count)`.
/// Statuses with no stats row are absent from the map (callers default to 0).
pub async fn batch_status_stats(
    state: &AppState,
    status_ids: &[i64],
) -> std::collections::HashMap<i64, (i64, i64, i64, i64)> {
    if status_ids.is_empty() {
        return std::collections::HashMap::new();
    }
    sqlx::query!(
        // `REST::StatusSerializer#reblogs_count` and `#favourites_count`: the
        // count a remote status's server reports, when it reported one
        // (`Status#untrusted_*_count`, never for a local status), else ours.
        r#"SELECT ss.status_id, ss.replies_count,
                  CASE WHEN NOT (COALESCE(s.local, false) OR s.uri IS NULL)
                            AND ss.untrusted_reblogs_count IS NOT NULL
                       THEN ss.untrusted_reblogs_count
                       ELSE GREATEST(ss.reblogs_count, 0) END AS "reblogs_count!",
                  CASE WHEN NOT (COALESCE(s.local, false) OR s.uri IS NULL)
                            AND ss.untrusted_favourites_count IS NOT NULL
                       THEN ss.untrusted_favourites_count
                       ELSE GREATEST(ss.favourites_count, 0) END AS "favourites_count!",
                  ss.quotes_count
           FROM status_stats ss JOIN statuses s ON s.id = ss.status_id
           WHERE ss.status_id = ANY($1::bigint[])"#,
        status_ids,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        (
            r.status_id,
            (
                r.replies_count,
                r.reblogs_count,
                r.favourites_count,
                r.quotes_count,
            ),
        )
    })
    .collect()
}

/// Populate the follower/following/statuses counts on every embedded account and
/// the reply/reblog/favourite/quote counts on every status (including reblogs)
/// of an already-built status list, reading from `account_stats` / `status_stats`
/// in two batched queries.
///
/// Mastodon serializes these real counts on every account and status; the
/// `*_from_db` converters leave them at 0, so any endpoint that returns a list
/// of statuses calls this once on the finished list before responding. Accepts
/// anything yielding `&mut Status` (a `Vec`'s `iter_mut`, a map's `values_mut`).
pub async fn hydrate_status_stats<'a>(
    state: &AppState,
    statuses: impl IntoIterator<Item = &'a mut super::types::Status>,
    viewer: impl Into<Option<i64>>,
) {
    let viewer = viewer.into();
    let mut refs: Vec<&mut super::types::Status> = statuses.into_iter().collect();
    attach_quotes(state, &mut refs, viewer).await;
    hydrate_without_quotes(state, refs, viewer).await;
}

/// [`hydrate_status_stats`] for statuses whose quotes are already attached,
/// or that are to carry none: everything but looking the quotes up.
pub(crate) async fn hydrate_without_quotes(
    state: &AppState,
    mut refs: Vec<&mut super::types::Status>,
    viewer: Option<i64>,
) {
    let mut account_ids: Vec<i64> = Vec::new();
    let mut status_ids: Vec<i64> = Vec::new();
    let mut collect = |s: &super::types::Status| {
        if let Ok(id) = s.id.parse() {
            status_ids.push(id);
        }
        if let Ok(id) = s.account.id.parse() {
            account_ids.push(id);
        }
    };
    for s in &refs {
        visit(s, &mut collect);
    }
    if status_ids.is_empty() {
        return;
    }
    let account_stats = batch_account_stats(state, &account_ids).await;
    let status_stats = batch_status_stats(state, &status_ids).await;
    let noindex = super::accounts::batch_noindex(state, &account_ids).await;
    let tagged_collections = super::collections::tagged_collections(state, &status_ids, viewer)
        .await
        .unwrap_or_default();
    // `AccountSerializer#email_subscriptions`, while the feature is enabled.
    let offering = if crate::email_subscriptions::enabled(state).await {
        Some(crate::email_subscriptions::offering(state, &account_ids).await)
    } else {
        None
    };

    let mut apply = |s: &mut super::types::Status| {
        if let Ok(aid) = s.account.id.parse::<i64>() {
            s.account.email_subscriptions = offering.as_ref().map(|o| o.contains(&aid));
            if let Some(&value) = noindex.get(&aid) {
                s.account.noindex = Some(value);
            }
            if let Some(&(statuses_c, following, followers)) = account_stats.get(&aid) {
                s.account.statuses_count = statuses_c;
                s.account.following_count = following;
                s.account.followers_count = followers;
            }
        }
        if let Ok(sid) = s.id.parse::<i64>() {
            if let Some(&(replies, reblogs, favourites, quotes)) = status_stats.get(&sid) {
                s.replies_count = replies;
                s.reblogs_count = reblogs;
                s.favourites_count = favourites;
                s.quotes_count = quotes;
            }
        }
        // `tagged_collections`.
        if let Some(collections) =
            s.id.parse::<i64>()
                .ok()
                .and_then(|sid| tagged_collections.get(&sid))
        {
            s.tagged_collections = collections.clone();
        }
    };
    for s in refs.iter_mut() {
        visit_mut(s, &mut apply);
    }

    // What `status_content_format` and `account_bio_format` need the
    // database for: the quote fallback, and the accounts a local bio mentions.
    super::formatting::apply_quote_fallbacks(state, &mut refs).await;
    let mut accounts: Vec<&mut super::types::Account> = Vec::new();
    for s in refs.iter_mut() {
        accounts_of(s, &mut accounts);
    }
    super::formatting::link_profile_mentions(state, accounts).await;
}

/// `s`, the post it boosts, and the posts their quotes embed.
fn visit(s: &super::types::Status, f: &mut impl FnMut(&super::types::Status)) {
    f(s);
    if let Some(rb) = s.reblog.as_deref() {
        visit(rb, f);
    }
    if let Some(q) = s.quote.as_ref().and_then(|q| q.quoted_status.as_deref()) {
        visit(q, f);
    }
}

/// [`visit`], mutably.
fn visit_mut(s: &mut super::types::Status, f: &mut impl FnMut(&mut super::types::Status)) {
    f(s);
    if let Some(rb) = s.reblog.as_deref_mut() {
        visit_mut(rb, f);
    }
    if let Some(q) = s
        .quote
        .as_mut()
        .and_then(|q| q.quoted_status.as_deref_mut())
    {
        visit_mut(q, f);
    }
}

/// The accounts of `s` and of every post [`visit`] reaches from it.
fn accounts_of<'a>(s: &'a mut super::types::Status, out: &mut Vec<&'a mut super::types::Account>) {
    let super::types::Status {
        account,
        reblog,
        quote,
        ..
    } = s;
    out.push(account);
    if let Some(rb) = reblog.as_deref_mut() {
        accounts_of(rb, out);
    }
    if let Some(q) = quote.as_mut().and_then(|q| q.quoted_status.as_deref_mut()) {
        accounts_of(q, out);
    }
}

/// Look up an already-cached preview card for a status. Never does network I/O.
pub(super) async fn fetch_status_card(
    state: &AppState,
    status_id: i64,
    viewer_id: Option<i64>,
) -> Option<super::types::PreviewCard> {
    super::preview_cards::for_statuses(state, &[status_id], viewer_id)
        .await
        .ok()?
        .remove(&status_id)
}

/// Which application posted each of these statuses.
///
/// Mastodon's `show_application?` is `user_shows_application? || viewer is the
/// author`: the author's `show_application` setting, which defaults to on, or
/// the author asking. An author with no user — a remote account — shows none.
/// The sync serializer cannot query, so list endpoints fetch the set in one go
/// and fill it in, as they do for emojis and counts.
pub async fn fetch_status_applications(
    state: &AppState,
    status_ids: &[i64],
    viewer_account_id: Option<i64>,
) -> std::collections::HashMap<i64, super::types::Application> {
    if status_ids.is_empty() {
        return std::collections::HashMap::new();
    }
    sqlx::query!(
        r#"SELECT s.id AS "status_id!", a.name, a.website
           FROM statuses s
           JOIN oauth_applications a ON a.id = s.application_id
           LEFT JOIN users u ON u.account_id = s.account_id
           WHERE s.id = ANY($1::bigint[])
             AND (s.account_id = $2
                  OR (u.id IS NOT NULL
                      AND COALESCE(u.settings, '') !~ '"show_application"\s*:\s*false'))"#,
        status_ids,
        viewer_account_id,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        (
            r.status_id,
            super::types::Application {
                name: r.name,
                website: r.website,
            },
        )
    })
    .collect()
}
