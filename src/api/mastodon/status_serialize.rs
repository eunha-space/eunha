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
    let rows = sqlx::query_as!(
        crate::db::models::MediaAttachment,
        "SELECT * FROM media_attachments WHERE status_id = ANY($1::bigint[]) ORDER BY id",
        status_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let mut map: std::collections::HashMap<i64, Vec<_>> = std::collections::HashMap::new();
    for m in rows {
        if let Some(sid) = m.status_id {
            map.entry(sid).or_default().push(m);
        }
    }
    Ok(map)
}

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

    // Fetch quote relationships from the quotes table
    let quote_rows = sqlx::query!(
        "SELECT status_id, quoted_status_id FROM quotes WHERE status_id = ANY($1::bigint[]) AND quoted_status_id IS NOT NULL",
        &status_ids,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    // Map from quoting status ID → quoted status ID
    let quote_of: HashMap<i64, i64> = quote_rows
        .iter()
        .filter_map(|r| r.quoted_status_id.map(|qid| (r.status_id, qid)))
        .collect();

    let quote_ids: Vec<i64> = quote_of
        .values()
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    if quote_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let quoted_statuses = sqlx::query_as!(
        crate::db::models::Status,
        "SELECT * FROM statuses WHERE id = ANY($1::bigint[]) AND deleted_at IS NULL",
        &quote_ids,
    )
    .fetch_all(&state.db)
    .await?;

    // Also look up any soft-deleted quoted statuses (they exist but have deleted_at set)
    let found_ids: HashSet<i64> = quoted_statuses.iter().map(|s| s.id).collect();
    let deleted_ids: Vec<i64> = quote_ids
        .iter()
        .filter(|id| !found_ids.contains(*id))
        .cloned()
        .collect();

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

    // Fetch quote states for all quoting statuses that have a quoted_status_id in quotes table
    let quoting_ids: Vec<i64> = quote_of.keys().cloned().collect();
    let quote_states: HashMap<i64, String> = if !quoting_ids.is_empty() {
        let rows = sqlx::query!(
            "SELECT status_id, state FROM quotes WHERE status_id = ANY($1::bigint[])",
            &quoting_ids,
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
        rows.into_iter()
            .map(|r| {
                (
                    r.status_id,
                    crate::db::models::quote_state::to_str(r.state).to_owned(),
                )
            })
            .collect()
    } else {
        HashMap::new()
    };

    // Check block relationships between viewer and quoted status authors (for "unauthorized" state)
    let blocked_author_ids: HashSet<i64> = if let Some(vid) = viewer_id {
        let author_ids: Vec<i64> = quoted_statuses
            .iter()
            .map(|s| s.account_id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if !author_ids.is_empty() {
            sqlx::query_scalar!(
                r#"SELECT target_account_id FROM blocks WHERE account_id = $1 AND target_account_id = ANY($2::bigint[])
                   UNION
                   SELECT account_id FROM blocks WHERE target_account_id = $1 AND account_id = ANY($2::bigint[])"#,
                vid, &author_ids,
            )
            .fetch_all(&state.db)
            .await
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .collect()
        } else {
            HashSet::new()
        }
    } else {
        HashSet::new()
    };

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

    // Build the final map keyed by quoting status ID → QuoteInfo.
    // Show QuoteInfo for all states (accepted, pending, revoked, rejected).
    // The effective state is derived from the DB state, with "deleted" and "unauthorized"
    // as viewer-computed overrides per Mastodon's REST::BaseQuoteSerializer logic.
    let mut result: HashMap<i64, super::types::QuoteInfo> = HashMap::new();
    for s in statuses {
        let Some(&qid) = quote_of.get(&s.id) else {
            continue;
        };
        let state_str = quote_states
            .get(&s.id)
            .cloned()
            .unwrap_or_else(|| "accepted".to_string());

        // Derive effective display state and whether to include the quoted
        // status body. Mastodon's REST::BaseQuoteSerializer only embeds the
        // quoted status for accepted quotes; pending/rejected/revoked quotes
        // are state-only.
        let (effective_state, include_status) = if deleted_ids.contains(&qid) {
            ("deleted".to_string(), false)
        } else {
            let quoted_author_id = quoted_statuses
                .iter()
                .find(|qs| qs.id == qid)
                .map(|qs| qs.account_id);
            let unauthorized = quoted_author_id
                .map(|aid| blocked_author_ids.contains(&aid))
                .unwrap_or(false);
            if unauthorized {
                ("unauthorized".to_string(), false)
            } else {
                let include_status = state_str == "accepted";
                (state_str.clone(), include_status)
            }
        };

        let quoted_status = if include_status {
            qs_map.get(&qid).cloned()
        } else {
            None
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
    let row = sqlx::query!(
        "SELECT id, options, multiple, expires_at, account_id FROM polls WHERE status_id = $1",
        status_id,
    )
    .fetch_optional(&state.db)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let now = chrono::Utc::now().naive_utc();
    let expired = row.expires_at.is_some_and(|t| t < now);

    let option_titles: Vec<String> = row.options;

    // Compute per-option vote counts live from poll_votes.
    let per_option = sqlx::query!(
        "SELECT choice, COUNT(*)::bigint AS \"cnt!\" FROM poll_votes WHERE poll_id = $1 GROUP BY choice",
        row.id,
    )
    .fetch_all(&state.db)
    .await?;

    let mut per_option_map: std::collections::HashMap<i32, i64> = std::collections::HashMap::new();
    for r in per_option {
        per_option_map.insert(r.choice, r.cnt);
    }

    let options: Vec<super::types::PollOption> = option_titles
        .iter()
        .enumerate()
        .map(|(i, title)| super::types::PollOption {
            title: title.clone(),
            votes_count: Some(*per_option_map.get(&(i as i32)).unwrap_or(&0)),
        })
        .collect();

    // Compute aggregate counts live.
    let (votes_count, voters_count) = sqlx::query!(
        r#"SELECT COUNT(*)::bigint AS "votes!", COUNT(DISTINCT account_id)::bigint AS "voters!" FROM poll_votes WHERE poll_id = $1"#,
        row.id,
    )
    .fetch_one(&state.db)
    .await
    .map(|r| (r.votes, r.voters))
    .unwrap_or((0, 0));

    // Mastodon initialises `voters_count` to 0 for every poll and serializes the
    // column as it stands, so the field is a number whether or not the poll is
    // multiple-choice. Its documentation says otherwise; the implementation is
    // what clients are written against.
    let voters_count = Some(voters_count);

    // `voted` and `own_votes` are both `if: :current_user?` — present together
    // for any authenticated request, absent together otherwise.
    let (voted, own_votes) = if let Some(vid) = viewer_id {
        let votes = sqlx::query!(
            "SELECT choice FROM poll_votes WHERE poll_id = $1 AND account_id = $2 ORDER BY choice",
            row.id,
            vid,
        )
        .fetch_all(&state.db)
        .await?;
        let choices: Vec<i32> = votes.iter().map(|v| v.choice).collect();
        // Mastodon's `Poll#voted?` is `account.id == account_id ||
        // votes.exists?`: an author has, in effect, already answered their own
        // poll, and a client uses this to decide whether to offer the choices.
        let voted = row.account_id == vid || !choices.is_empty();
        (Some(voted), Some(choices))
    } else {
        (None, None)
    };

    Ok(Some(super::types::Poll {
        id: row.id.to_string(),
        expires_at: row.expires_at.map(super::convert::mastodon_date),
        expired,
        multiple: row.multiple,
        votes_count,
        voters_count,
        options,
        emojis: vec![],
        voted,
        own_votes,
    }))
}

pub async fn fetch_status_media(
    state: &AppState,
    status_id: i64,
) -> AppResult<Vec<crate::db::models::MediaAttachment>> {
    Ok(sqlx::query_as!(
        crate::db::models::MediaAttachment,
        "SELECT * FROM media_attachments WHERE status_id = $1 ORDER BY id",
        status_id,
    )
    .fetch_all(&state.db)
    .await?)
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
            url: r.url.unwrap_or_default(),
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
                url: r.url.unwrap_or_default(),
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

    fn extract_shortcodes(text: &str) -> Vec<String> {
        let mut codes = Vec::new();
        let mut rest = text;
        while let Some(start) = rest.find(':') {
            rest = &rest[start + 1..];
            if let Some(end) = rest.find(':') {
                let code = &rest[..end];
                if !code.is_empty() && code.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    codes.push(code.to_string());
                }
                rest = &rest[end + 1..];
            } else {
                break;
            }
        }
        codes
    }

    // Collect all shortcodes per status
    let mut status_codes: Vec<(i64, Vec<String>)> = Vec::new();
    for s in statuses {
        let combined = format!("{} {}", s.spoiler_text, s.text);
        let codes = extract_shortcodes(&combined);
        if !codes.is_empty() {
            status_codes.push((s.id, codes));
        }
    }

    let mut map: std::collections::HashMap<i64, Vec<super::types::CustomEmoji>> =
        std::collections::HashMap::new();

    if status_codes.is_empty() {
        return Ok(map);
    }

    let all_codes: Vec<String> = status_codes
        .iter()
        .flat_map(|(_, codes)| codes.iter().cloned())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let rows = sqlx::query!(
        r#"SELECT shortcode, image_remote_url, visible_in_picker
           FROM custom_emojis
           WHERE shortcode = ANY($1) AND domain IS NULL AND NOT disabled"#,
        &all_codes,
    )
    .fetch_all(&state.db)
    .await?;

    let emoji_by_code: std::collections::HashMap<String, super::types::CustomEmoji> = rows
        .into_iter()
        .map(|r| {
            let url = r.image_remote_url.unwrap_or_default();
            (
                r.shortcode.clone(),
                super::types::CustomEmoji {
                    shortcode: r.shortcode,
                    url: url.clone(),
                    static_url: url,
                    visible_in_picker: r.visible_in_picker,
                    category: None,
                    featured: None,
                },
            )
        })
        .collect();

    for (status_id, codes) in status_codes {
        let unique_codes: std::collections::HashSet<&String> = codes.iter().collect();
        let emojis: Vec<super::types::CustomEmoji> = unique_codes
            .iter()
            .filter_map(|c| emoji_by_code.get(*c).cloned())
            .collect();
        if !emojis.is_empty() {
            map.insert(status_id, emojis);
        }
    }

    Ok(map)
}

/// Batch-fetch polls for a list of status IDs. Returns map from status_id → Poll.
pub async fn batch_status_polls(
    state: &AppState,
    status_ids: &[i64],
    viewer_id: Option<i64>,
) -> AppResult<std::collections::HashMap<i64, super::types::Poll>> {
    use std::collections::HashMap;

    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let rows = sqlx::query!(
        r#"SELECT id, status_id, options, multiple, expires_at, account_id
           FROM polls WHERE status_id = ANY($1::bigint[])"#,
        status_ids,
    )
    .fetch_all(&state.db)
    .await?;

    if rows.is_empty() {
        return Ok(HashMap::new());
    }

    let poll_ids: Vec<i64> = rows.iter().map(|r| r.id).collect();

    // Batch-fetch per-option vote counts live from poll_votes.
    struct OptionCount {
        poll_id: i64,
        choice: i32,
        cnt: i64,
    }
    let option_counts: Vec<OptionCount> = sqlx::query_as!(
        OptionCount,
        "SELECT poll_id, choice, COUNT(*)::bigint AS \"cnt!\" FROM poll_votes WHERE poll_id = ANY($1::bigint[]) GROUP BY poll_id, choice",
        &poll_ids,
    )
    .fetch_all(&state.db)
    .await?;

    let mut counts_by_poll_option: HashMap<(i64, i32), i64> = HashMap::new();
    for c in option_counts {
        counts_by_poll_option.insert((c.poll_id, c.choice), c.cnt);
    }

    // Batch-fetch total votes and unique voters per poll.
    struct PollTotals {
        poll_id: i64,
        votes: i64,
        voters: i64,
    }
    let totals: Vec<PollTotals> = sqlx::query_as!(
        PollTotals,
        r#"SELECT poll_id, COUNT(*)::bigint AS "votes!", COUNT(DISTINCT account_id)::bigint AS "voters!" FROM poll_votes WHERE poll_id = ANY($1::bigint[]) GROUP BY poll_id"#,
        &poll_ids,
    )
    .fetch_all(&state.db)
    .await?;

    let mut totals_map: HashMap<i64, (i64, i64)> = HashMap::new();
    for t in totals {
        totals_map.insert(t.poll_id, (t.votes, t.voters));
    }

    // Batch-fetch the viewer's own votes.
    let vote_rows = if let Some(vid) = viewer_id {
        sqlx::query!(
            "SELECT poll_id, choice FROM poll_votes WHERE poll_id = ANY($1::bigint[]) AND account_id = $2 ORDER BY poll_id, choice",
            &poll_ids, vid,
        )
        .fetch_all(&state.db)
        .await?
    } else {
        vec![]
    };

    let mut votes_by_poll: HashMap<i64, Vec<i32>> = HashMap::new();
    for v in vote_rows {
        votes_by_poll.entry(v.poll_id).or_default().push(v.choice);
    }

    let now = chrono::Utc::now().naive_utc();
    let mut result = HashMap::new();
    for row in rows {
        let expired = row.expires_at.is_some_and(|t| t < now);
        let option_titles: Vec<String> = row.options;
        let options: Vec<super::types::PollOption> = option_titles
            .iter()
            .enumerate()
            .map(|(i, title)| {
                let cnt = *counts_by_poll_option.get(&(row.id, i as i32)).unwrap_or(&0);
                super::types::PollOption {
                    title: title.clone(),
                    votes_count: Some(cnt),
                }
            })
            .collect();

        let (votes_count, voters_count) = totals_map
            .get(&row.id)
            .map(|&(v, u)| (v, u))
            .unwrap_or((0, 0));
        // As in the single-status path: always a number, and both viewer fields
        // present together for an authenticated request.
        let voters_count = Some(voters_count);

        let (voted, own_votes) = if let Some(vid) = viewer_id {
            let votes = votes_by_poll.get(&row.id).cloned().unwrap_or_default();
            let voted = row.account_id == vid || !votes.is_empty();
            (Some(voted), Some(votes))
        } else {
            (None, None)
        };
        result.insert(
            row.status_id,
            super::types::Poll {
                id: row.id.to_string(),
                expires_at: row.expires_at.map(super::convert::mastodon_date),
                expired,
                multiple: row.multiple,
                votes_count,
                voters_count,
                options,
                emojis: vec![],
                voted,
                own_votes,
            },
        );
    }
    Ok(result)
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
    hydrate_status_stats(state, std::iter::once(&mut api)).await;
    Ok(api)
}

/// Extract `:shortcode:` patterns from status text + spoiler and look them up
/// in `custom_emojis` for the status's instance.
async fn fetch_status_emojis(
    state: &AppState,
    s: &crate::db::models::Status,
) -> Vec<super::types::CustomEmoji> {
    let combined = format!("{} {}", s.spoiler_text, s.text);
    let shortcodes: Vec<&str> = {
        let mut v = Vec::new();
        let mut rest = combined.as_str();
        while let Some(start) = rest.find(':') {
            rest = &rest[start + 1..];
            if let Some(end) = rest.find(':') {
                let code = &rest[..end];
                if !code.is_empty() && code.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    v.push(code);
                }
                rest = &rest[end + 1..];
            } else {
                break;
            }
        }
        v
    };

    if shortcodes.is_empty() {
        return vec![];
    }

    let rows = sqlx::query!(
        r#"SELECT shortcode, image_remote_url, visible_in_picker
           FROM custom_emojis
           WHERE shortcode = ANY($1) AND domain IS NULL AND NOT disabled"#,
        &shortcodes.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    rows.into_iter()
        .map(|r| {
            let url = r.image_remote_url.unwrap_or_default();
            super::types::CustomEmoji {
                shortcode: r.shortcode,
                url: url.clone(),
                static_url: url,
                visible_in_picker: r.visible_in_picker,
                category: None,
                featured: None,
            }
        })
        .collect()
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
        "SELECT status_id, replies_count, reblogs_count, favourites_count, quotes_count
         FROM status_stats WHERE status_id = ANY($1::bigint[])",
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
) {
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
    // `AccountSerializer#email_subscriptions`, while the feature is enabled.
    let offering = if crate::email_subscriptions::enabled(state).await {
        Some(crate::email_subscriptions::offering(state, &account_ids).await)
    } else {
        None
    };

    let apply = |s: &mut super::types::Status| {
        if let Ok(aid) = s.account.id.parse::<i64>() {
            s.account.email_subscriptions = offering.as_ref().map(|o| o.contains(&aid));
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
