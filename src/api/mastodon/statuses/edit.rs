//! Editing statuses: `PUT /statuses/:id`, plus the edit-history and source
//! endpoints (`/history`, `/source`).

use super::*;

// ── PUT /api/v1/statuses/:id ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct EditMediaAttribute {
    #[serde(default, deserialize_with = "rails::string")]
    pub id: String,
    #[serde(default, deserialize_with = "rails::opt_string")]
    pub description: Option<String>,
    /// `"x,y"`, or `[x, y]`.
    pub focus: Option<serde_json::Value>,
}

/// `MediaAttachment::MAX_DESCRIPTION_LENGTH`.
const MAX_DESCRIPTION_LENGTH: usize = 10_000;

/// What an edit's `media_attributes` entry changes of one of the next
/// attachments.
struct MediaUpdate {
    id: i64,
    description: Option<String>,
    focus: Option<serde_json::Value>,
}

/// `MediaAttachment#focus=`: `x,y` (or a pair), each `to_f`, as the
/// `{x, y}` kept in `file_meta`; nothing for a blank one.
fn focus_point(value: &serde_json::Value) -> Option<serde_json::Value> {
    // Ruby's `String#to_f`: the longest leading number, else 0.
    fn to_f(s: &str) -> f64 {
        let s = s.trim_start();
        let mut end = 0;
        let bytes = s.as_bytes();
        if matches!(bytes.first(), Some(b'+' | b'-')) {
            end = 1;
        }
        let mut seen_dot = false;
        let mut seen_digit = false;
        while let Some(&b) = bytes.get(end) {
            match b {
                b'0'..=b'9' => seen_digit = true,
                b'.' if !seen_dot => seen_dot = true,
                _ => break,
            }
            end += 1;
        }
        if !seen_digit {
            return 0.0;
        }
        s[..end].trim_end_matches('.').parse().unwrap_or(0.0)
    }
    let number = |v: &serde_json::Value| match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => Some(to_f(s)),
        _ => None,
    };
    let parts: Vec<Option<f64>> = match value {
        serde_json::Value::String(s) if !blank(s) => s.split(',').map(|p| Some(to_f(p))).collect(),
        serde_json::Value::Array(items) if !items.is_empty() => items.iter().map(number).collect(),
        _ => return None,
    };
    let at = |i: usize| parts.get(i).copied().flatten();
    Some(serde_json::json!({ "x": at(0), "y": at(1) }))
}

#[derive(Debug, Deserialize)]
pub struct EditStatusForm {
    #[serde(default, deserialize_with = "rails::opt_string")]
    pub status: Option<String>,
    #[serde(default, deserialize_with = "rails::opt_string")]
    pub spoiler_text: Option<String>,
    #[serde(default, deserialize_with = "rails::opt_bool")]
    pub sensitive: Option<bool>,
    #[serde(default, deserialize_with = "rails::opt_string")]
    pub language: Option<String>,
    #[serde(default, deserialize_with = "rails::opt_present_strings")]
    pub media_ids: Option<Vec<String>>,
    #[serde(default, deserialize_with = "rails::nested_attributes")]
    pub media_attributes: Option<Vec<EditMediaAttribute>>,
    /// `poll`, absent and `null` alike meaning the post has no poll now.
    #[serde(default, deserialize_with = "double_option")]
    pub poll: Option<Option<PollForm>>,
    /// `update_options[:quote_approval_policy] = quote_approval_policy if
    /// status_params[:quote_approval_policy].present?`
    #[serde(default, deserialize_with = "rails::opt_string")]
    pub quote_approval_policy: Option<String>,
}

fn double_option<'de, D>(de: D) -> Result<Option<Option<PollForm>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    super::poll_form(de).map(Some)
}

