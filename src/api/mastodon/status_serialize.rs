//! Status serialization / hydration.
//!
//! Turns database status rows into Mastodon API `Status` entities and batch-
//! hydrates the associated media, reblogs, quotes, polls, tags, mentions,
//! emojis, cards and counters. Split out of `accounts.rs`.

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

/// Batch-fetch quoted statuses for a list of statuses. Returns a map from
/// quoting status ID → fully-built API `Status` (without the quote's own quote).
pub async fn batch_quote_data(
    state: &AppState,
    statuses: &[crate::db::models::Status],
    viewer_id: Option<i64>,
) -> AppResult<std::collections::HashMap<i64, super::types::QuoteInfo>> {
    use std::collections::{HashMap, HashSet};

    let status_ids: Vec<i64> = statuses.iter().map(|s| s.id).collect();

    // The quotes these statuses make, as `REST::StatusSerializer#quote` shows
    // them: `object.quote if object.quote&.acceptable?`, accepted or not
    // legacy.
    let quote_rows = sqlx::query!(
        r#"SELECT status_id, quoted_status_id, state FROM quotes
           WHERE status_id = ANY($1::bigint[]) AND (state = 1 OR NOT legacy)"#,
        &status_ids,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    if quote_rows.is_empty() {
        return Ok(HashMap::new());
    }

    // Map from quoting status ID → quoted status ID
    let quote_of: HashMap<i64, i64> = quote_rows
        .iter()
        .filter_map(|r| r.quoted_status_id.map(|qid| (r.status_id, qid)))
        .collect();
    let quote_states: HashMap<i64, i32> =
        quote_rows.iter().map(|r| (r.status_id, r.state)).collect();

    let quote_ids: Vec<i64> = quote_of
        .values()
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let quoted_statuses = sqlx::query_as!(
        crate::db::models::Status,
        "SELECT * FROM statuses WHERE id = ANY($1::bigint[]) AND deleted_at IS NULL",
        &quote_ids,
    )
    .fetch_all(&state.db)
    .await?;

    let account_ids: Vec<i64> = quoted_statuses
        .iter()
        .map(|s| s.account_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let accounts = if !account_ids.is_empty() {
        sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
            &account_ids,
        )
        .fetch_all(&state.db)
        .await?
    } else {
        vec![]
    };
    let account_map: HashMap<i64, Account> = accounts.into_iter().map(|a| (a.id, a)).collect();

    let qs_ids: Vec<i64> = quoted_statuses.iter().map(|s| s.id).collect();
    let (media_map, tags_map, mentions_map, emojis_map, polls_map, cards_map, ctxs) =
        if !qs_ids.is_empty() {
            let media = batch_status_media(state, &qs_ids).await?;
            let tags = batch_statuses_tags(state, &qs_ids).await?;
            let mentions = batch_status_mentions(state, &qs_ids).await?;
            let emojis = batch_status_emojis(state, &quoted_statuses).await?;
            let polls = batch_status_polls(state, &qs_ids, viewer_id).await?;
            let cards = batch_status_cards(state, &qs_ids, viewer_id).await?;
            let ctxs = if let Some(vid) = viewer_id {
                super::statuses::batch_viewer_contexts(state, vid, &qs_ids).await?
            } else {
                HashMap::new()
            };
            (media, tags, mentions, emojis, polls, cards, ctxs)
        } else {
            (
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
            )
        };

    // `StatusFilter#filter_state_for_quote` for each quoted status, as the
    // viewer sees it: nothing for their own post, `unauthorized` for one
    // `StatusPolicy#show?` hides from them, then `blocked_domain`,
    // `blocked_account` and `muted_account`.
    let mut filter_states: HashMap<i64, &'static str> = HashMap::new();
    if !quoted_statuses.is_empty() {
        let author_ids: Vec<i64> = account_ids.clone();
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
        for qs in &quoted_statuses {
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

    // Fetch shallow quote states for nested quotes (quoted statuses that themselves quote something).
    let nested_quoting_ids: Vec<i64> = quoted_statuses.iter().map(|qs| qs.id).collect();
    // (status_id → (state, quoted_status_id)) for nested quotes
    let nested_quote_info: HashMap<i64, (String, i64)> = if !nested_quoting_ids.is_empty() {
        let rows = sqlx::query!(
            "SELECT status_id, state, quoted_status_id FROM quotes WHERE status_id = ANY($1::bigint[]) AND quoted_status_id IS NOT NULL",
            &nested_quoting_ids,
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
        rows.into_iter()
            .filter_map(|r| {
                r.quoted_status_id.map(|qid| {
                    (
                        r.status_id,
                        (
                            crate::db::models::quote_state::to_str(r.state).to_owned(),
                            qid,
                        ),
                    )
                })
            })
            .collect()
    } else {
        HashMap::new()
    };

    // Build a map from quoted status id → API Status
    let mut qs_map: HashMap<i64, super::types::Status> = HashMap::new();
    for qs in &quoted_statuses {
        let Some(account) = account_map.get(&qs.account_id) else {
            continue;
        };
        let media = media_map.get(&qs.id).cloned().unwrap_or_default();
        let mentions = mentions_map.get(&qs.id).cloned().unwrap_or_default();
        let ctx = ctxs.get(&qs.id).cloned();
        let mut api = super::convert::status_from_db(
            &state.urls,
            qs,
            account,
            media,
            None,
            ctx,
            &mentions,
            &[],
        );
        api.tags = tags_map.get(&qs.id).cloned().unwrap_or_default();
        api.mentions = mentions;
        api.emojis = emojis_map.get(&qs.id).cloned().unwrap_or_default();
        api.poll = polls_map.get(&qs.id).cloned();
        api.card = cards_map.get(&qs.id).cloned();
        // Attach shallow quote info for the nested quote (ShallowQuoteSerializer behavior)
        if let Some((state_str, nested_qid)) = nested_quote_info.get(&qs.id) {
            api.quote = Some(super::types::QuoteInfo {
                state: state_str.clone(),
                quoted_status: None,
                quoted_status_id: Some(nested_qid.to_string()),
            });
        }
        qs_map.insert(qs.id, api);
    }

    // Build the final map keyed by quoting status ID → QuoteInfo, as
    // `REST::BaseQuoteSerializer` has it: a quote not accepted shows its
    // state alone; an accepted one is `deleted` when the quoted post is gone,
    // else filtered as the viewer sees the quoted post, and carries it unless
    // the viewer may not see it.
    let mut result: HashMap<i64, super::types::QuoteInfo> = HashMap::new();
    for s in statuses {
        let Some(&db_state) = quote_states.get(&s.id) else {
            continue;
        };
        let quoted_id = quote_of.get(&s.id).copied();
        let quoted = quoted_id.and_then(|qid| qs_map.get(&qid));
        let (effective_state, quoted_status) =
            if db_state != crate::db::models::quote_state::ACCEPTED {
                (
                    crate::db::models::quote_state::to_str(db_state).to_string(),
                    None,
                )
            } else if let Some(quoted) = quoted {
                let filter = quoted_id.and_then(|qid| filter_states.get(&qid).copied());
                let shown = filter != Some("unauthorized");
                (
                    filter.unwrap_or("accepted").to_string(),
                    shown.then(|| quoted.clone()),
                )
            } else {
                ("deleted".to_string(), None)
            };
        result.insert(
            s.id,
            super::types::QuoteInfo {
                state: effective_state,
                quoted_status: quoted_status.map(Box::new),
                quoted_status_id: None,
            },
        );
    }
    Ok(result)
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
    let domain = &state.instance.domain;
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
            let tag_lower = r.name.to_lowercase();
            super::types::StatusTag {
                url: format!(
                    "https://{}/tags/{}",
                    domain,
                    urlencoding::encode(&tag_lower)
                ),
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
           WHERE m.status_id = $1
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
                format!("https://{}/@{}", state.urls.local_domain, r.username)
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
    let domain = &state.instance.domain;
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
        let tag_lower = r.name.to_lowercase();
        map.entry(r.status_id)
            .or_default()
            .push(super::types::StatusTag {
                url: format!(
                    "https://{}/tags/{}",
                    domain,
                    urlencoding::encode(&tag_lower)
                ),
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
           WHERE m.status_id = ANY($1::bigint[])
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
                    format!("https://{}/@{}", state.urls.local_domain, r.username)
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
    api.account.roles = fetch_account_roles(state, account.id).await;
    api.tags = fetch_statuses_tags(state, id).await?;
    api.mentions = mentions;
    api.emojis = status_emojis;
    api.poll = fetch_status_poll(state, id, viewer_account_id).await?;
    api.card = fetch_status_card(state, id, viewer_account_id).await;
    // Populate quoted status if present (check quotes table)
    {
        let quote_statuses = vec![s.clone()];
        let qmap = batch_quote_data(state, &quote_statuses, viewer_account_id).await?;
        if let Some(qi) = qmap.into_values().next() {
            api.quote = Some(qi);
        }
    }
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
                rb.account.roles = fetch_account_roles(state, rb_account_id).await;
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
        collect(s);
        if let Some(rb) = s.reblog.as_deref() {
            collect(rb);
        }
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

    let apply = |s: &mut super::types::Status| {
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
    };
    for s in refs.iter_mut() {
        apply(s);
        if let Some(rb) = s.reblog.as_deref_mut() {
            apply(rb);
        }
    }
    // `tagged_collections`.
    if !tagged_collections.is_empty() {
        for s in refs.iter_mut() {
            let s = &mut **s;
            if let Ok(sid) = s.id.parse::<i64>() {
                if let Some(collections) = tagged_collections.get(&sid).cloned() {
                    s.tagged_collections = collections;
                }
            }
            if let Some(rb) = s.reblog.as_deref_mut() {
                if let Ok(sid) = rb.id.parse::<i64>() {
                    if let Some(collections) = tagged_collections.get(&sid).cloned() {
                        rb.tagged_collections = collections;
                    }
                }
            }
        }
    }

    // What `status_content_format` and `account_bio_format` need the
    // database for: the quote fallback, and the accounts a local bio mentions.
    super::formatting::apply_quote_fallbacks(state, &mut refs).await;
    let mut accounts: Vec<&mut super::types::Account> = Vec::new();
    for s in refs.iter_mut() {
        let s = &mut **s;
        accounts.push(&mut s.account);
        if let Some(rb) = s.reblog.as_deref_mut() {
            accounts.push(&mut rb.account);
        }
    }
    super::formatting::link_profile_mentions(state, accounts).await;
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
