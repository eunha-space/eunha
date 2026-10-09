use axum::{
    extract::{Extension, FromRequest, Path, Query, RawQuery},
    http::{HeaderMap, Uri},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use super::extractors::rails;
use std::collections::HashMap;

use super::{
    accounts::{batch_account_emojis, batch_account_roles, batch_accounts_to_api},
    convert::{account_from_db, status_from_db},
    status_serialize::{
        batch_reblog_data, batch_status_cards, batch_status_emojis, batch_status_media,
        batch_status_mentions, batch_status_polls, batch_statuses_tags, build_status,
        fetch_reblog_data, fetch_status_media, hydrate_status_stats,
    },
    types::{PaginationParams, Status, StatusContext, StatusEdit, StatusSource},
};
use crate::{
    db::models::{Account, Status as DbStatus},
    error::{AppError, AppResult},
    middleware::{AuthenticatedUser, ResolvedInstance},
    push,
    state::AppState,
};

mod post;
pub use post::post_status;
pub(crate) use post::{process_status, validate_media, PostError, Posting};
mod context;
pub use context::get_status_context;
mod edit;
pub(crate) use edit::status_edits;
pub use edit::{edit_status, get_status_history, get_status_source};
mod quotes;
pub use quotes::{get_status_quotes, revoke_quote};

#[derive(Debug, Deserialize, Default)]
pub struct PollForm {
    #[serde(default, deserialize_with = "rails::strings")]
    pub options: Vec<String>,
    #[serde(default, deserialize_with = "rails::opt_int")]
    pub expires_in: Option<i64>,
    #[serde(default, deserialize_with = "rails::opt_bool")]
    pub multiple: Option<bool>,
    #[serde(default, deserialize_with = "rails::opt_bool")]
    pub hide_totals: Option<bool>,
}

/// Embed the (context-less) quote `note` as a QuoteRequest's `instrument`,
/// folding the Note's JSON-LD term definitions into the request's compound
/// `@context` so the embedded terms (`quote`, `Hashtag`, `sensitive`, …) still
/// resolve. Mirrors how [`crate::api::ap::note::NoteBundle::into_create`] hoists
/// the note context to the activity's top level.
fn inline_quote_instrument(request: &mut serde_json::Value, note: serde_json::Value) {
    let note_ctx = crate::api::ap::note::note_context();
    if let (Some(req_terms), Some(note_terms)) = (
        request
            .get_mut("@context")
            .and_then(serde_json::Value::as_array_mut)
            .and_then(|ctx| ctx.get_mut(1))
            .and_then(serde_json::Value::as_object_mut),
        note_ctx
            .as_array()
            .and_then(|ctx| ctx.get(1))
            .and_then(serde_json::Value::as_object),
    ) {
        for (key, value) in note_terms {
            req_terms
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
    }
    request["instrument"] = note;
}

/// Maximum media attachments per status (Mastodon `Status::MEDIA_ATTACHMENTS_LIMIT`).
const MEDIA_ATTACHMENTS_LIMIT: usize = 4;

/// Poll limits, matching Mastodon's `PollOptionsValidator` /
/// `PollExpirationValidator`.
const POLL_MAX_OPTIONS: usize = 4;
const POLL_MAX_OPTION_CHARS: usize = 50;
const POLL_MIN_EXPIRATION: i64 = 5 * 60; // 5 minutes
const POLL_MAX_EXPIRATION: i64 = 2_629_746; // ActiveSupport `1.month`

/// Ruby's `String#blank?`: empty, or nothing but whitespace.
pub(crate) fn blank(s: &str) -> bool {
    s.chars().all(char::is_whitespace)
}

/// `Poll#prepare_options`: `options.map(&:strip).compact_blank`, run before
/// a local poll is validated and saved. (`strip` takes off ASCII whitespace
/// and NUL; `compact_blank` drops any option then blank.)
pub(crate) fn prepare_poll_options(options: &[String]) -> Vec<String> {
    options
        .iter()
        .map(|o| {
            o.trim_matches(|c: char| {
                matches!(c, '\0' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ')
            })
            .to_owned()
        })
        .filter(|o| !blank(o))
        .collect()
}

/// The errors a local `Poll` with these options and `expires_in` fails
/// validation with, in the order Rails adds them: `options` and `expires_at`
/// present, `PollOptionsValidator`, `PollExpirationValidator`. `nested` names
/// the attributes as a status's `poll_attributes` do (`Poll options`), for a
/// poll saved with its status; otherwise as the poll's own (`Options`).
fn poll_errors(options: &[String], expires_in: Option<i64>, nested: bool) -> Vec<String> {
    use unicode_segmentation::UnicodeSegmentation;

    let (opts, expires) = if nested {
        ("Poll options", "Poll expires at")
    } else {
        ("Options", "Expires at")
    };
    let mut errors = Vec::new();
    if options.is_empty() {
        errors.push(format!("{opts} can't be blank"));
    }
    if expires_in.is_none() {
        errors.push(format!("{expires} can't be blank"));
    }
    if options.len() <= 1 {
        errors.push(format!("{opts} must have more than one item"));
    }
    if options.len() > POLL_MAX_OPTIONS {
        errors.push(format!(
            "{opts} can't contain more than {POLL_MAX_OPTIONS} items"
        ));
    }
    if options
        .iter()
        .any(|o| o.graphemes(true).count() > POLL_MAX_OPTION_CHARS)
    {
        errors.push(format!(
            "{opts} cannot be longer than {POLL_MAX_OPTION_CHARS} characters each"
        ));
    }
    let mut seen = std::collections::HashSet::new();
    if !options.iter().all(|o| seen.insert(o)) {
        errors.push(format!("{opts} contain duplicate items"));
    }
    match expires_in {
        Some(secs) if secs > POLL_MAX_EXPIRATION => {
            errors.push(format!("{expires} is too far into the future"));
        }
        Some(secs) if secs < POLL_MIN_EXPIRATION => {
            errors.push(format!("{expires} is too soon"));
        }
        _ => {}
    }
    errors
}

/// `poll.save!` for a poll an edit gives: its options prepared, then
/// validated as the poll's own, `Validation failed: …` when it is not valid.
fn validate_poll_form(poll: &PollForm) -> AppResult<Vec<String>> {
    let options = prepare_poll_options(&poll.options);
    let errors = poll_errors(&options, poll.expires_in, false);
    if errors.is_empty() {
        Ok(options)
    } else {
        Err(AppError::Unprocessable(format!(
            "Validation failed: {}",
            errors.join(", ")
        )))
    }
}

/// The errors a local post fails `Status`'s validations with, in the order
/// Rails adds them: `text` present unless the post has media, is a boost or
/// quotes (`blank?`, so whitespace is no text, and a poll does not count),
/// `StatusLengthValidator`, `DisallowedHashtagsValidator`, then, for a post
/// saved with its poll, the poll's own (`poll_attributes`).
pub(crate) async fn status_errors(
    state: &AppState,
    text: &str,
    spoiler_text: &str,
    exempt_from_text: bool,
    poll: Option<(&[String], Option<i64>)>,
) -> AppResult<Vec<String>> {
    let mut errors = Vec::new();
    if !exempt_from_text && blank(text) {
        errors.push("Text can't be blank".to_owned());
    }
    if crate::api::mastodon::formatting::countable_length(text, spoiler_text) > 500 {
        errors.push("Text character limit of 500 exceeded".to_owned());
    }
    // `Tag.matching_name(Extractor.extract_hashtags(text)).reject(&:usable?)`.
    let names: Vec<String> = extract_hashtags(text)
        .iter()
        .map(|tag| crate::search::tags::normalize(tag))
        .filter(|name| !name.is_empty())
        .collect();
    if !names.is_empty() {
        let disallowed: Vec<String> = sqlx::query_scalar!(
            "SELECT name FROM tags WHERE lower(name) = ANY($1) AND usable = false ORDER BY id",
            &names,
        )
        .fetch_all(&state.db)
        .await?;
        match disallowed.len() {
            0 => {}
            1 => errors.push(format!(
                "Text contained a disallowed hashtag: {}",
                disallowed[0]
            )),
            _ => errors.push(format!(
                "Text contained the disallowed hashtags: {}",
                disallowed.join(", ")
            )),
        }
    }
    if let Some((options, expires_in)) = poll {
        errors.extend(poll_errors(options, expires_in, true));
    }
    Ok(errors)
}

#[derive(Debug, Deserialize, Default)]
pub struct PostStatusForm {
    #[serde(default, deserialize_with = "rails::opt_string")]
    pub status: Option<String>,
    #[serde(default, deserialize_with = "rails::opt_present")]
    pub in_reply_to_id: Option<String>,
    #[serde(alias = "quote_id", default, deserialize_with = "rails::opt_present")]
    pub quoted_status_id: Option<String>,
    #[serde(default, deserialize_with = "rails::opt_present")]
    pub quote_approval_policy: Option<String>,
    #[serde(default, deserialize_with = "rails::opt_present")]
    pub spoiler_text: Option<String>,
    #[serde(default, deserialize_with = "rails::opt_bool")]
    pub sensitive: Option<bool>,
    #[serde(default, deserialize_with = "rails::opt_present")]
    pub language: Option<String>,
    #[serde(default, deserialize_with = "rails::opt_string")]
    pub visibility: Option<String>,
    #[serde(default, deserialize_with = "rails::opt_present_strings")]
    pub media_ids: Option<Vec<String>>,
    #[serde(default, deserialize_with = "poll_form")]
    pub poll: Option<PollForm>,
    #[serde(default, deserialize_with = "rails::opt_present")]
    pub scheduled_at: Option<String>,
    #[serde(default, deserialize_with = "rails::opt_strings")]
    pub allowed_mentions: Option<Vec<String>>,
}

/// `poll`: a hash of its parameters; anything else (a form's lone `poll=`)
/// is none.
pub(crate) fn poll_form<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<PollForm>, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Object(map) => serde_json::from_value(serde_json::Value::Object(map))
            .map(Some)
            .map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}

// ── GET /api/v1/statuses (batch) ──────────────────────────────────────────

pub async fn get_statuses_batch(
    state: AppState,
    RawQuery(qs): RawQuery,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Vec<Status>>> {
    let viewer_id = auth.as_ref().map(|Extension(a)| a.account_id);

    let ids: Vec<i64> = url::form_urlencoded::parse(qs.as_deref().unwrap_or("").as_bytes())
        .filter(|(k, _)| k == "id[]" || k == "id")
        .filter_map(|(_, v)| v.parse::<i64>().ok())
        .collect();

    if ids.len() > 20 {
        return Err(AppError::Unprocessable("Too many IDs requested".into()));
    }

    if ids.is_empty() {
        return Ok(Json(vec![]));
    }

    let statuses: Vec<DbStatus> = sqlx::query_as!(
        DbStatus,
        "SELECT * FROM statuses WHERE id = ANY($1::bigint[]) AND deleted_at IS NULL",
        &ids,
    )
    .fetch_all(&state.db)
    .await?;

    if statuses.is_empty() {
        return Ok(Json(vec![]));
    }

    // Batch block check
    let blocked_account_ids: std::collections::HashSet<i64> = if let Some(vid) = viewer_id {
        let other_ids: Vec<i64> = statuses
            .iter()
            .filter(|s| s.account_id != vid)
            .map(|s| s.account_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        if other_ids.is_empty() {
            std::collections::HashSet::new()
        } else {
            sqlx::query_scalar!(
                r#"SELECT target_account_id FROM blocks WHERE account_id = $1 AND target_account_id = ANY($2::bigint[])
                   UNION
                   SELECT account_id FROM blocks WHERE target_account_id = $1 AND account_id = ANY($2::bigint[])"#,
                vid, &other_ids,
            )
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .flatten()
            .collect()
        }
    } else {
        std::collections::HashSet::new()
    };

    // Batch follow check for private statuses
    let private_author_ids: Vec<i64> = statuses
        .iter()
        .filter(|s| {
            s.visibility == crate::db::models::vis::PRIVATE && viewer_id != Some(s.account_id)
        })
        .map(|s| s.account_id)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let followed_ids: std::collections::HashSet<i64> = if let (Some(vid), false) =
        (viewer_id, private_author_ids.is_empty())
    {
        sqlx::query_scalar!(
            "SELECT target_account_id FROM follows WHERE account_id = $1 AND target_account_id = ANY($2::bigint[])",
            vid, &private_author_ids,
        )
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .collect()
    } else {
        std::collections::HashSet::new()
    };

    // Batch mention check for statuses whose visibility can be granted by mention.
    let mention_checked_ids: Vec<i64> = statuses
        .iter()
        .filter(|s| {
            matches!(
                s.visibility,
                crate::db::models::vis::PRIVATE
                    | crate::db::models::vis::DIRECT
                    | crate::db::models::vis::LIMITED
            ) && viewer_id != Some(s.account_id)
        })
        .map(|s| s.id)
        .collect();
    let mentioned_status_ids: std::collections::HashSet<i64> = if let (Some(vid), false) =
        (viewer_id, mention_checked_ids.is_empty())
    {
        sqlx::query_scalar!(
            "SELECT status_id FROM mentions WHERE account_id = $1 AND status_id = ANY($2::bigint[])",
            vid, &mention_checked_ids,
        )
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .collect()
    } else {
        std::collections::HashSet::new()
    };

    let visible: Vec<DbStatus> = statuses
        .into_iter()
        .filter(|s| {
            if viewer_id != Some(s.account_id) && blocked_account_ids.contains(&s.account_id) {
                return false;
            }
            match s.visibility {
                crate::db::models::vis::PRIVATE => {
                    viewer_id == Some(s.account_id)
                        || followed_ids.contains(&s.account_id)
                        || mentioned_status_ids.contains(&s.id)
                }
                crate::db::models::vis::DIRECT | crate::db::models::vis::LIMITED => {
                    viewer_id == Some(s.account_id) || mentioned_status_ids.contains(&s.id)
                }
                _ => true,
            }
        })
        .collect();

    if visible.is_empty() {
        return Ok(Json(vec![]));
    }

    let account_ids: Vec<i64> = visible
        .iter()
        .map(|s| s.account_id)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let accounts_vec: Vec<Account> = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
        &account_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let account_map: HashMap<i64, Account> = accounts_vec.into_iter().map(|a| (a.id, a)).collect();

    let all_ids: Vec<i64> = visible.iter().map(|s| s.id).collect();
    let media_map = batch_status_media(&state, &all_ids).await?;
    let reblog_map = batch_reblog_data(&state, &visible).await?;
    let reblog_ids: Vec<i64> = reblog_map.values().map(|(rs, _, _)| rs.id).collect();
    let mut enrich_ids = all_ids.clone();
    enrich_ids.extend_from_slice(&reblog_ids);
    let tags_map = batch_statuses_tags(&state, &enrich_ids).await?;
    let mentions_map = batch_status_mentions(&state, &enrich_ids).await?;
    let all_for_emoji: Vec<DbStatus> = visible
        .iter()
        .cloned()
        .chain(reblog_map.values().map(|(rs, _, _)| rs.clone()))
        .collect();
    let emojis_map = batch_status_emojis(&state, &all_for_emoji).await?;
    let polls_map = batch_status_polls(&state, &enrich_ids, viewer_id).await?;
    let cards_map = batch_status_cards(&state, &enrich_ids, viewer_id).await?;
    let viewer_ctxs = if let Some(vid) = viewer_id {
        batch_viewer_contexts(&state, vid, &all_ids).await?
    } else {
        HashMap::new()
    };
    // Preserve original request order
    let id_order: HashMap<i64, usize> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    let mut indexed: Vec<(usize, Status)> = Vec::with_capacity(visible.len());
    for s in &visible {
        let Some(account) = account_map.get(&s.account_id) else {
            continue;
        };
        let media = media_map.get(&s.id).cloned().unwrap_or_default();
        let reblog = reblog_map.get(&s.id).cloned();
        let mentions = mentions_map.get(&s.id).cloned().unwrap_or_default();
        let rb_mentions = reblog
            .as_ref()
            .and_then(|(rs, _, _)| mentions_map.get(&rs.id))
            .cloned()
            .unwrap_or_default();
        let ctx = viewer_ctxs.get(&s.id).cloned();
        let mut api = status_from_db(
            &state.urls,
            s,
            account,
            media,
            reblog,
            ctx,
            &mentions,
            &rb_mentions,
        );
        api.tags = tags_map.get(&s.id).cloned().unwrap_or_default();
        api.mentions = mentions;
        api.emojis = emojis_map.get(&s.id).cloned().unwrap_or_default();
        api.poll = polls_map.get(&s.id).cloned();
        api.card = cards_map.get(&s.id).cloned();
        if let Some(ref mut rb) = api.reblog {
            let rid: i64 = rb.id.parse().unwrap_or(0);
            rb.tags = tags_map.get(&rid).cloned().unwrap_or_default();
            rb.mentions = rb_mentions;
            rb.emojis = emojis_map.get(&rid).cloned().unwrap_or_default();
            rb.poll = polls_map.get(&rid).cloned();
            rb.card = cards_map.get(&rid).cloned();
        }
        let order = id_order.get(&s.id).copied().unwrap_or(usize::MAX);
        indexed.push((order, api));
    }
    indexed.sort_by_key(|(i, _)| *i);
    let mut out: Vec<Status> = indexed.into_iter().map(|(_, s)| s).collect();
    hydrate_status_stats(&state, out.iter_mut(), viewer_id).await;
    Ok(Json(out))
}

// ── GET /api/v1/statuses/:id ──────────────────────────────────────────────

pub async fn get_status(
    state: AppState,
    Path(id): Path<i64>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Status>> {
    // `Status.find`, under the default scope: a status that was discarded but
    // kept for moderators is not found, as one destroyed is not.
    let (status, account) = fetch_status_with_account(&state, id).await?;

    let viewer_id = auth.as_ref().map(|Extension(a)| a.account_id);

    // Block check: if viewer is not the author and there's a block in either direction, 404.
    if let Some(vid) = viewer_id {
        if vid != status.account_id {
            let blocked = sqlx::query_scalar!(
                r#"SELECT 1 FROM blocks
                   WHERE (account_id = $1 AND target_account_id = $2)
                      OR (account_id = $2 AND target_account_id = $1)"#,
                vid,
                status.account_id
            )
            .fetch_optional(&state.db)
            .await?
            .is_some();
            if blocked {
                return Err(AppError::NotFound);
            }
        }
    }

    match status.visibility {
        crate::db::models::vis::PRIVATE => {
            let is_author = viewer_id == Some(status.account_id);
            let is_follower = if let Some(vid) = viewer_id {
                sqlx::query_scalar!(
                    "SELECT 1 as e FROM follows WHERE account_id = $1 AND target_account_id = $2",
                    vid,
                    status.account_id
                )
                .fetch_optional(&state.db)
                .await?
                .is_some()
            } else {
                false
            };
            let is_mentioned = if let Some(vid) = viewer_id {
                sqlx::query_scalar!(
                    "SELECT 1 as e FROM mentions WHERE status_id = $1 AND account_id = $2",
                    id,
                    vid,
                )
                .fetch_optional(&state.db)
                .await?
                .is_some()
            } else {
                false
            };
            if !is_author && !is_follower && !is_mentioned {
                return Err(AppError::NotFound);
            }
        }
        crate::db::models::vis::DIRECT | crate::db::models::vis::LIMITED
            if viewer_id != Some(status.account_id) =>
        {
            let is_mentioned = if let Some(vid) = viewer_id {
                sqlx::query_scalar!(
                    "SELECT 1 as e FROM mentions WHERE status_id = $1 AND account_id = $2",
                    id,
                    vid,
                )
                .fetch_optional(&state.db)
                .await?
                .is_some()
            } else {
                false
            };
            if !is_mentioned {
                return Err(AppError::NotFound);
            }
        }
        _ => {}
    }

    let media = fetch_status_media(&state, id).await?;
    let reblog = fetch_reblog_data(&state, &status).await?;
    let viewer_ctx = if let Some(Extension(auth)) = auth {
        Some(build_viewer_context(&state, auth.account_id, id).await?)
    } else {
        None
    };
    // `show_application?`: the author's setting, or the author asking.
    let application = super::status_serialize::fetch_status_applications(
        &state,
        &[status.id],
        viewer_ctx.as_ref().map(|c| c.account_id),
    )
    .await
    .remove(&status.id);

    let s = super::status_serialize::build_status_with_app(
        &state,
        &status,
        &account,
        media,
        reblog,
        viewer_ctx,
        application,
    )
    .await?;
    Ok(Json(s))
}

// ── DELETE /api/v1/statuses/:id ────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct DeleteStatusParams {
    #[serde(default)]
    pub delete_media: Option<super::extractors::FlexBool>,
}

/// `Api::V1::StatusesController#destroy`: the status rendered as it was, then
/// discarded with its boosts and unpinned, and removed by
/// `RemovalWorker`, which keeps its media for a redraft unless
/// `delete_media` is given. Eunha removes it before answering.
pub async fn delete_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    super::extractors::Params(params): super::extractors::Params<DeleteStatusParams>,
) -> AppResult<Json<Status>> {
    auth.require_scope("write:statuses")?;
    let (status, _account) = fetch_status_with_account(&state, id).await?;
    // Mastodon scopes to `current_account.statuses.find`, so another user's
    // status is a 404 (not 403) — it also avoids confirming the status exists.
    if status.account_id != auth.account_id {
        return Err(AppError::NotFound);
    }

    // Rendered before `discard_with_reblogs`, for the media's own URLs.
    let mut s = serialize_status(&state, &status, None).await?;
    s.text = Some(status.text.clone());

    crate::remove_status::discard_with_reblogs(&state, &status).await?;
    sqlx::query!("DELETE FROM status_pins WHERE status_id = $1", id)
        .execute(&state.db)
        .await?;
    let delete_media = params.delete_media.is_some_and(|b| b.0);
    crate::remove_status::call(
        &state,
        id,
        crate::remove_status::Options {
            redraft: !delete_media,
            ..Default::default()
        },
    )
    .await?;

    Ok(Json(s))
}

// ── POST /api/v1/statuses/:id/favourite ───────────────────────────────────

/// The id of a favourite's `Like`, `ActivityPub::LikeSerializer#id`: the
/// actor's URI, `#likes/` and the `favourites` row's id. The `Undo` of it is
/// this with `/undo`.
fn like_id(actor_url: &str, favourite_id: i64) -> String {
    format!("{actor_url}#likes/{favourite_id}")
}

pub async fn favourite_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Status>> {
    auth.require_scope("write:favourites")?;
    let (s, _) = fetch_status_with_account(&state, id).await?;
    check_status_visible(&state, &s, auth.account_id).await?;

    let favourite_id = sqlx::query_scalar!(
        "INSERT INTO favourites (account_id, status_id, created_at, updated_at) VALUES ($1,$2, now(), now()) ON CONFLICT DO NOTHING RETURNING id",
        auth.account_id, id
    )
    .fetch_optional(&state.db)
    .await?;
    let favourited = favourite_id.is_some();
    if favourited {
        crate::fasp::events::favourite_created(&state, id).await;
        // `FavouriteService#increment_statistics`, for a new favourite.
        crate::activity_tracker::increment(&state, crate::activity_tracker::INTERACTIONS).await;
    }
    // `Favourite`'s `update_index('statuses', :status)`.
    crate::search::elasticsearch::indexing::status_interaction(&state, id).await;

    sqlx::query!(
        r#"INSERT INTO status_stats (status_id, favourites_count, created_at, updated_at)
           VALUES ($1, 1, now(), now())
           ON CONFLICT (status_id) DO UPDATE
             SET favourites_count = (SELECT COUNT(*) FROM favourites WHERE status_id = $1),
                 untrusted_favourites_count = CASE WHEN status_stats.untrusted_favourites_count IS NULL THEN NULL ELSE LEAST(GREATEST(status_stats.untrusted_favourites_count + (SELECT COUNT(*) FROM favourites WHERE status_id = $1) - status_stats.favourites_count, 0), 100000000) END,
                 updated_at = now()"#,
        id
    )
    .execute(&state.db)
    .await?;
    // `FavouriteService`: `Trends.statuses.register`, for a new favourite.
    if favourited {
        crate::trends::register_status(&state, id).await;
    }

    let (status, account) = fetch_status_with_account(&state, id).await?;

    // Notify status author
    let from_account = fetch_account(&state, auth.account_id).await?;
    push::create_and_push(
        &state,
        status.account_id,
        auth.account_id,
        "favourite",
        Some(id),
    )
    .await;

    // `FavouriteService#create_notification`: a new favourite of a remote
    // account's post is a `Like` to that account's own inbox, if it speaks
    // ActivityPub (`status.account.activitypub?`).
    if let Some(favourite_id) =
        favourite_id.filter(|_| account.domain.is_some() && account.is_activitypub())
    {
        if crate::federation::keypair::has_signing_key(&state, from_account.id)
            .await
            .unwrap_or(false)
        {
            let domain = state.instance.domain.clone();
            let actor_url = crate::federation::tag::account_uri_of(&domain, &from_account);
            // `ActivityPub::LikeSerializer#id`.
            let like_id = like_id(&actor_url, favourite_id);
            let status_uri = status.uri.clone().unwrap_or_default();
            let like = crate::federation::activity::like(&like_id, &actor_url, &status_uri)?;
            let key_id = format!("{}#main-key", actor_url);
            if let Err(e) = crate::federation::delivery::deliver_to_inboxes(
                &state,
                like,
                vec![account.inbox_url.clone()],
                key_id,
            )
            .await
            {
                tracing::warn!(error = %e, "failed to enqueue Like delivery");
            }
        }
    }

    Ok(Json(
        serialize_status(&state, &status, Some(auth.account_id)).await?,
    ))
}

// ── POST /api/v1/statuses/:id/unfavourite ─────────────────────────────────

pub async fn unfavourite_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Status>> {
    auth.require_scope("write:favourites")?;
    let (s, account) = fetch_status_with_account(&state, id).await?;
    check_status_visible(&state, &s, auth.account_id).await?;

    let unfavourited = sqlx::query_scalar!(
        "DELETE FROM favourites WHERE account_id = $1 AND status_id = $2 RETURNING id",
        auth.account_id,
        id
    )
    .fetch_optional(&state.db)
    .await?;
    if let Some(favourite_id) = unfavourited {
        // `Favourite`'s `has_one :notification, dependent: :destroy`.
        crate::remove_status::destroy_notification_of(&state.db, "Favourite", favourite_id).await?;
        crate::statuses_cleanup::invalidate_cleanup_info(
            &state,
            auth.account_id,
            id,
            crate::statuses_cleanup::Undone::Unfav,
        )
        .await;
    }
    crate::search::elasticsearch::indexing::status_interaction(&state, id).await;

    sqlx::query!(
        r#"UPDATE status_stats SET favourites_count = (SELECT COUNT(*) FROM favourites WHERE status_id = $1),
               untrusted_favourites_count = CASE WHEN untrusted_favourites_count IS NULL THEN NULL ELSE LEAST(GREATEST(untrusted_favourites_count + (SELECT COUNT(*) FROM favourites WHERE status_id = $1) - favourites_count, 0), 100000000) END,
               updated_at = now()
           WHERE status_id = $1"#,
        id
    )
    .execute(&state.db)
    .await?;

    // `UnfavouriteService`: the favourite undone, of a remote account's
    // post, is an `Undo(Like)` to that account's own inbox, if it speaks
    // ActivityPub.
    if let Some(favourite_id) =
        unfavourited.filter(|_| account.domain.is_some() && account.is_activitypub())
    {
        if let Some(actor_row) = sqlx::query!(
            "SELECT username, id_scheme FROM accounts WHERE id = $1 AND domain IS NULL",
            auth.account_id,
        )
        .fetch_optional(&state.db)
        .await?
        {
            if crate::federation::keypair::has_signing_key(&state, auth.account_id)
                .await
                .unwrap_or(false)
            {
                let domain = state.instance.domain.clone();
                let actor_url = crate::federation::tag::account_uri(
                    &domain,
                    auth.account_id,
                    actor_row.id_scheme,
                    &actor_row.username,
                );
                // `ActivityPub::UndoLikeSerializer#id`, and its `Like`'s.
                let like_id = like_id(&actor_url, favourite_id);
                let status_uri = s.uri.clone().unwrap_or_default();
                let undo_id = format!("{like_id}/undo");
                let undo = crate::federation::activity::undo_like(
                    &undo_id,
                    &actor_url,
                    &like_id,
                    &status_uri,
                )?;
                let key_id = format!("{}#main-key", actor_url);
                if let Err(e) = crate::federation::delivery::deliver_to_inboxes(
                    &state,
                    undo,
                    vec![account.inbox_url.clone()],
                    key_id,
                )
                .await
                {
                    tracing::warn!(error = %e, "failed to enqueue Undo(Like) delivery");
                }
            }
        }
    }

    let (status, _) = fetch_status_with_account(&state, id).await?;
    Ok(Json(
        serialize_status(&state, &status, Some(auth.account_id)).await?,
    ))
}

// ── POST /api/v1/statuses/:id/reblog ──────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct ReblogForm {
    #[serde(default, deserialize_with = "rails::opt_string")]
    pub visibility: Option<String>,
}

pub async fn reblog_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    super::extractors::Params(form): super::extractors::Params<ReblogForm>,
) -> AppResult<Json<Status>> {
    auth.require_scope("write:statuses")?;
    let (fetched, _) = fetch_status_with_account(&state, id).await?;
    // If this is itself a reblog, boost the original instead
    let original_id = fetched.reblog_of_id.unwrap_or(id);
    let original = if original_id != id {
        let (o, _) = fetch_status_with_account(&state, original_id).await?;
        o
    } else {
        fetched
    };
    // visibility check: 404 if not visible, 403 if visible but not rebloggable
    check_status_visible(&state, &original, auth.account_id).await?;
    // direct messages are never rebloggable; private statuses only by their author
    if matches!(
        original.visibility,
        crate::db::models::vis::DIRECT | crate::db::models::vis::LIMITED
    ) || (original.visibility == crate::db::models::vis::PRIVATE
        && original.account_id != auth.account_id)
    {
        return Err(AppError::Forbidden);
    }

    // Reject an unrecognized requested visibility rather than coercing to direct.
    if let Some(v) = form.visibility.as_deref() {
        if !matches!(v, "public" | "unlisted" | "private" | "direct") {
            return Err(AppError::Unprocessable(format!(
                "Validation failed: Visibility is not included in the list: {v}"
            )));
        }
    }

    let boost_account = fetch_account(&state, auth.account_id).await?;

    // Determine visibility: hidden originals keep their own visibility;
    // otherwise use the requested visibility or fall back to the user's default.
    let boost_visibility = if matches!(
        original.visibility,
        crate::db::models::vis::PRIVATE
            | crate::db::models::vis::DIRECT
            | crate::db::models::vis::LIMITED
    ) {
        // Hidden originals keep their own visibility (Mastodon: reblogged_status.hidden?).
        original.visibility
    } else {
        match form.visibility.as_deref() {
            Some(v) => crate::db::models::vis::from_str(v),
            // Mastodon falls back to the booster's default posting privacy.
            None => {
                let defaults = super::accounts::user_defaults(&state, auth.account_id).await;
                crate::db::models::vis::from_str(&defaults.privacy)
            }
        }
    };

    // `ReblogsController#create`: `ReblogService` under
    // `with_redis_lock("reblog:<account>:<status>")`, which, held by another,
    // raises `RaceConditionError`.
    let lock = crate::redis_lock::try_acquire_lockable(
        &state,
        &format!("reblog:{}:{id}", auth.account_id),
        crate::redis_lock::DEFAULT_TTL_MS,
    )
    .await
    .ok_or_else(|| {
        AppError::ServiceUnavailable(
            "There was a temporary problem serving your request, please try again".into(),
        )
    })?;
    let result: AppResult<Json<Status>> = async {
    // Idempotent: if already reblogged, return the existing boost
    let existing = sqlx::query_as!(
        DbStatus,
        "SELECT * FROM statuses WHERE account_id = $1 AND reblog_of_id = $2 AND deleted_at IS NULL",
        auth.account_id,
        original_id,
    )
    .fetch_optional(&state.db)
    .await?;
    if let Some(boost) = existing {
        let ctx = build_viewer_context(&state, auth.account_id, original_id).await?;
        let media = fetch_status_media(&state, boost.id).await?;
        let reblog = fetch_reblog_data(&state, &boost).await?;
        return Ok(Json(
            build_status(&state, &boost, &boost_account, media, reblog, Some(ctx)).await?,
        ));
    }

    let boost_id = crate::snowflake::next_id();
    // `Status#store_uri`: a local boost's `uri` is `uri_for` it, the id of
    // its `Announce`.
    let boost_uri = crate::federation::tag::activity_uri(
        &state.instance.domain,
        boost_account.id,
        boost_account.id_scheme,
        &boost_account.username,
        boost_id,
    );
    let boost = sqlx::query_as!(
        DbStatus,
        r#"INSERT INTO statuses (id, account_id, text, visibility, reblog_of_id, local, uri, created_at, updated_at)
           VALUES ($1,$2,'',$3,$4, true, $5, now(), now())
           RETURNING *"#,
        boost_id,
        auth.account_id,
        boost_visibility,
        original_id,
        boost_uri,
    )
    .fetch_one(&state.db)
    .await?;
    // `set_conversation`: a boost starts a conversation of its own.
    crate::conversation::assign(&state.db, boost.id).await?;

    sqlx::query!(
        r#"INSERT INTO status_stats (status_id, reblogs_count, created_at, updated_at)
           VALUES ($1, 1, now(), now())
           ON CONFLICT (status_id) DO UPDATE
             SET reblogs_count = status_stats.reblogs_count + 1,
                 untrusted_reblogs_count = CASE
                   WHEN status_stats.untrusted_reblogs_count IS NULL
                     OR EXISTS (SELECT 1 FROM statuses WHERE id = $1 AND (COALESCE(local, false) OR uri IS NULL))
                   THEN status_stats.untrusted_reblogs_count
                   ELSE LEAST(GREATEST(status_stats.untrusted_reblogs_count + 1, 0), 100000000) END,
                 updated_at = now()"#,
        original_id
    )
    .execute(&state.db)
    .await?;

    // A boost is a status of the booster's, counted like any other.
    if let Err(e) = crate::counters::on_status_created(
        &state.db,
        auth.account_id,
        boost.visibility,
        None,
        boost.created_at,
    )
    .await
    {
        tracing::error!(error = %e, "failed to count a boost");
    }
    // `update_index('statuses', :proper)`: the boosted post, which the booster
    // may now search.
    crate::search::elasticsearch::indexing::status(&state, original_id).await;
    crate::search::elasticsearch::indexing::account(&state, auth.account_id).await;

    // `ReblogService`: `Trends.register!`.
    crate::trends::register(&state, boost.id).await;
    // `Status#update_statistics` for the boost, and `ReblogService#increment_statistics`.
    crate::activity_tracker::local_status_created(&state, boost.visibility).await;
    crate::activity_tracker::increment(&state, crate::activity_tracker::INTERACTIONS).await;
    crate::fasp::events::status_created(&state, boost.id).await;

    // Notify original author
    push::create_and_push(
        &state,
        original.account_id,
        auth.account_id,
        "reblog",
        Some(original_id),
    )
    .await;

    // Build viewer context against the ORIGINAL so the nested reblog object
    // carries correct favourited/bookmarked/reblogged flags for the iOS client.
    let ctx = build_viewer_context(&state, auth.account_id, original_id).await?;
    let media = fetch_status_media(&state, boost.id).await?;
    let reblog = fetch_reblog_data(&state, &boost).await?;
    let api_boost = build_status(&state, &boost, &boost_account, media, reblog, Some(ctx)).await?;

    // Fan the boost into followers' home feeds (mirrors the post path) so it
    // appears immediately, not only after a feed repopulate, then stream it
    // (`DistributionWorker`).
    crate::feed::distribute_later(&state, boost.id).await;

    // Send Announce activity to followers and original status author (if remote)
    if crate::federation::keypair::has_signing_key(&state, boost_account.id)
        .await
        .unwrap_or(false)
    {
        let actor_url =
            crate::federation::tag::account_uri_of(&state.instance.domain, &boost_account);
        // `ActivityPub::AnnounceNoteSerializer`, as the outbox serves it: the
        // id `TagManager#activity_uri_for` names, addressed by `#to` and
        // `#cc`, and the booster's own followers-only post inline.
        let announce = crate::portability::backup::announce_note(
            &state,
            &boost_account,
            boost.id,
            original_id,
            boost_visibility,
            boost.created_at,
            true,
        )
        .await?
        .map(crate::portability::backup::announce_document);
        let key_id = format!("{}#main-key", actor_url);

        // Reach the reblog audience (StatusReachFinder reblog branch): the
        // original author + the booster's followers + relays (public).
        use crate::db::models::vis;
        let inboxes = crate::federation::delivery::status_reach_inboxes(
            &state,
            boost.id,
            boost_account.id,
            None,
            matches!(boost_visibility, vis::PUBLIC | vis::UNLISTED),
            false,
            boost_visibility == vis::PUBLIC,
            matches!(boost_visibility, vis::PUBLIC | vis::UNLISTED | vis::PRIVATE),
            Some(original.account_id),
            &[],
        )
        .await
        .unwrap_or_default();
        if let Some(announce) = announce.filter(|_| !inboxes.is_empty()) {
            let signed = crate::federation::delivery::LinkedData::for_status(
                matches!(boost_visibility, vis::PUBLIC | vis::UNLISTED),
                crate::federation::delivery::LinkedData::UnlessAuthorizedFetch,
            );
            let synchronize = crate::federation::followers_synchronization::synchronizes(
                &state,
                boost_account.id,
                boost_visibility,
            )
            .await;
            if let Err(e) = crate::federation::delivery::deliver_status_to_inboxes(
                &state,
                announce,
                inboxes,
                key_id,
                signed,
                synchronize,
            )
            .await
            {
                tracing::warn!(error = %e, "failed to enqueue Announce delivery");
            }
        }
    }

    Ok(Json(api_boost))
    }
    .await;
    lock.release().await;
    result
}

// ── POST /api/v1/statuses/:id/unreblog ────────────────────────────────────

pub async fn unreblog_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Status>> {
    auth.require_scope("write:statuses")?;
    let (status_raw, _) = fetch_status_with_account(&state, id).await?;

    // Accept both the original status ID and the reblog's own ID.
    // When iOS sends the reblog wrapper's ID, resolve it to the original.
    let original_id = status_raw.reblog_of_id.unwrap_or(id);

    // `current_account.statuses.find_by(reblog_of_id:)`.
    let boost_id = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE account_id = $1 AND reblog_of_id = $2 AND deleted_at IS NULL",
        auth.account_id,
        original_id
    )
    .fetch_optional(&state.db)
    .await?;
    // `ReblogsController#destroy`: one's own boost goes whoever may see the
    // post now; without one, the post is shown only if `show?` allows it.
    if boost_id.is_none() {
        check_status_visible(&state, &status_raw, auth.account_id).await?;
    }

    let (original, _) = fetch_status_with_account(&state, original_id).await?;
    let mut rendered = serialize_status(&state, &original, Some(auth.account_id)).await?;
    if let Some(boost_id) = boost_id {
        // `count = [@reblog.reblogs_count - 1, 0].max`, then `@status.discard`
        // and `RemovalWorker`.
        let count = (rendered.reblogs_count - 1).max(0);
        crate::remove_status::discard(&state, boost_id).await?;
        crate::remove_status::call(&state, boost_id, crate::remove_status::Options::default())
            .await?;
        rendered = serialize_status(&state, &original, Some(auth.account_id)).await?;
        rendered.reblogs_count = count;
        rendered.reblogged = Some(false);
    }
    Ok(Json(rendered))
}

