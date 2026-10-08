//! Editing statuses: `PUT /statuses/:id`, plus the edit-history and source
//! endpoints (`/history`, `/source`).

use super::*;

// ── PUT /api/v1/statuses/:id ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct EditMediaAttribute {
    pub id: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EditStatusForm {
    pub status: Option<String>,
    pub spoiler_text: Option<String>,
    pub sensitive: Option<bool>,
    pub language: Option<String>,
    pub media_ids: Option<Vec<String>>,
    pub media_attributes: Option<Vec<EditMediaAttribute>>,
    // Double-option so we can tell an absent `poll` (no change) from an explicit
    // `poll: null` (remove the poll) — Mastodon keys off `options.key?(:poll)`.
    #[serde(default, deserialize_with = "double_option")]
    pub poll: Option<Option<PollForm>>,
    /// `update_options[:quote_approval_policy] = quote_approval_policy if
    /// status_params[:quote_approval_policy].present?`
    pub quote_approval_policy: Option<String>,
}

fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(de).map(Some)
}

pub async fn edit_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<EditStatusForm>,
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
        Some(crate::api::mastodon::DEFAULT_LOCALE),
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

    // Save the current version to the edit history before updating. The snapshot
    // is stamped with the version's own creation time (Mastodon snapshots with
    // `at_time: edited_at || created_at`), not the moment it is superseded, and
    // carries that version's media order and poll options so `/history` renders
    // each past version faithfully.
    let snapshot_at = status.edited_at.unwrap_or(status.created_at);
    // `ordered_media_attachment_ids&.dup || media_attachments.pluck(:id)`.
    let snapshot_media = match status.ordered_media_attachment_ids.clone() {
        Some(ids) => ids,
        None => {
            sqlx::query_scalar!(
                "SELECT id FROM media_attachments WHERE status_id = $1 ORDER BY id",
                id,
            )
            .fetch_all(&state.db)
            .await?
        }
    };
    let snapshot_poll = existing_poll.as_ref().map(|p| p.options.clone());
    // Each attachment's description as it is now, before this edit changes
    // any: Mastodon's `media_descriptions`
    // (`ordered_media_attachments.map(&:description)`), which `/history`
    // shows each past version with.
    let snapshot_descriptions: Vec<Option<String>> = previous_media
        .iter()
        .map(|m| m.description.clone())
        .collect();
    sqlx::query!(
        r#"INSERT INTO status_edits (status_id, account_id, text, spoiler_text, sensitive, ordered_media_attachment_ids, media_descriptions, poll_options, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now())"#,
        id, auth.account_id, status.text, status.spoiler_text, status.sensitive,
        &snapshot_media, &snapshot_descriptions as &[Option<String>],
        snapshot_poll.as_deref(), snapshot_at,
    )
    .execute(&state.db)
    .await?;

    let hashtags = extract_hashtags(&new_text);
    let mention_handles = extract_mention_handles(&new_text);
    let resolved = resolve_mention_accounts(&state, &mention_handles, &instance_domain).await;

    sqlx::query!(
        "UPDATE statuses SET text = $1, spoiler_text = $2, sensitive = $3, language = $4, quote_approval_policy = $6, edited_at = now() WHERE id = $5",
        new_text, new_spoiler, new_sensitive, new_language, id, new_quote_policy,
    )
    .execute(&state.db)
    .await?;

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
            .execute(&state.db)
            .await?;
        }
        sqlx::query!(
            "UPDATE media_attachments SET status_id = $1 WHERE id = ANY($2) AND status_id IS NULL",
            id,
            next,
        )
        .execute(&state.db)
        .await?;
        sqlx::query!(
            "UPDATE statuses SET ordered_media_attachment_ids = $2 WHERE id = $1",
            id,
            next,
        )
        .execute(&state.db)
        .await?;
    }

    // Apply the poll change (Mastodon resets votes when options change; an
    // explicit poll:null removes the poll).
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
                        let _ = sqlx::query!("DELETE FROM poll_votes WHERE poll_id = $1", ep.id)
                            .execute(&state.db)
                            .await;
                    }
                    // `UpdateStatusService#update_poll!`: `reset_votes!` when
                    // the options changed, the tallies kept otherwise.
                    let _ = sqlx::query!(
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
                    .execute(&state.db)
                    .await;
                }
                None => {
                    if let Ok(poll_id) = sqlx::query_scalar!(
                        // `polls.new(votes_count: 0)`, a zero tally for each
                        // option, and no `voters_count`.
                        r#"INSERT INTO polls (status_id, account_id, options, multiple, hide_totals, expires_at,
                                              cached_tallies, votes_count, created_at, updated_at)
                           VALUES ($1, $2, $3, $4, $5, $6,
                                   ARRAY(SELECT 0::bigint FROM unnest($3::varchar[])), 0, now(), now())
                           RETURNING id"#,
                        id,
                        auth.account_id,
                        &opts as &[String],
                        pf.multiple.unwrap_or(false),
                        pf.hide_totals.unwrap_or(false),
                        expires_at,
                    )
                    .fetch_one(&state.db)
                    .await
                    {
                            let _ = sqlx::query!(
                            "UPDATE statuses SET poll_id = $1 WHERE id = $2",
                            poll_id, id,
                        )
                        .execute(&state.db)
                        .await;
                    }
                }
            }
        }
        Some(None) => {
            if let Some(ep) = &existing_poll {
                let _ = sqlx::query!("DELETE FROM poll_votes WHERE poll_id = $1", ep.id)
                    .execute(&state.db)
                    .await;
                let _ = sqlx::query!("UPDATE statuses SET poll_id = NULL WHERE id = $1", id)
                    .execute(&state.db)
                    .await;
                let _ = sqlx::query!("DELETE FROM polls WHERE id = $1", ep.id)
                    .execute(&state.db)
                    .await;
            }
        }
        None => {}
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