pub async fn edit_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    super::super::extractors::Params(form): super::super::extractors::Params<EditStatusForm>,
) -> AppResult<Json<Status>> {
    auth.require_scope("write:statuses")?;
    let (status, account) = fetch_status_with_account(&state, id).await?;
    // Mastodon scopes to `current_account.statuses.find` → 404 for another
    // user's status.
    if status.account_id != auth.account_id {
        return Err(AppError::NotFound);
    }
    if status.reblog_of_id.is_some() {
        return Err(AppError::Unprocessable("Reblogs cannot be edited".into()));
    }

    let instance_domain = state.instance.domain.clone();

    // The controller always gives `UpdateStatusService` the text, content
    // warning, sensitivity, language, attachments and poll, each `nil` when
    // the request leaves it out: an edit says what the post now is, whole.

    // `update_immediate_attributes!`: `@options[:text].presence || ''`, and,
    // for a post that quotes nothing, a blank text becomes the content
    // warning given (`@options.delete(:spoiler_text)`), which then no longer
    // changes the content warning or marks the post sensitive.
    let quotes = crate::quotes::find_by_status(&state.db, id)
        .await?
        .is_some();
    let mut new_text = form
        .status
        .clone()
        .filter(|text| !blank(text))
        .unwrap_or_default();
    let mut spoiler_given = form.spoiler_text.clone();
    let mut spoiler_key = true;
    if blank(&new_text) && !quotes {
        new_text = spoiler_given.take().unwrap_or_default();
        spoiler_key = false;
    }
    let new_spoiler = if spoiler_key {
        spoiler_given.clone().unwrap_or_default()
    } else {
        status.spoiler_text.clone()
    };
    // `@options[:sensitive] || @options[:spoiler_text].present?`.
    let new_sensitive =
        form.sensitive.unwrap_or(false) || spoiler_given.as_deref().is_some_and(|s| !blank(s));
    // `valid_locale_cascade(options[:language], status.language, user's
    // preferred posting language, I18n.default_locale)`.
    let preferred = crate::api::mastodon::accounts::user_defaults(&state, status.account_id)
        .await
        .language;
    let new_language = crate::languages::valid_locale_cascade(&[
        form.language.as_deref(),
        status.language.as_deref(),
        preferred.as_deref(),
        Some(state.instance.default_locale()),
    ]);

    // `update_media_attachments!`: the next attachments, validated, in the
    // order asked for (none when none are given). The attachments changed
    // when that order is not the one the status had, or when a
    // `media_attributes` entry changes one of them (`significantly_changed?`:
    // its description, thumbnail or focus).
    let previous_media: Vec<i64> =
        crate::api::mastodon::status_serialize::fetch_status_media(&state, id)
            .await?
            .iter()
            .map(|m| m.id)
            .collect();
    let next_media: Vec<i64> =
        validate_media(&state, auth.account_id, form.media_ids.as_deref(), Some(id)).await?;
    let current_attributes = sqlx::query!(
        "SELECT id, description, file_meta FROM media_attachments WHERE id = ANY($1)",
        &next_media,
    )
    .fetch_all(&state.db)
    .await?;
    let mut media_updates: Vec<MediaUpdate> = Vec::new();
    let mut media_changed = previous_media != next_media;
    for attr in form.media_attributes.iter().flatten() {
        let Some(current) = attr
            .id
            .trim()
            .parse::<i64>()
            .ok()
            .and_then(|media_id| current_attributes.iter().find(|m| m.id == media_id))
        else {
            continue;
        };
        if let Some(description) = &attr.description {
            // `validates :description, length: { maximum: MAX_DESCRIPTION_LENGTH }`.
            if description.chars().count() > MAX_DESCRIPTION_LENGTH {
                return Err(AppError::Unprocessable(format!(
                    "Validation failed: Description is too long (maximum is {MAX_DESCRIPTION_LENGTH} characters)"
                )));
            }
        }
        let focus = attr.focus.as_ref().and_then(focus_point);
        let current_focus = current
            .file_meta
            .as_ref()
            .and_then(|meta| meta.get("focus"))
            .cloned();
        let description_changed = attr
            .description
            .as_ref()
            .is_some_and(|d| current.description.as_deref() != Some(d.as_str()));
        let focus_changed = focus
            .as_ref()
            .is_some_and(|f| current_focus.as_ref() != Some(f));
        media_changed |= description_changed || focus_changed;
        media_updates.push(MediaUpdate {
            id: current.id,
            description: attr.description.clone(),
            focus,
        });
    }

    // `update_poll!`: a poll given is validated and saved, its votes reset
    // when its options or multiplicity changed; none given takes away the
    // poll the post had. Either changes the poll (`@poll_changed`), since a
    // poll given again ends `expires_in` from now.
    let given_poll = form.poll.as_ref().and_then(Option::as_ref);
    let prepared_poll = match given_poll {
        Some(pf) => Some(validate_poll_form(pf)?),
        None => None,
    };
    let existing_poll = sqlx::query!(
        "SELECT id, options, multiple, expires_at FROM polls WHERE id = $1",
        status.poll_id,
    )
    .fetch_optional(&state.db)
    .await?;
    let poll_changed = given_poll.is_some() || existing_poll.is_some();

    // `@status.quote_approval_policy = @options[:quote_approval_policy] if
    // @options[:quote_approval_policy].present?`, then `downgrade_quote_policy`
    // for a post only some may see.
    let new_quote_policy = match form
        .quote_approval_policy
        .as_deref()
        .filter(|p| !p.is_empty())
    {
        Some(requested) => crate::db::models::quote_policy::from_api(requested)
            .ok_or_else(|| AppError::Unprocessable("Record invalid".into()))?,
        None => status.quote_approval_policy,
    };
    let new_quote_policy = if matches!(
        status.visibility,
        crate::db::models::vis::PUBLIC | crate::db::models::vis::UNLISTED
    ) {
        new_quote_policy
    } else {
        0
    };

    // `raise NoChangesSubmittedError unless significant_changes?`: an edit
    // that changes nothing is no edit, and returns the post as it was.
    let significant = new_text != status.text
        || new_spoiler != status.spoiler_text
        || new_sensitive != status.sensitive
        || new_language != status.language
        || new_quote_policy != status.quote_approval_policy
        || status.ordered_media_attachment_ids.as_ref() != Some(&next_media)
        || media_changed
        || poll_changed;

    if !significant {
        return Ok(Json(
            serialize_status(&state, &status, Some(auth.account_id)).await?,
        ));
    }

    // `@status.save!`: `Status`'s validations, text required unless the post
    // now has media or quotes.
    let errors = status_errors(
        &state,
        &new_text,
        &new_spoiler,
        !next_media.is_empty() || quotes,
        None,
    )
    .await?;
    if !errors.is_empty() {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: {}",
            errors.join(", ")
        )));
    }

    // `rate_limit by: :account, family: :statuses` on the edit
    // `create_edit!` saves.
    crate::rate_limit::record(&state, auth.account_id, crate::rate_limit::STATUSES).await?;
    let hashtags = extract_hashtags(&new_text);
    let mention_handles = extract_mention_handles(&new_text);
    let resolved = resolve_mention_accounts(&state, &mention_handles, &instance_domain).await;

    // `Status.transaction do create_previous_edit! … create_edit! end`: the
    // original goes into the history first if the post has none, then the
    // edit changes the post, then the version it made goes in too.
    let mut tx = state.db.begin().await?;
    crate::status_snapshot::create_previous_edit(&mut tx, id).await?;

    // `update_media_attachments!`: `media.update!(attributes.slice(:thumbnail,
    // :description, :focus))` for the next attachments, the added ones
    // attached, and the order recorded. An attachment taken off stays
    // attached, so that the versions in the history that showed it still
    // can.
    for update in &media_updates {
        sqlx::query!(
            r#"UPDATE media_attachments
               SET description = COALESCE($2, description),
                   file_meta = CASE WHEN $3::jsonb IS NULL THEN file_meta
                       ELSE jsonb_set(COALESCE(file_meta::jsonb, '{}'::jsonb), '{focus}', $3::jsonb)::json END,
                   updated_at = now()
               WHERE id = $1"#,
            update.id,
            update.description,
            update.focus,
        )
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query!(
        "UPDATE media_attachments SET status_id = $1 WHERE id = ANY($2) AND status_id IS NULL",
        id,
        &next_media,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE statuses SET ordered_media_attachment_ids = $2 WHERE id = $1",
        id,
        &next_media,
    )
    .execute(&mut *tx)
    .await?;

    // `update_poll!`.
    match (given_poll, prepared_poll) {
        (Some(pf), Some(opts)) => {
            let expires_at = pf
                .expires_in
                .map(|secs| chrono::Utc::now().naive_utc() + chrono::Duration::seconds(secs));
            match &existing_poll {
                Some(ep) => {
                    // `@options[:poll][:options] != poll.options ||
                    // multiple != poll.multiple`: `reset_votes!`.
                    let options_changed =
                        ep.options != pf.options || ep.multiple != pf.multiple.unwrap_or(false);
                    if options_changed {
                        sqlx::query!("DELETE FROM poll_votes WHERE poll_id = $1", ep.id)
                            .execute(&mut *tx)
                            .await?;
                    }
                    sqlx::query!(
                        r#"UPDATE polls
                             SET options = $2, multiple = $3, hide_totals = $4, expires_at = $5,
                                 cached_tallies = CASE WHEN $6
                                     THEN ARRAY(SELECT 0::bigint FROM unnest($2::varchar[]))
                                     ELSE cached_tallies END,
                                 votes_count = CASE WHEN $6 THEN 0 ELSE votes_count END,
                                 voters_count = CASE WHEN $6 THEN 0 ELSE voters_count END,
                                 updated_at = now()
                           WHERE id = $1"#,
                        ep.id,
                        &opts as &[String],
                        pf.multiple.unwrap_or(false),
                        pf.hide_totals.unwrap_or(false),
                        expires_at,
                        options_changed,
                    )
                    .execute(&mut *tx)
                    .await?;
                }
                None => {
                    // `polls.new(votes_count: 0)`, then `reset_votes!` as its
                    // options changed from none: a zero tally for each
                    // option, and no voters.
                    let poll_id = sqlx::query_scalar!(
                        r#"INSERT INTO polls (status_id, account_id, options, multiple, hide_totals, expires_at,
                                              cached_tallies, votes_count, voters_count, created_at, updated_at)
                           VALUES ($1, $2, $3, $4, $5, $6,
                                   ARRAY(SELECT 0::bigint FROM unnest($3::varchar[])), 0, 0, now(), now())
                           RETURNING id"#,
                        id,
                        auth.account_id,
                        &opts as &[String],
                        pf.multiple.unwrap_or(false),
                        pf.hide_totals.unwrap_or(false),
                        expires_at,
                    )
                    .fetch_one(&mut *tx)
                    .await?;
                    sqlx::query!(
                        "UPDATE statuses SET poll_id = $1 WHERE id = $2",
                        poll_id,
                        id
                    )
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        _ => {
            // `previous_poll.destroy`, its votes and notifications with it.
            if let Some(ep) = &existing_poll {
                crate::remove_status::destroy_poll_in(&mut tx, ep.id)
                    .await
                    .map_err(AppError::Internal)?;
            }
        }
    }

    // `update_immediate_attributes!`.
    sqlx::query!(
        "UPDATE statuses SET text = $1, spoiler_text = $2, sensitive = $3, language = $4, quote_approval_policy = $6, edited_at = now() WHERE id = $5",
        new_text, new_spoiler, new_sensitive, new_language, id, new_quote_policy,
    )
    .execute(&mut *tx)
    .await?;
    crate::status_snapshot::create_edit(&mut tx, id, auth.account_id).await?;
    tx.commit().await?;

    // `update_metadata!`.
    store_statuses_tags(&state, id, auth.account_id, &hashtags).await?;
    store_status_mentions(&state, id, &resolved).await?;
    // `update_index('statuses', :proper)`.
    crate::search::elasticsearch::indexing::status(&state, id).await;
    // `UpdateStatusService#reset_preview_card!`: a changed text gets its card
    // afresh.
    if new_text != status.text {
        crate::preview_card::reset(&state, id).await;
        crate::preview_card::crawl(&state, id).await;
    }
    // `queue_poll_notifications!`, with the poll's end before the edit
    // (`@previous_expires_at`).
    let previous_expires_at = existing_poll.as_ref().and_then(|p| p.expires_at);
    if let Err(error) = crate::api::mastodon::polls::queue_poll_notifications(
        &state,
        id,
        previous_expires_at,
        false,
    )
    .await
    {
        tracing::error!(status_id = id, %error, "could not queue a poll's expiration notice");
    }

    // `broadcast_updates!`: `DistributionWorker` with `update`, which tells
    // the boosters and quoters, and the timelines that have it.
    crate::quotes::distribute_update(&state, id, false).await;

    let (updated_status, _) = fetch_status_with_account(&state, id).await?;
    let api_status = serialize_status(&state, &updated_status, Some(auth.account_id)).await?;

    if let Err(e) = federate_status_update(&state, id, &account, &updated_status).await {
        tracing::warn!(status_id = id, error = %e, "failed to enqueue ActivityPub status update");
    }
    // `after_update_commit :trigger_update_webhooks`.
    crate::moderation::webhooks::trigger(
        &state,
        "status.updated",
        crate::moderation::webhooks::Object::Status(id),
    )
    .await;
    crate::fasp::events::status_updated(&state, id).await;

    Ok(Json(api_status))
}

// ── GET /api/v1/statuses/:id/history ──────────────────────────────────────

pub async fn get_status_history(
    state: AppState,
    Path(id): Path<i64>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Vec<StatusEdit>>> {
    let status = sqlx::query_as!(
        DbStatus,
        "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let viewer_id = auth.as_ref().map(|Extension(a)| a.account_id);
    match viewer_id {
        Some(vid) => check_status_visible(&state, &status, vid).await?,
        None => {
            if !matches!(
                status.visibility,
                crate::db::models::vis::PUBLIC | crate::db::models::vis::UNLISTED
            ) {
                return Err(AppError::NotFound);
            }
        }
    }

    Ok(Json(status_edits(&state, &status).await?))
}

/// `HistoriesController#status_edits`: the post's `status_edits` rows,
/// oldest first (`ordered`, by id), the last one the post as it is; or, for
/// a post never edited, a snapshot of it built on the spot at its
/// `edited_at || created_at`. Each rendered as `REST::StatusEditSerializer`
/// does, with the account that made that version.
pub(crate) async fn status_edits(
    state: &AppState,
    status: &DbStatus,
) -> AppResult<Vec<StatusEdit>> {
    let id = status.id;
    let author = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = $1",
        status.account_id,
    )
    .fetch_one(&state.db)
    .await?;

    // Named columns: `media_descriptions` holds NULL for an attachment that
    // had no description, which a `SELECT *` would read as `Vec<String>`
    // and refuse, failing the whole history.
    let mut edits = sqlx::query_as!(
        crate::db::models::StatusEdit,
        r#"SELECT id, status_id, account_id, text, spoiler_text, sensitive, created_at,
                  media_descriptions AS "media_descriptions: Vec<Option<String>>",
                  ordered_media_attachment_ids, poll_options, quote_id, updated_at
           FROM status_edits WHERE status_id = $1 ORDER BY id ASC"#,
        id,
    )
    .fetch_all(&state.db)
    .await?;
    if edits.is_empty() {
        // `[@status.build_snapshot(at_time: @status.edited_at ||
        // @status.created_at)]`.
        let mut conn = state.db.acquire().await?;
        if let Some(snapshot) = crate::status_snapshot::build(&mut conn, id).await? {
            let at = status.edited_at.unwrap_or(status.created_at);
            edits.push(crate::db::models::StatusEdit {
                id: 0,
                status_id: id,
                account_id: Some(snapshot.account_id),
                text: snapshot.text,
                spoiler_text: snapshot.spoiler_text,
                sensitive: Some(snapshot.sensitive),
                created_at: at,
                media_descriptions: Some(snapshot.media_descriptions),
                ordered_media_attachment_ids: Some(snapshot.ordered_media_attachment_ids),
                poll_options: snapshot.poll_options,
                quote_id: snapshot.quote_id,
                updated_at: at,
            });
        }
    }

    // Every version is rendered the same way: upstream's serializer runs
    // `status_content_format` over each edit just as it does over a status.
    // A `StatusEdit` does not respond to `active_mentions`, so the only
    // account preloaded for its mentions is the author's, and a mention of
    // anyone else stays text.
    let local_domain = state.urls.local_domain.clone();
    let render = |text: &str| -> String {
        crate::api::mastodon::formatting::status_content(&local_domain, text, &author, &[])
    };

    // `has_one :account`: whoever made each version — the author, or the
    // instance's representative for a moderator's change — and none for an
    // edit whose account is gone.
    let account_ids: Vec<i64> = edits
        .iter()
        .filter_map(|e| e.account_id)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let editors = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = ANY($1)",
        &account_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let editor_emojis = batch_account_emojis(state, &editors).await;
    let editor_roles = batch_account_roles(state, &editors).await;
    let mut api_editors = std::collections::HashMap::new();
    for editor in &editors {
        let mut api_account = account_from_db(&state.urls, editor);
        api_account.emojis = editor_emojis.get(&editor.id).cloned().unwrap_or_default();
        api_account.roles = editor_roles.get(&editor.id).cloned().unwrap_or_default();
        crate::api::mastodon::accounts::apply_account_stats(state, &mut api_account, editor.id)
            .await;
        api_editors.insert(editor.id, api_account);
    }

    // `status.media_attachments.index_by(&:id)`: the post's attachments,
    // those taken off by an edit included.
    let fetched_media: Vec<crate::db::models::MediaAttachment> = sqlx::query_as!(
        crate::db::models::MediaAttachment,
        "SELECT * FROM media_attachments WHERE status_id = $1",
        id,
    )
    .fetch_all(&state.db)
    .await?;
    let media_map: std::collections::HashMap<i64, &crate::db::models::MediaAttachment> =
        fetched_media.iter().map(|m| (m.id, m)).collect();

    // A version's attachments carry the descriptions they had then, as
    // Mastodon's `PreservedMediaAttachment` does: `descriptions[i]` for the
    // attachment at position `i`, none where it had none. An edit recorded
    // without them keeps each attachment's description as it is now.
    let ordered_media = |ids: Option<&Vec<i64>>,
                         descriptions: Option<&Vec<Option<String>>>|
     -> Vec<crate::api::mastodon::types::MediaAttachment> {
        ids.map(|list| {
            list.iter()
                .enumerate()
                .filter_map(|(position, id)| Some((position, media_map.get(id)?)))
                .map(|(position, m)| {
                    let mut media = crate::api::mastodon::convert::media_from_db(&state.urls, m);
                    if let Some(descriptions) = descriptions {
                        media.description = descriptions.get(position).cloned().flatten();
                    }
                    media
                })
                .filter(|m| {
                    m.url.is_some() || m.remote_url.as_deref().is_some_and(|u| !u.is_empty())
                })
                .take(4)
                .collect()
        })
        .unwrap_or_default()
    };

    Ok(edits
        .iter()
        .map(|e| {
            let poll = e.poll_options.as_ref().filter(|o| !o.is_empty()).map(|opts| {
                serde_json::json!({
                    "options": opts.iter().map(|t| serde_json::json!({ "title": t })).collect::<Vec<_>>()
                })
            });
            StatusEdit {
                content: render(&e.text),
                spoiler_text: e.spoiler_text.clone(),
                sensitive: e.sensitive.unwrap_or(false),
                created_at: crate::api::mastodon::convert::mastodon_date(e.created_at),
                account: e.account_id.and_then(|a| api_editors.get(&a).cloned()),
                media_attachments: ordered_media(
                    e.ordered_media_attachment_ids.as_ref(),
                    e.media_descriptions.as_ref(),
                ),
                emojis: vec![],
                poll,
                quote: None,
            }
        })
        .collect())
}

// ── GET /api/v1/statuses/:id/source ───────────────────────────────────────

pub async fn get_status_source(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<StatusSource>> {
    auth.require_scope("read:statuses")?;
    let status = sqlx::query_as!(
        DbStatus,
        "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    // Mastodon allows any authenticated user who has visibility to the status
    // to read its source — not just the author.
    match status.visibility {
        crate::db::models::vis::PRIVATE => {
            let is_author = status.account_id == auth.account_id;
            let is_follower = sqlx::query_scalar!(
                "SELECT 1 as e FROM follows WHERE account_id = $1 AND target_account_id = $2",
                auth.account_id,
                status.account_id,
            )
            .fetch_optional(&state.db)
            .await?
            .is_some();
            if !is_author && !is_follower {
                return Err(AppError::NotFound);
            }
        }
        crate::db::models::vis::DIRECT => {
            let is_author = status.account_id == auth.account_id;
            let is_mentioned = sqlx::query_scalar!(
                "SELECT 1 as e FROM mentions WHERE status_id = $1 AND account_id = $2",
                id,
                auth.account_id,
            )
            .fetch_optional(&state.db)
            .await?
            .is_some();
            if !is_author && !is_mentioned {
                return Err(AppError::NotFound);
            }
        }
        _ => {}
    }

    Ok(Json(StatusSource {
        id: status.id.to_string(),
        text: status.text,
        spoiler_text: status.spoiler_text,
    }))
}