// ── POST /api/v1/statuses/:id/bookmark ────────────────────────────────────

pub async fn bookmark_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Status>> {
    auth.require_scope("write:bookmarks")?;
    let (s, _) = fetch_status_with_account(&state, id).await?;
    check_status_visible(&state, &s, auth.account_id).await?;

    sqlx::query!(
        "INSERT INTO bookmarks (account_id, status_id, created_at, updated_at) VALUES ($1, $2, now(), now()) ON CONFLICT DO NOTHING",
        auth.account_id, id
    )
    .execute(&state.db)
    .await?;
    // `Bookmark`'s `update_index('statuses', :status)`.
    crate::search::elasticsearch::indexing::status_interaction(&state, id).await;

    let (status, _) = fetch_status_with_account(&state, id).await?;
    Ok(Json(
        serialize_status(&state, &status, Some(auth.account_id)).await?,
    ))
}

// ── POST /api/v1/statuses/:id/unbookmark ──────────────────────────────────

pub async fn unbookmark_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Status>> {
    auth.require_scope("write:bookmarks")?;
    let (s, _) = fetch_status_with_account(&state, id).await?;
    check_status_visible(&state, &s, auth.account_id).await?;

    let unbookmarked = sqlx::query!(
        "DELETE FROM bookmarks WHERE account_id = $1 AND status_id = $2",
        auth.account_id,
        id
    )
    .execute(&state.db)
    .await?;
    if unbookmarked.rows_affected() > 0 {
        crate::statuses_cleanup::invalidate_cleanup_info(
            &state,
            auth.account_id,
            id,
            crate::statuses_cleanup::Undone::Unbookmark,
        )
        .await;
    }
    crate::search::elasticsearch::indexing::status_interaction(&state, id).await;

    let (status, _) = fetch_status_with_account(&state, id).await?;
    Ok(Json(
        serialize_status(&state, &status, Some(auth.account_id)).await?,
    ))
}