/// Every version of `status`, oldest first and the current one last, as
/// `REST::StatusEditSerializer` renders `status.edits` and the status itself.
pub(crate) async fn status_edits(
    state: &AppState,
    status: &DbStatus,
) -> AppResult<Vec<StatusEdit>> {
    let id = status.id;
    let account = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = $1",
        status.account_id,
    )
    .fetch_one(&state.db)
    .await?;

    // Named columns: `media_descriptions` holds NULL for an attachment that
    // had no description, which a `SELECT *` would read as `Vec<String>`
    // and refuse, failing the whole history.
    let edits = sqlx::query_as!(
        crate::db::models::StatusEdit,
        r#"SELECT id, status_id, account_id, text, spoiler_text, sensitive, created_at,
                  media_descriptions AS "media_descriptions: Vec<Option<String>>",
                  ordered_media_attachment_ids, poll_options, quote_id, updated_at
           FROM status_edits WHERE status_id = $1 ORDER BY created_at ASC"#,
        id,
    )
    .fetch_all(&state.db)
    .await?;

    // Every version is rendered the same way, current and historical alike:
    // upstream's serializer runs `status_content_format` over each edit just
    // as it does over a status. A `StatusEdit` does not respond to
    // `active_mentions`, so the only account preloaded for its mentions is the
    // author's, and a mention of anyone else stays text.
    let local_domain = state.urls.local_domain.clone();
    let render = |text: &str| -> String {
        crate::api::mastodon::formatting::status_content(&local_domain, text, &account, &[])
    };
    let current_content = render(&status.text);

    let account_emojis = batch_account_emojis(state, std::slice::from_ref(&account)).await;
    let account_roles = batch_account_roles(state, std::slice::from_ref(&account)).await;
    let mut api_account = account_from_db(&state.urls, &account);
    api_account.emojis = account_emojis.get(&account.id).cloned().unwrap_or_default();
    api_account.roles = account_roles.get(&account.id).cloned().unwrap_or_default();
    crate::api::mastodon::accounts::apply_account_stats(state, &mut api_account, account.id).await;

    // Collect all media attachment IDs needed across all edits, then batch-fetch them.
    let all_media_ids: Vec<i64> = edits
        .iter()
        .filter_map(|e| e.ordered_media_attachment_ids.as_ref())
        .flat_map(|ids| ids.iter().copied())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let fetched_media: Vec<crate::db::models::MediaAttachment> = if all_media_ids.is_empty() {
        vec![]
    } else {
        sqlx::query_as!(
            crate::db::models::MediaAttachment,
            "SELECT * FROM media_attachments WHERE id = ANY($1)",
            &all_media_ids,
        )
        .fetch_all(&state.db)
        .await?
    };
    let media_map: std::collections::HashMap<i64, &crate::db::models::MediaAttachment> =
        fetched_media.iter().map(|m| (m.id, m)).collect();

    // A past version's attachments carry the descriptions they had then, as
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
                .collect()
        })
        .unwrap_or_default()
    };

    let mut result: Vec<StatusEdit> = edits.iter().map(|e| {
        let poll = e.poll_options.as_ref().filter(|o| !o.is_empty()).map(|opts| {
            serde_json::json!({ "options": opts.iter().map(|t| serde_json::json!({"title": t})).collect::<Vec<_>>() })
        });
        StatusEdit {
            content: render(&e.text),
            spoiler_text: e.spoiler_text.clone(),
            sensitive: e.sensitive.unwrap_or(false),
            created_at: crate::api::mastodon::convert::mastodon_date(e.created_at),
            account: api_account.clone(),
            media_attachments: ordered_media(
                e.ordered_media_attachment_ids.as_ref(),
                e.media_descriptions.as_ref(),
            ),
            emojis: vec![],
            poll,
            quote: None,
        }
    }).collect();

    // Current version poll — render its options so the latest history entry
    // matches Mastodon (which snapshots poll_options on every edit).
    let current_poll = if status.poll_id.is_some() {
        sqlx::query_scalar!(
            "SELECT options FROM polls WHERE status_id = $1",
            id,
        )
        .fetch_optional(&state.db)
        .await?
        .map(|opts: Vec<String>| {
            serde_json::json!({
                "options": opts.iter().map(|t| serde_json::json!({ "title": t })).collect::<Vec<_>>()
            })
        })
    } else {
        None
    };

    // The current version, as `build_snapshot` has it: the status's
    // `ordered_media_attachments`, every attachment by id when it has no
    // order recorded.
    let current_media = crate::api::mastodon::status_serialize::fetch_status_media(state, id)
        .await?
        .iter()
        .map(|m| crate::api::mastodon::convert::media_from_db(&state.urls, m))
        .filter(|m| m.url.is_some() || m.remote_url.as_deref().is_some_and(|u| !u.is_empty()))
        .collect();

    // Append current version
    result.push(StatusEdit {
        content: current_content,
        spoiler_text: status.spoiler_text.clone(),
        sensitive: status.sensitive,
        created_at: crate::api::mastodon::convert::mastodon_date(
            status.edited_at.unwrap_or(status.created_at),
        ),
        account: api_account,
        media_attachments: current_media,
        emojis: vec![],
        poll: current_poll,
        quote: None,
    });

    Ok(result)
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
