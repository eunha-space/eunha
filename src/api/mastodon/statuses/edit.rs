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
    // Double-option so we can tell an absent `poll` (no change) from an explicit
    // `poll: null` (remove the poll) — Mastodon keys off `options.key?(:poll)`.
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

    // Compute the proposed new values.
    let new_text = form.status.clone().unwrap_or_else(|| status.text.clone());
    let new_spoiler = form
        .spoiler_text
        .clone()
        .unwrap_or_else(|| status.spoiler_text.clone());
    // Mastodon StatusLengthValidator: spoiler + body, URLs as 23 chars, mentions
    // without their domain, counted in grapheme clusters.
    if crate::api::mastodon::formatting::countable_length(&new_text, &new_spoiler) > 500 {
        return Err(AppError::Unprocessable(
            "Validation failed: Text character limit of 500 exceeded".into(),
        ));
    }
    // Mastodon forces sensitive when a content warning is present.
    let new_sensitive = form.sensitive.unwrap_or(status.sensitive) || !new_spoiler.is_empty();
    // `UpdateStatusService`: `valid_locale_cascade(options[:language],
    // status.language, user's preferred posting language,
    // I18n.default_locale)`.
    let preferred = crate::api::mastodon::accounts::user_defaults(&state, status.account_id)
        .await
        .language;
    let new_language = crate::languages::valid_locale_cascade(&[
        form.language.as_deref(),
        status.language.as_deref(),
        preferred.as_deref(),
        Some(state.instance.default_locale()),
    ]);

    // `update_media_attachments! if @options.key?(:media_ids)`: the next
    // attachments, validated, in the order asked for. The media changed when
    // that order is not the one the status had (`ordered_media_attachments`),
    // or when a `media_attributes` entry for one of them changes its
    // description (`significantly_changed?`).
    let previous_media: Vec<crate::db::models::MediaAttachment> =
        crate::api::mastodon::status_serialize::fetch_status_media(&state, id).await?;
    let next_media: Option<Vec<i64>> = match form.media_ids.as_deref() {
        Some(ids) => Some(validate_media(&state, auth.account_id, Some(ids), Some(id)).await?),
        None => None,
    };
    let media_descriptions: Vec<(i64, String)> = match (&next_media, &form.media_attributes) {
        (Some(next), Some(attrs)) => attrs
            .iter()
            .filter_map(|attr| {
                let media_id = attr.id.parse::<i64>().ok()?;
                let description = attr.description.clone()?;
                next.contains(&media_id).then_some((media_id, description))
            })
            .collect(),
        _ => Vec::new(),
    };
    let descriptions_changed = if media_descriptions.is_empty() {
        false
    } else {
        let ids: Vec<i64> = media_descriptions.iter().map(|(id, _)| *id).collect();
        let current = sqlx::query!(
            "SELECT id, description FROM media_attachments WHERE id = ANY($1)",
            &ids,
        )
        .fetch_all(&state.db)
        .await?;
        media_descriptions.iter().any(|(media_id, description)| {
            current
                .iter()
                .find(|m| m.id == *media_id)
                .is_some_and(|m| m.description.as_deref() != Some(description.as_str()))
        })
    };
    let media_changed = descriptions_changed
        || next_media
            .as_ref()
            .is_some_and(|next| previous_media.iter().map(|m| m.id).collect::<Vec<_>>() != *next);

    // Poll editing (Mastodon UpdateStatusService#update_poll!): a poll in the
    // request creates or updates one; changing options resets votes.
    if let Some(Some(pf)) = &form.poll {
        validate_poll_form(pf)?;
    }
    let existing_poll = sqlx::query!(
        "SELECT id, options, multiple, hide_totals, expires_at FROM polls WHERE status_id = $1",
        id,
    )
    .fetch_optional(&state.db)
    .await?;
    let poll_changed = match (&form.poll, &existing_poll) {
        (Some(Some(pf)), Some(ep)) => {
            pf.options != ep.options
                || pf.multiple.unwrap_or(false) != ep.multiple
                || pf.hide_totals.unwrap_or(false) != ep.hide_totals
                // `@poll_changed = true if @previous_expires_at !=
                // preloadable_poll&.expires_at`: `expires_in=` counts from
                // now, so a poll given again ends at another time.
                || pf.expires_in.is_some()
                || ep.expires_at.is_some()
        }
        (Some(Some(_)), None) => true, // adding a poll
        (Some(None), Some(_)) => true, // explicit poll:null removes it
        (Some(None), None) => false,
        (None, _) => false, // absent: no change
    };

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

    // Mastodon only records an edit (and bumps edited_at / notifies) when the
    // submission actually changes the status; a no-op edit returns it as-is.
    let significant = new_text != status.text
        || new_spoiler != status.spoiler_text
        || new_sensitive != status.sensitive
        || new_language != status.language
        || new_quote_policy != status.quote_approval_policy
        || media_changed
        || poll_changed;

    if !significant {
        return Ok(Json(
            serialize_status(&state, &status, Some(auth.account_id)).await?,
        ));
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

    // `update_media_attachments!`: the descriptions given for the next
    // attachments, the added ones attached, and the order recorded. An
    // attachment taken off stays attached, so that the versions in the
    // history that showed it still can.
    if let Some(next) = &next_media {
        for (media_id, description) in &media_descriptions {
            sqlx::query!(
                "UPDATE media_attachments SET description = $1, updated_at = now() WHERE id = $2",
                description,
                media_id,
            )
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query!(
            "UPDATE media_attachments SET status_id = $1 WHERE id = ANY($2) AND status_id IS NULL",
            id,
            next,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE statuses SET ordered_media_attachment_ids = $2 WHERE id = $1",
            id,
            next,
        )
        .execute(&mut *tx)
        .await?;
    }

    // `update_poll!`: a poll in the request creates or updates one, and
    // changing its options resets the votes; an explicit `poll: null`
    // removes it.
    match &form.poll {
        Some(Some(pf)) => {
            let expires_at = pf
                .expires_in
                .map(|secs| chrono::Utc::now().naive_utc() + chrono::Duration::seconds(secs));
            let opts: Vec<String> = pf.options.clone();
            match &existing_poll {
                Some(ep) => {
                    let options_changed =
                        ep.options != opts || ep.multiple != pf.multiple.unwrap_or(false);
                    if options_changed {
                        sqlx::query!("DELETE FROM poll_votes WHERE poll_id = $1", ep.id)
                            .execute(&mut *tx)
                            .await?;
                    }
                    // `reset_votes!` when the options changed, the tallies
                    // kept otherwise.
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
        Some(None) => {
            if let Some(ep) = &existing_poll {
                sqlx::query!("DELETE FROM poll_votes WHERE poll_id = $1", ep.id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query!("UPDATE statuses SET poll_id = NULL WHERE id = $1", id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query!("DELETE FROM polls WHERE id = $1", ep.id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        None => {}
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
    // `queue_poll_notifications!`, with the poll's end before the edit when
    // the edit gave a poll (`@previous_expires_at`).
    let previous_expires_at = match &form.poll {
        Some(_) => existing_poll.as_ref().and_then(|p| p.expires_at),
        None => None,
    };
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