// ── POST /api/v1/statuses/:id/pin ─────────────────────────────────────────

/// Federate an `Add`/`Remove` of a status to/from the actor's featured (pinned)
/// collection, delivered to followers (Mastodon PinsController). No-op for
/// remote authors or accounts without a signing key.
async fn federate_pin_change(state: &AppState, account: &Account, status: &DbStatus, is_add: bool) {
    if account.domain.is_some()
        || !crate::federation::keypair::has_signing_key(state, account.id)
            .await
            .unwrap_or(false)
    {
        return;
    }
    let Some(status_uri) = status.uri.clone().filter(|s| !s.is_empty()) else {
        return;
    };
    let domain = &state.instance.domain;
    let actor_url = crate::federation::tag::account_uri_of(domain, account);
    let target = format!("{actor_url}/collections/featured");
    let activity = if is_add {
        crate::federation::activity::add_to_collection(&actor_url, &status_uri, &target)
    } else {
        crate::federation::activity::remove_from_collection(&actor_url, &status_uri, &target)
    };
    let key_id = format!("{actor_url}#main-key");
    if let Err(e) =
        crate::federation::delivery::fanout_to_followers(state, activity, account.id, key_id).await
    {
        tracing::warn!(error = %e, "failed to enqueue pin Add/Remove delivery");
    }
}

pub async fn pin_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Status>> {
    auth.require_scope("write:accounts")?;
    let (status, account) = fetch_status_with_account(&state, id).await?;
    if status.account_id != auth.account_id {
        return Err(AppError::Unprocessable(
            "Validation failed: You can only pin your own statuses".into(),
        ));
    }
    if status.reblog_of_id.is_some() {
        return Err(AppError::Unprocessable(
            "Validation failed: Reblogs cannot be pinned".into(),
        ));
    }
    let pin_count = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM status_pins WHERE account_id = $1",
        auth.account_id
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);
    if pin_count >= 5 {
        return Err(AppError::Unprocessable(
            "Validation failed: You have already pinned the maximum number of statuses".into(),
        ));
    }
    let inserted = sqlx::query!(
        "INSERT INTO status_pins (account_id, status_id, created_at, updated_at) VALUES ($1, $2, now(), now()) ON CONFLICT DO NOTHING",
        auth.account_id, id
    )
    .execute(&state.db)
    .await?;
    if inserted.rows_affected() > 0 {
        federate_pin_change(&state, &account, &status, true).await;
    }
    Ok(Json(
        serialize_status(&state, &status, Some(auth.account_id)).await?,
    ))
}

// ── POST /api/v1/statuses/:id/unpin ───────────────────────────────────────

pub async fn unpin_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Status>> {
    auth.require_scope("write:accounts")?;
    let (status, account) = fetch_status_with_account(&state, id).await?;
    // `Statuses::BaseController#set_status`: `authorize @status, :show?`.
    check_status_visible(&state, &status, auth.account_id).await?;
    let deleted = sqlx::query!(
        "DELETE FROM status_pins WHERE account_id = $1 AND status_id = $2",
        auth.account_id,
        id
    )
    .execute(&state.db)
    .await?;
    if deleted.rows_affected() > 0 {
        crate::statuses_cleanup::invalidate_cleanup_info(
            &state,
            auth.account_id,
            id,
            crate::statuses_cleanup::Undone::Unpin,
        )
        .await;
        federate_pin_change(&state, &account, &status, false).await;
    }
    Ok(Json(
        serialize_status(&state, &status, Some(auth.account_id)).await?,
    ))
}

// ── POST /api/v1/statuses/:id/mute ────────────────────────────────────────

pub async fn mute_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Status>> {
    auth.require_scope("write:mutes")?;
    let (status, _) = fetch_status_with_account(&state, id).await?;
    // `authorize @status, :show?`, then the status's conversation, which
    // every status is given (`Mastodon::ValidationError` without one).
    check_status_visible(&state, &status, auth.account_id).await?;
    let cid = status
        .conversation_id
        .ok_or_else(|| AppError::Unprocessable("Validation failed".into()))?;
    sqlx::query!(
        "INSERT INTO conversation_mutes (account_id, conversation_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        auth.account_id, cid
    )
    .execute(&state.db)
    .await?;
    Ok(Json(
        serialize_status(&state, &status, Some(auth.account_id)).await?,
    ))
}

// ── POST /api/v1/statuses/:id/unmute ──────────────────────────────────────

pub async fn unmute_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Status>> {
    auth.require_scope("write:mutes")?;
    let (status, _) = fetch_status_with_account(&state, id).await?;
    check_status_visible(&state, &status, auth.account_id).await?;
    let cid = status
        .conversation_id
        .ok_or_else(|| AppError::Unprocessable("Validation failed".into()))?;
    sqlx::query!(
        "DELETE FROM conversation_mutes WHERE account_id = $1 AND conversation_id = $2",
        auth.account_id,
        cid
    )
    .execute(&state.db)
    .await?;
    Ok(Json(
        serialize_status(&state, &status, Some(auth.account_id)).await?,
    ))
}

// ── GET /api/v1/statuses/:id/favourited_by ────────────────────────────────

pub async fn favourited_by(
    state: AppState,
    Path(id): Path<i64>,
    Query(pagination): Query<PaginationParams>,
    uri: Uri,
    req_headers: HeaderMap,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<impl IntoResponse> {
    let (status, _) = fetch_status_with_account(&state, id).await?;
    let viewer_id = auth.as_ref().map(|Extension(a)| a.account_id);
    if let Some(vid) = viewer_id {
        check_status_visible(&state, &status, vid).await?;
    } else if matches!(
        status.visibility,
        crate::db::models::vis::PRIVATE
            | crate::db::models::vis::DIRECT
            | crate::db::models::vis::LIMITED
    ) {
        return Err(AppError::NotFound);
    }

    let limit = pagination.limit_clamped(40, 80);
    let max_id = pagination
        .max_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let since_id = pagination
        .since_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let min_id = pagination
        .min_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());

    // Paginate by favourite.id (matching Mastodon's Favourite.paginate_by_max_id)
    let fav_rows = sqlx::query!(
        r#"SELECT f.id AS fav_id, f.account_id FROM favourites f
           JOIN accounts a ON a.id = f.account_id
           WHERE f.status_id = $1
             AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
             AND ($2::bigint IS NULL OR NOT EXISTS (
                 SELECT 1 FROM blocks
                 WHERE (account_id = $2 AND target_account_id = a.id)
                    OR (account_id = a.id AND target_account_id = $2)
             ))
             AND ($2::bigint IS NULL OR NOT EXISTS (
                 SELECT 1 FROM mutes WHERE account_id = $2 AND target_account_id = a.id
             ))
             AND ($3::bigint IS NULL OR f.id < $3)
             AND ($4::bigint IS NULL OR f.id > $4)
             AND ($5::bigint IS NULL OR f.id > $5)
           ORDER BY f.id DESC LIMIT $6"#,
        id,
        viewer_id,
        max_id,
        since_id,
        min_id,
        limit,
    )
    .fetch_all(&state.db)
    .await?;

    let first_fav_id = fav_rows.first().map(|r| r.fav_id.to_string());
    let last_fav_id = fav_rows.last().map(|r| r.fav_id.to_string());
    let account_ids: Vec<i64> = fav_rows.iter().map(|r| r.account_id).collect();
    let account_map: std::collections::HashMap<i64, Account> = if account_ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
            &account_ids
        )
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect()
    };
    let accounts: Vec<Account> = fav_rows
        .iter()
        .filter_map(|r| account_map.get(&r.account_id).cloned())
        .collect();

    let result = batch_accounts_to_api(&state, &accounts).await;
    let bounds = first_fav_id.zip(last_fav_id);
    let resp_headers = super::link_headers(
        &req_headers,
        &uri,
        bounds.as_ref().map(|(n, o)| (n.as_str(), o.as_str())),
    );
    Ok((resp_headers, Json(result)))
}

// ── GET /api/v1/statuses/:id/reblogged_by ─────────────────────────────────

pub async fn reblogged_by(
    state: AppState,
    Path(id): Path<i64>,
    Query(pagination): Query<PaginationParams>,
    uri: Uri,
    req_headers: HeaderMap,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<impl IntoResponse> {
    let (status, _) = fetch_status_with_account(&state, id).await?;
    let viewer_id = auth.as_ref().map(|Extension(a)| a.account_id);
    if let Some(vid) = viewer_id {
        check_status_visible(&state, &status, vid).await?;
    } else if matches!(
        status.visibility,
        crate::db::models::vis::PRIVATE
            | crate::db::models::vis::DIRECT
            | crate::db::models::vis::LIMITED
    ) {
        return Err(AppError::NotFound);
    }

    let limit = pagination.limit_clamped(40, 80);
    let max_id = pagination
        .max_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let since_id = pagination
        .since_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let min_id = pagination
        .min_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());

    // Paginate by reblog status.id (matching Mastodon's Status.paginate_by_max_id)
    let reblog_rows = sqlx::query!(
        r#"SELECT s.id AS reblog_id, s.account_id FROM statuses s
           JOIN accounts a ON a.id = s.account_id
           WHERE s.reblog_of_id = $1 AND s.deleted_at IS NULL
             AND s.visibility IN (0, 1)
             AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
             AND ($2::bigint IS NULL OR NOT EXISTS (
                 SELECT 1 FROM blocks
                 WHERE (account_id = $2 AND target_account_id = a.id)
                    OR (account_id = a.id AND target_account_id = $2)
             ))
             AND ($2::bigint IS NULL OR NOT EXISTS (
                 SELECT 1 FROM mutes WHERE account_id = $2 AND target_account_id = a.id
             ))
             AND ($3::bigint IS NULL OR s.id < $3)
             AND ($4::bigint IS NULL OR s.id > $4)
             AND ($5::bigint IS NULL OR s.id > $5)
           ORDER BY s.id DESC LIMIT $6"#,
        id,
        viewer_id,
        max_id,
        since_id,
        min_id,
        limit,
    )
    .fetch_all(&state.db)
    .await?;

    let first_reblog_id = reblog_rows.first().map(|r| r.reblog_id.to_string());
    let last_reblog_id = reblog_rows.last().map(|r| r.reblog_id.to_string());
    let account_ids: Vec<i64> = reblog_rows.iter().map(|r| r.account_id).collect();
    let account_map: std::collections::HashMap<i64, Account> = if account_ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
            &account_ids
        )
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect()
    };
    let accounts: Vec<Account> = reblog_rows
        .iter()
        .filter_map(|r| account_map.get(&r.account_id).cloned())
        .collect();

    let result = batch_accounts_to_api(&state, &accounts).await;
    let bounds = first_reblog_id.zip(last_reblog_id);
    let resp_headers = super::link_headers(
        &req_headers,
        &uri,
        bounds.as_ref().map(|(n, o)| (n.as_str(), o.as_str())),
    );
    Ok((resp_headers, Json(result)))
}

// ── PATCH /api/v1/statuses/:id/interaction_policy ─────────────────────────

#[derive(Debug, serde::Deserialize, Default)]
pub struct InteractionPolicyForm {
    pub quote_approval_policy: Option<String>,
}

/// `Api::V1::Statuses::InteractionPoliciesController#update`.
pub async fn update_interaction_policy(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    super::extractors::Params(form): super::extractors::Params<InteractionPolicyForm>,
) -> AppResult<Json<Status>> {
    auth.require_scope("write:statuses")?;
    let (status, account) = fetch_status_with_account(&state, id).await?;
    // `set_status`: `authorize @status, :show?` hides what the caller may not
    // see; then `authorize @status, :update?`.
    check_status_visible(&state, &status, auth.account_id).await?;
    if status.account_id != auth.account_id {
        return Err(AppError::Forbidden);
    }
    // `quote_approval_policy`, else the user's default.
    let requested = match form.quote_approval_policy.filter(|p| !p.is_empty()) {
        Some(p) => p,
        None => {
            crate::api::mastodon::accounts::user_defaults(&state, auth.account_id)
                .await
                .quote_policy
        }
    };
    // `raise ActiveRecord::RecordInvalid`, with no record to name.
    let mut policy = crate::db::models::quote_policy::from_api(&requested)
        .ok_or_else(|| AppError::Unprocessable("Record invalid".into()))?;
    // `downgrade_quote_policy`: a local post no one else may see allows no
    // quotes.
    if !matches!(
        status.visibility,
        crate::db::models::vis::PUBLIC | crate::db::models::vis::UNLISTED
    ) {
        policy = 0;
    }
    let changed = policy != status.quote_approval_policy;
    if changed {
        sqlx::query!(
            "UPDATE statuses SET quote_approval_policy = $1, updated_at = now() WHERE id = $2",
            policy,
            id,
        )
        .execute(&state.db)
        .await?;
        // `@status.update!`: its `after_update_commit`s.
        crate::moderation::webhooks::status_updated(&state, id).await;
        crate::fasp::events::status_updated(&state, id).await;
    }
    let status = sqlx::query_as!(DbStatus, "SELECT * FROM statuses WHERE id = $1", id)
        .fetch_one(&state.db)
        .await?;
    if changed {
        // `broadcast_updates!`: local timelines, without notifying anyone, and
        // the servers that have it.
        crate::quotes::distribute_update(&state, id, true).await;
        if let Err(error) = federate_status_update(&state, id, &account, &status).await {
            tracing::warn!(
                status_id = id,
                ?error,
                "could not federate a quote policy change"
            );
        }
    }
    Ok(Json(
        serialize_status(&state, &status, Some(auth.account_id)).await?,
    ))
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// `StatusPolicy#show?` for a local viewer, as a 404 when it fails: the
/// author available; a direct or limited status only for its author and
/// those it mentions; a private one for them and the author's followers; any
/// other unless its author blocks the viewer. (A local viewer has no domain
/// for `author_blocking_domain?` to block.)
pub(crate) async fn check_status_visible(
    state: &AppState,
    status: &DbStatus,
    viewer_id: i64,
) -> AppResult<()> {
    use crate::db::models::vis;

    // `return false if author.unavailable?`.
    let author_available = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM accounts
             WHERE id = $1 AND suspended_at IS NULL AND requested_deletion_at IS NULL
           ) AS "e!""#,
        status.account_id,
    )
    .fetch_one(&state.db)
    .await?;
    if !author_available {
        return Err(AppError::NotFound);
    }
    let owned = status.account_id == viewer_id;
    let mention_exists = || async {
        sqlx::query_scalar!(
            r#"SELECT EXISTS (
                 SELECT 1 FROM mentions WHERE status_id = $1 AND account_id = $2
               ) AS "e!""#,
            status.id,
            viewer_id,
        )
        .fetch_one(&state.db)
        .await
    };
    let shown = match status.visibility {
        // `requires_mention?`.
        vis::DIRECT | vis::LIMITED => owned || mention_exists().await?,
        vis::PRIVATE => {
            owned
                || sqlx::query_scalar!(
                    r#"SELECT EXISTS (
                         SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2
                       ) AS "e!""#,
                    viewer_id,
                    status.account_id,
                )
                .fetch_one(&state.db)
                .await?
                || mention_exists().await?
        }
        // `!author_blocking?`.
        _ => {
            !sqlx::query_scalar!(
                r#"SELECT EXISTS (
                 SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = $2
               ) AS "e!""#,
                status.account_id,
                viewer_id,
            )
            .fetch_one(&state.db)
            .await?
        }
    };
    if shown {
        Ok(())
    } else {
        Err(AppError::NotFound)
    }
}

/// `StatusPolicy#show?` with no one signed in: the author available and the
/// status public or unlisted.
pub(crate) async fn check_status_public(state: &AppState, status: &DbStatus) -> AppResult<()> {
    use crate::db::models::vis;
    if !matches!(status.visibility, vis::PUBLIC | vis::UNLISTED) {
        return Err(AppError::NotFound);
    }
    let author_available = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM accounts
             WHERE id = $1 AND suspended_at IS NULL AND requested_deletion_at IS NULL
           ) AS "e!""#,
        status.account_id,
    )
    .fetch_one(&state.db)
    .await?;
    if author_available {
        Ok(())
    } else {
        Err(AppError::NotFound)
    }
}

async fn fetch_status_with_account(state: &AppState, id: i64) -> AppResult<(DbStatus, Account)> {
    let status = sqlx::query_as!(
        DbStatus,
        "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    // `StatusPolicy#show?` starts with `return false if author.unavailable?`:
    // a suspended author's statuses are invisible (404) for as long as the
    // suspension lasts, rather than being deleted when it is applied.
    let account = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = $1 AND suspended_at IS NULL AND requested_deletion_at IS NULL",
        status.account_id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    Ok((status, account))
}

async fn fetch_account(state: &AppState, id: i64) -> AppResult<Account> {
    sqlx::query_as!(Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

pub(crate) async fn federate_status_update(
    state: &AppState,
    status_id: i64,
    account: &Account,
    status: &DbStatus,
) -> anyhow::Result<()> {
    if account.domain.is_some() || status.reblog_of_id.is_some() {
        return Ok(());
    }
    if !matches!(
        status.visibility,
        crate::db::models::vis::PUBLIC
            | crate::db::models::vis::UNLISTED
            | crate::db::models::vis::PRIVATE
            | crate::db::models::vis::DIRECT
    ) {
        return Ok(());
    }

    if !crate::federation::keypair::has_signing_key(state, account.id)
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }

    let bundle =
        match crate::api::ap::note::build_note(state, &state.instance.domain, status_id).await? {
            Some(bundle) => bundle,
            None => return Ok(()),
        };

    let updated_at = status
        .edited_at
        .unwrap_or_else(|| chrono::Utc::now().naive_utc())
        .and_utc();
    let update_id = format!("{}#updates/{}", bundle.note_uri, updated_at.timestamp());
    let activity = serde_json::json!({
        "@context": crate::api::ap::note::note_context(),
        "id": update_id,
        "type": "Update",
        "actor": bundle.actor_url,
        // `edited_at.iso8601`: whole seconds, with a `Z`.
        "published": crate::api::ap::note::iso8601(updated_at),
        "to": bundle.to,
        "cc": bundle.cc,
        "object": bundle.note,
    });
    let key_id = crate::federation::tag::key_id_of(&state.instance.domain, account);

    // Reach the same audience that received the original (StatusReachFinder).
    use crate::db::models::vis;
    let inboxes = crate::federation::delivery::status_reach_inboxes(
        state,
        status_id,
        account.id,
        status.in_reply_to_account_id,
        matches!(status.visibility, vis::PUBLIC | vis::UNLISTED),
        false,
        status.visibility == vis::PUBLIC,
        matches!(
            status.visibility,
            vis::PUBLIC | vis::UNLISTED | vis::PRIVATE
        ),
        None,
        &[],
    )
    .await?;
    if !inboxes.is_empty() {
        let signed = crate::federation::delivery::LinkedData::for_status(
            matches!(status.visibility, vis::PUBLIC | vis::UNLISTED),
            crate::federation::delivery::LinkedData::UnlessAuthorizedFetch,
        );
        // `ActivityPub::StatusUpdateDistributionWorker`, a
        // `DistributionWorker`.
        let synchronize = crate::federation::followers_synchronization::synchronizes(
            state,
            account.id,
            status.visibility,
        )
        .await;
        crate::federation::delivery::deliver_status_to_inboxes(
            state,
            activity,
            inboxes,
            key_id,
            signed,
            synchronize,
        )
        .await?;
    }

    Ok(())
}

/// Batch-fetch viewer context for a list of status IDs in 5 queries.
/// Returns a map from status_id → StatusViewerContext.
pub(super) async fn batch_viewer_contexts(
    state: &AppState,
    viewer_id: i64,
    status_ids: &[i64],
) -> AppResult<std::collections::HashMap<i64, super::convert::StatusViewerContext>> {
    use super::convert::StatusViewerContext;
    use std::collections::{HashMap, HashSet};

    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let fav_set: HashSet<i64> = sqlx::query_scalar!(
        "SELECT status_id FROM favourites WHERE account_id = $1 AND status_id = ANY($2::bigint[])",
        viewer_id,
        status_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();

    let reb_set: HashSet<i64> = sqlx::query_scalar!(
        r#"SELECT reblog_of_id as "reblog_of_id!: i64" FROM statuses
           WHERE account_id = $1 AND reblog_of_id = ANY($2::bigint[]) AND deleted_at IS NULL"#,
        viewer_id,
        status_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();

    let book_set: HashSet<i64> = sqlx::query_scalar!(
        "SELECT status_id FROM bookmarks WHERE account_id = $1 AND status_id = ANY($2::bigint[])",
        viewer_id,
        status_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();

    let mute_set: HashSet<i64> = sqlx::query_scalar!(
        "SELECT s.id FROM statuses s JOIN conversation_mutes cm ON cm.conversation_id = s.conversation_id WHERE cm.account_id = $1 AND s.id = ANY($2::bigint[])",
        viewer_id, status_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();

    let pin_set: HashSet<i64> = sqlx::query_scalar!(
        "SELECT status_id FROM status_pins WHERE account_id = $1 AND status_id = ANY($2::bigint[])",
        viewer_id,
        status_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();

    // The follow flags feed `quote_policy_for_account`, which a boost answers
    // for the post it boosts (`object.proper`), so a boost maps to that
    // post's author.
    let status_author_rows = sqlx::query!(
        r#"SELECT s.id AS status_id, COALESCE(o.account_id, s.account_id) AS "account_id!"
           FROM statuses s LEFT JOIN statuses o ON o.id = s.reblog_of_id
           WHERE s.id = ANY($1::bigint[])"#,
        status_ids,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let status_to_author: HashMap<i64, i64> = status_author_rows
        .into_iter()
        .map(|r| (r.status_id, r.account_id))
        .collect();

    let author_ids: Vec<i64> = status_to_author
        .values()
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let viewer_follows_set: HashSet<i64> = if !author_ids.is_empty() {
        sqlx::query_scalar!(
            "SELECT target_account_id FROM follows WHERE account_id = $1 AND target_account_id = ANY($2::bigint[])",
            viewer_id, &author_ids,
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect()
    } else {
        HashSet::new()
    };

    let author_follows_set: HashSet<i64> = if !author_ids.is_empty() {
        sqlx::query_scalar!(
            "SELECT account_id FROM follows WHERE account_id = ANY($1::bigint[]) AND target_account_id = $2",
            &author_ids, viewer_id,
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect()
    } else {
        HashSet::new()
    };

    let mut result = HashMap::with_capacity(status_ids.len());
    for &id in status_ids {
        let author_id = status_to_author.get(&id).cloned().unwrap_or(0);
        result.insert(
            id,
            StatusViewerContext {
                account_id: viewer_id,
                follows_author: viewer_follows_set.contains(&author_id),
                author_follows: author_follows_set.contains(&author_id),
                favourited: fav_set.contains(&id),
                reblogged: reb_set.contains(&id),
                bookmarked: book_set.contains(&id),
                muted: mute_set.contains(&id),
                pinned: pin_set.contains(&id),
            },
        );
    }
    Ok(result)
}

/// Fully serialize a single status for an optional viewer.
///
/// Performs the account / media / reblog / viewer-context fetches that every
/// single-status endpoint needs, then delegates to [`build_status`]. Pass
/// `viewer_id` as `None` for unauthenticated serialization (omits per-viewer
/// flags such as `favourited`). This is the single entry point single-status
/// endpoints should use instead of re-inlining the fetch quintet.
pub async fn serialize_status(
    state: &AppState,
    status: &DbStatus,
    viewer_id: Option<i64>,
) -> AppResult<super::types::Status> {
    let account = fetch_account(state, status.account_id).await?;
    let media = fetch_status_media(state, status.id).await?;
    let reblog = fetch_reblog_data(state, status).await?;
    let viewer_ctx = match viewer_id {
        Some(vid) => Some(build_viewer_context(state, vid, status.id).await?),
        None => None,
    };
    build_status(state, status, &account, media, reblog, viewer_ctx).await
}

pub async fn build_viewer_context(
    state: &AppState,
    viewer_id: i64,
    status_id: i64,
) -> AppResult<super::convert::StatusViewerContext> {
    // Delegate to the batched implementation so the per-viewer flag logic
    // (favourited / reblogged / bookmarked / muted / pinned + follow
    // relationships) lives in exactly one place. `batch_viewer_contexts`
    // inserts an entry for every requested id, so the fallback is a safety
    // net that only fires if the id list is somehow dropped.
    Ok(batch_viewer_contexts(state, viewer_id, &[status_id])
        .await?
        .remove(&status_id)
        .unwrap_or(super::convert::StatusViewerContext {
            account_id: viewer_id,
            follows_author: false,
            author_follows: false,
            favourited: false,
            reblogged: false,
            muted: false,
            bookmarked: false,
            pinned: false,
        }))
}

/// `Extractor.extract_hashtags`, each name once, lowercased.
pub fn extract_hashtags(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    crate::formatter::extractor::extract_hashtags(text)
        .into_iter()
        .filter_map(|e| match e.kind {
            // As written, one per normalized name (`find_or_create_by_names`'s
            // `uniq(&:first)`), so the first spelling becomes the tag's
            // `display_name` when it is new.
            crate::formatter::extractor::Kind::Hashtag(tag) => seen
                .insert(crate::search::tags::normalize(&tag))
                .then_some(tag),
            _ => None,
        })
        .collect()
}

/// `text.scan(Account::MENTION_RE)`, as `ProcessMentionsService` reads it:
/// each username and domain once, lowercased.
pub fn extract_mention_handles(text: &str) -> Vec<(String, Option<String>)> {
    let mut seen = std::collections::HashSet::new();
    crate::formatter::extractor::mention_handles(text)
        .into_iter()
        .filter_map(|(username, domain)| {
            let username = username.to_lowercase();
            let domain = domain.map(|d| d.to_lowercase());
            seen.insert((username.clone(), domain.clone()))
                .then_some((username, domain))
        })
        .collect()
}

pub async fn resolve_mention_accounts(
    state: &AppState,
    handles: &[(String, Option<String>)],
    local_domain: &str,
) -> Vec<(String, Account)> {
    let mut result = Vec::new();
    for (username, domain) in handles {
        // A mention that names this instance's own domain refers to a local
        // account, which is stored with domain IS NULL (mirrors Mastodon's
        // TagManager#local_domain? normalization).
        let domain = domain
            .as_deref()
            .filter(|d| local_domain.is_empty() || !d.eq_ignore_ascii_case(local_domain));

        let account = if let Some(d) = domain {
            sqlx::query_as!(
                Account,
                "SELECT * FROM accounts WHERE LOWER(username) = $1 AND domain = $2 LIMIT 1",
                username,
                d,
            )
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
        } else {
            sqlx::query_as!(
                Account,
                "SELECT * FROM accounts WHERE LOWER(username) = $1 AND domain IS NULL LIMIT 1",
                username,
            )
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
        };

        // Unknown remote account, or one not speaking ActivityPub
        // (`mention_undeliverable?`): resolve it via WebFinger and fetch the
        // actor, mirroring Mastodon's ProcessMentionsService, so that
        // mentioning a user this instance has never seen still creates the
        // mention and federates. One still undeliverable is no mention.
        let undeliverable = |a: &Account| a.domain.is_some() && !a.is_activitypub();
        let account = match account {
            Some(acct) if !undeliverable(&acct) => Some(acct),
            _ => match domain {
                Some(d) => match crate::federation::webfinger::resolve_allowed(state, username, d)
                    .await
                {
                    Ok(actor_url) => match crate::api::ap::inbox::resolve_or_fetch_remote_account(
                        state, &actor_url,
                    )
                    .await
                    {
                        Ok(id) => {
                            sqlx::query_as!(Account, "SELECT * FROM accounts WHERE id = $1", id)
                                .fetch_optional(&state.db)
                                .await
                                .ok()
                                .flatten()
                        }
                        Err(e) => {
                            tracing::debug!(handle = %format!("{username}@{d}"), error = %e, "mention actor fetch failed");
                            None
                        }
                    },
                    Err(e) => {
                        tracing::debug!(handle = %format!("{username}@{d}"), error = %e, "mention webfinger failed");
                        None
                    }
                },
                None => None,
            },
        };

        if let Some(acct) = account.filter(|a| !undeliverable(a)) {
            result.push((username.clone(), acct));
        }
    }
    result
}

pub async fn store_statuses_tags(
    state: &AppState,
    status_id: i64,
    account_id: i64,
    hashtags: &[String],
) -> AppResult<()> {
    let previous: Vec<i64> = sqlx::query_scalar!(
        "DELETE FROM statuses_tags WHERE status_id = $1 RETURNING tag_id",
        status_id
    )
    .fetch_all(&state.db)
    .await?;
    let mut current: Vec<i64> = vec![];
    for tag_name in hashtags {
        let Some(tag_id) = crate::tags::find_or_create(&state.db, tag_name).await? else {
            continue;
        };
        if !current.contains(&tag_id) {
            current.push(tag_id);
        }
        sqlx::query!(
            "INSERT INTO statuses_tags (status_id, tag_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            status_id,
            tag_id,
        )
        .execute(&state.db)
        .await?;
        // `Tag`'s `update_index('tags', :self)`.
        crate::search::elasticsearch::indexing::tags(state, &[tag_id]).await;
    }
    // `ProcessHashtagsService#update_featured_tags!`.
    if let Some(row) = sqlx::query!(
        "SELECT visibility, created_at FROM statuses WHERE id = $1",
        status_id
    )
    .fetch_optional(&state.db)
    .await?
    {
        crate::featured_tags::update_for_status(
            &state.db,
            account_id,
            status_id,
            row.visibility,
            row.created_at,
            &previous,
            &current,
        )
        .await?;
    }
    Ok(())
}

pub async fn store_status_mentions(
    state: &AppState,
    status_id: i64,
    resolved: &[(String, Account)],
) -> AppResult<()> {
    // `assign_mentions!`: never mention an account the author blocks, or one
    // on a domain the author blocks; such a mention, even one made before,
    // is destroyed.
    let ids: Vec<i64> = resolved.iter().map(|(_, account)| account.id).collect();
    let blocked: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT a.id FROM accounts a, statuses s
           WHERE s.id = $1 AND a.id = ANY($2)
             AND (EXISTS (SELECT 1 FROM blocks b
                          WHERE b.account_id = s.account_id AND b.target_account_id = a.id)
                  OR EXISTS (SELECT 1 FROM account_domain_blocks d
                             WHERE d.account_id = s.account_id AND d.domain = a.domain))"#,
        status_id,
        &ids,
    )
    .fetch_all(&state.db)
    .await?;
    if !blocked.is_empty() {
        sqlx::query!(
            "DELETE FROM mentions WHERE status_id = $1 AND account_id = ANY($2)",
            status_id,
            &blocked,
        )
        .execute(&state.db)
        .await?;
    }
    let current: Vec<i64> = ids.into_iter().filter(|id| !blocked.contains(id)).collect();
    for account_id in &current {
        sqlx::query!(
            r#"INSERT INTO mentions (status_id, account_id, created_at, updated_at) VALUES ($1, $2, now(), now())
               ON CONFLICT (account_id, status_id) DO UPDATE SET silent = false, updated_at = now()
               WHERE mentions.silent"#,
            status_id, account_id,
        )
        .execute(&state.db)
        .await?;
    }
    // A mention the text no longer makes is kept, silently: withdrawing
    // access from someone who was already notified would confuse more than
    // it helps, so whoever an edit stops mentioning can still see a private
    // or direct post. (One already silent, which `Quote#ensure_quoted_access`
    // made, stays as it is.)
    sqlx::query!(
        r#"UPDATE mentions SET silent = true
           WHERE status_id = $1 AND NOT silent AND NOT (account_id = ANY($2))"#,
        status_id,
        &current,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod inline_quote_tests {
    use super::inline_quote_instrument;
    use serde_json::json;

    #[test]
    fn inlines_note_and_merges_context() {
        // A QuoteRequest as ojak-vocab emits it: compound @context declaring the
        // FEP-044f `QuoteRequest` term, with `instrument` as a bare URI.
        let mut request = json!({
            "@context": [
                "https://www.w3.org/ns/activitystreams",
                { "QuoteRequest": "https://w3id.org/fep/044f#QuoteRequest" }
            ],
            "type": "QuoteRequest",
            "id": "https://seoul.earth/users/sohu/quote_requests/1",
            "actor": "https://seoul.earth/users/sohu",
            "object": "https://hackers.pub/ap/notes/abc",
            "instrument": "https://seoul.earth/users/sohu/statuses/1",
        });
        // The context-less Note we inline (as `NoteBundle::note` is built).
        let note = json!({
            "id": "https://seoul.earth/users/sohu/statuses/1",
            "type": "Note",
            "attributedTo": "https://seoul.earth/users/sohu",
            "quote": "https://hackers.pub/ap/notes/abc",
            "quoteUri": "https://hackers.pub/ap/notes/abc",
        });

        inline_quote_instrument(&mut request, note.clone());

        // The instrument is now the embedded Note object, not a URI.
        assert_eq!(request["instrument"], note);
        assert_eq!(
            request["instrument"]["quote"],
            "https://hackers.pub/ap/notes/abc"
        );

        // The request context keeps the QuoteRequest term and gains the Note's
        // JSON-LD terms so the embedded fields resolve.
        let terms = &request["@context"][1];
        assert_eq!(
            terms["QuoteRequest"],
            "https://w3id.org/fep/044f#QuoteRequest"
        );
        assert_eq!(
            terms["quote"],
            json!({ "@id": "fep:quote", "@type": "@id" })
        );
        assert_eq!(terms["Hashtag"], "as:Hashtag");
        assert_eq!(terms["sensitive"], "as:sensitive");
    }
}
