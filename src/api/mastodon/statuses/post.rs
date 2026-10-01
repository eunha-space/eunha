//! Creating statuses: `POST /api/v1/statuses`, including multipart/form
//! parsing, poll/media/quote assembly, scheduling, and federation.

use super::*;

// ── POST /api/v1/statuses ──────────────────────────────────────────────────

pub async fn post_status(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    request: axum::extract::Request,
) -> AppResult<axum::response::Response> {
    use axum::response::IntoResponse;
    auth.require_scope("write:statuses")?;

    // Capture the Idempotency-Key header before the request body is consumed.
    let idempotency_key = request
        .headers()
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let form = extract_post_status_form(request).await?;
    let account = fetch_account(&state, auth.account_id).await?;

    // If we've already processed a request with this key, replay the stored status.
    if let Some(ref ik) = idempotency_key {
        use redis::AsyncCommands;
        let redis_key = state
            .redis_keys
            .key(format!("idempotency:{}:{}", auth.account_id, ik));
        let mut redis = state.redis_coordination.clone();
        if let Ok(Some(existing_id)) = redis.get::<_, Option<i64>>(&redis_key).await {
            if let Some(status) = sqlx::query_as!(
                DbStatus,
                "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
                existing_id,
            )
            .fetch_optional(&state.db)
            .await?
            {
                let media = fetch_status_media(&state, status.id).await?;
                let viewer_ctx = build_viewer_context(&state, auth.account_id, status.id)
                    .await
                    .ok();
                let api_status = crate::api::mastodon::status_serialize::build_status_with_app(
                    &state, &status, &account, media, None, viewer_ctx, None,
                )
                .await?;
                return Ok((axum::http::StatusCode::OK, Json(api_status)).into_response());
            }
        }
    }
    let mut text = form.status.clone().unwrap_or_default();
    let mut spoiler_text = form.spoiler_text.clone().unwrap_or_default();
    // Mastodon PostStatusService#preprocess_attributes promotes a lone content
    // warning (no body, no quote) into the body, leaving no CW. `sensitive` is
    // still forced on below because the CW was present when it was evaluated.
    let spoiler_was_present = !spoiler_text.is_empty();
    if text.is_empty() && spoiler_was_present && form.quoted_status_id.is_none() {
        text = std::mem::take(&mut spoiler_text);
    }
    if text.is_empty()
        && form.media_ids.as_ref().is_none_or(|m| m.is_empty())
        && form.poll.is_none()
    {
        return Err(AppError::Unprocessable(
            "Status must have text or media".into(),
        ));
    }
    // Mastodon StatusLengthValidator: spoiler + body, URLs as 23 chars, mentions
    // without their domain, counted in grapheme clusters.
    if crate::api::mastodon::formatting::countable_length(&text, &spoiler_text) > 500 {
        return Err(AppError::Unprocessable(
            "Validation failed: Text character limit of 500 exceeded".into(),
        ));
    }

    // Validate poll options before inserting anything
    if let Some(ref poll_form) = form.poll {
        validate_poll_form(poll_form)?;
    }

    // Handle scheduled statuses. Mastodon's PostStatusService ignores a
    // scheduled_at in the past (posts immediately); otherwise ScheduledStatus
    // must be at least MINIMUM_OFFSET (5 min) in the future and is bounded by
    // total (300) and daily (25) per-account limits.
    if let Some(ref scheduled_at_str) = form.scheduled_at {
        let scheduled_at = chrono::DateTime::parse_from_rfc3339(scheduled_at_str)
            .map(|t| t.with_timezone(&chrono::Utc).naive_utc())
            .map_err(|_| AppError::Unprocessable("Invalid scheduled_at format".into()))?;
        let now = chrono::Utc::now().naive_utc();
        // Past dates fall through and post immediately.
        if scheduled_at > now {
            if scheduled_at <= now + chrono::Duration::minutes(5) {
                return Err(AppError::Unprocessable(
                    "Validation failed: Scheduled date must be in the future".into(),
                ));
            }
            let total = sqlx::query_scalar!(
                "SELECT COUNT(*) FROM scheduled_statuses WHERE account_id = $1",
                account.id,
            )
            .fetch_one(&state.db)
            .await?
            .unwrap_or(0);
            if total >= 300 {
                return Err(AppError::Unprocessable(
                    "Validation failed: Total number of scheduled statuses exceeded".into(),
                ));
            }
            let daily = sqlx::query_scalar!(
                "SELECT COUNT(*) FROM scheduled_statuses WHERE account_id = $1 AND scheduled_at::date = $2",
                account.id,
                scheduled_at.date(),
            )
            .fetch_one(&state.db)
            .await?
            .unwrap_or(0);
            if daily >= 25 {
                return Err(AppError::Unprocessable(
                    "Validation failed: Daily number of scheduled statuses exceeded".into(),
                ));
            }
            let params = serde_json::json!({
                "text": text,
                "visibility": form.visibility,
                "spoiler_text": spoiler_text,
                "sensitive": form.sensitive,
                "language": form.language,
                "in_reply_to_id": form.in_reply_to_id,
                "media_ids": form.media_ids,
                "poll": form.poll.as_ref().map(|p| serde_json::json!({
                    "options": p.options,
                    "expires_in": p.expires_in,
                    "multiple": p.multiple,
                    "hide_totals": p.hide_totals,
                })),
            });
            let row = sqlx::query!(
                r#"INSERT INTO scheduled_statuses (account_id, scheduled_at, params)
                   VALUES ($1, $2, $3)
                   RETURNING id, scheduled_at"#,
                account.id,
                scheduled_at,
                params,
            )
            .fetch_one(&state.db)
            .await?;
            state.queues.scheduled_statuses.notify_one();
            let resp = ScheduledStatusResponse {
                id: row.id.to_string(),
                scheduled_at: row
                    .scheduled_at
                    .map(crate::api::mastodon::convert::mastodon_date),
                params,
                media_attachments: vec![],
            };
            return Ok((axum::http::StatusCode::CREATED, Json(resp)).into_response());
        }
    }

    // Reject an unrecognized visibility rather than silently coercing it (the
    // fallback maps unknown strings to `direct`, which would turn a typo into a
    // DM). Mastodon only accepts these client-settable visibilities.
    if let Some(v) = form.visibility.as_deref() {
        if !matches!(v, "public" | "unlisted" | "private" | "direct") {
            return Err(AppError::Unprocessable(format!(
                "Validation failed: Visibility is not included in the list: {v}"
            )));
        }
    }

    // Fall back to the user's stored posting defaults when the form omits them.
    let defaults = crate::api::mastodon::accounts::user_defaults(&state, auth.account_id).await;
    let mut visibility = form
        .visibility
        .as_deref()
        .map(str::to_owned)
        .unwrap_or(defaults.privacy);
    // A silenced account cannot post publicly: Mastodon downgrades public to
    // unlisted so the post stays out of public and federated timelines.
    if visibility == "public" && account.silenced_at.is_some() {
        visibility = "unlisted".to_string();
    }
    // Mastodon forces sensitive when a content warning is present
    // (PostStatusService: `sensitive || spoiler_text.present?`).
    let sensitive = form.sensitive.unwrap_or(defaults.sensitive) || spoiler_was_present;
    // Mastodon's `valid_locale_cascade(options[:language], user's preferred
    // posting language, I18n.default_locale)` — a status always ends up with a
    // language, so a client offering a translation or filtering by one has
    // something to read. eunha stopped at the user's setting and left it null.
    let language = form
        .language
        .clone()
        .or(defaults.language)
        .or_else(|| Some(crate::api::mastodon::DEFAULT_LOCALE.to_string()));
    let in_reply_to_id = form
        .in_reply_to_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());

    // Look up the parent author for in_reply_to_account_id
    let in_reply_to_account_id: Option<i64> = if let Some(parent_id) = in_reply_to_id {
        let account_id = sqlx::query_scalar!(
            "SELECT account_id FROM statuses WHERE id = $1 AND deleted_at IS NULL",
            parent_id,
        )
        .fetch_optional(&state.db)
        .await?;
        if account_id.is_none() {
            return Err(AppError::Unprocessable(
                "in_reply_to_id does not exist".into(),
            ));
        }
        account_id
    } else {
        None
    };

    // Validate quoted_status_id
    let mut quoted_author_id: Option<i64> = None;
    let quote_of_id: Option<i64> = if let Some(ref qid_str) = form.quoted_status_id {
        let qid = qid_str
            .parse::<i64>()
            .map_err(|_| AppError::Unprocessable("invalid quoted_status_id".into()))?;
        let quoted = sqlx::query!(
            "SELECT id, account_id, visibility, reblog_of_id FROM statuses WHERE id = $1 AND deleted_at IS NULL",
            qid,
        )
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::Unprocessable("quoted_status_id does not exist".into()))?;
        // Cannot quote direct messages
        if quoted.visibility == 3 {
            return Err(AppError::Unprocessable(
                "cannot quote a direct message".into(),
            ));
        }
        // Cannot quote a reblog; must quote the original post directly
        if quoted.reblog_of_id.is_some() {
            return Err(AppError::Unprocessable("cannot quote a reblog".into()));
        }
        // Quoting a followers-only post forces the quote down to followers-only,
        // so the quoted content is never exposed to a wider audience than the
        // original (Mastodon PostStatusService#preprocess_attributes).
        if quoted.visibility == crate::db::models::vis::PRIVATE
            && matches!(visibility.as_str(), "public" | "unlisted")
        {
            visibility = "private".to_string();
        }
        // Block check against quoted author
        let blocked = sqlx::query_scalar!(
            r#"SELECT 1 FROM blocks
               WHERE (account_id = $1 AND target_account_id = $2)
                  OR (account_id = $2 AND target_account_id = $1)
               LIMIT 1"#,
            account.id,
            quoted.account_id,
        )
        .fetch_optional(&state.db)
        .await?;
        if blocked.is_some() {
            return Err(AppError::Unprocessable(
                "not allowed to interact with this post".into(),
            ));
        }
        // `authorize(@quoted_status, :quote?)`: its policy must not deny us.
        let relation = sqlx::query!(
            r#"SELECT
                 EXISTS (SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2) AS "follows_author!",
                 EXISTS (SELECT 1 FROM follows WHERE account_id = $2 AND target_account_id = $1) AS "followed_by_author!",
                 (SELECT quote_approval_policy FROM statuses WHERE id = $3) AS "policy!""#,
            account.id,
            quoted.account_id,
            quoted.id,
        )
        .fetch_one(&state.db)
        .await?;
        if crate::db::models::quote_policy::for_account(
            relation.policy,
            account.id == quoted.account_id,
            relation.follows_author,
            relation.followed_by_author,
        ) == crate::db::models::quote_policy::ForAccount::Denied
        {
            return Err(AppError::Forbidden);
        }
        quoted_author_id = Some(quoted.account_id);
        Some(quoted.id)
    } else {
        None
    };

    let hashtags = extract_hashtags(&text);
    let mention_handles = extract_mention_handles(&text);
    let resolved = resolve_mention_accounts(&state, &mention_handles, &instance.domain).await;

    // Mastodon safeguard_private_mention_quote!: a direct post that quotes
    // someone else's status must mention that author, otherwise they would be
    // quoted into a conversation they cannot see.
    if visibility == "direct" {
        if let Some(qauthor) = quoted_author_id {
            if qauthor != account.id && !resolved.iter().any(|(_, a)| a.id == qauthor) {
                return Err(AppError::Unprocessable(
                    "Validation failed: The quoted user must be mentioned in a direct message"
                        .into(),
                ));
            }
        }
    }

    // Safeguard: if the caller passed allowed_mentions, reject the post if any resolved
    // mentions are not in that list (mirrors Mastodon's PostStatusService#safeguard_mentions!).
    if let Some(ref allowed_ids) = form.allowed_mentions {
        let unexpected: Vec<serde_json::Value> = resolved
            .iter()
            .filter(|(_, acct)| !allowed_ids.iter().any(|aid| aid == &acct.id.to_string()))
            .map(|(_, acct)| serde_json::json!({ "id": acct.id.to_string(), "acct": acct.acct() }))
            .collect();
        if !unexpected.is_empty() {
            let body = serde_json::json!({
                "error": "These accounts will be mentioned, but you did not explicitly select them",
                "unexpected_accounts": unexpected,
            });
            return Ok((axum::http::StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response());
        }
    }

    let status_id = crate::snowflake::next_id();
    let uri = crate::federation::tag::status_uri(
        &instance.domain,
        account.id,
        account.id_scheme,
        &account.username,
        status_id,
    );
    // Human permalink — always the /@username form, independent of id_scheme.
    let human_url = format!(
        "https://{}/@{}/{}",
        instance.domain, account.username, status_id
    );

    // Validate media_ids before inserting the status (Mastodon
    // PostStatusService#validate_media!) — fail early so no cleanup is needed.
    let parsed_media_ids: Vec<i64> = if let Some(ref ids) = form.media_ids {
        // Reject more than the 4-attachment limit outright.
        if ids.len() > MEDIA_ATTACHMENTS_LIMIT {
            return Err(AppError::Unprocessable(format!(
                "Validation failed: Cannot attach more than {MEDIA_ATTACHMENTS_LIMIT} files"
            )));
        }
        let mut parsed = Vec::with_capacity(ids.len());
        let mut has_audio_or_video = false;
        let mut any_not_ready = false;
        for id_str in ids {
            let media_id = id_str.parse::<i64>().map_err(|_| {
                AppError::Unprocessable(format!("media_ids: invalid id '{}'", id_str))
            })?;
            let row = sqlx::query!(
                r#"SELECT "type", processing FROM media_attachments
                   WHERE id = $1 AND account_id = $2 AND status_id IS NULL"#,
                media_id,
                account.id,
            )
            .fetch_optional(&state.db)
            .await?;
            let Some(row) = row else {
                return Err(AppError::Unprocessable(format!(
                    "media_ids: '{}' not found, already attached, or not owned by you",
                    id_str
                )));
            };
            // audio(3)/video(2) can't be combined with other media (gifv is exempt,
            // matching MediaAttachment#audio_or_video?).
            if matches!(row.r#type, 2 | 3) {
                has_audio_or_video = true;
            }
            // processing set and not complete(2) means still processing/failed.
            if row.processing.is_some_and(|p| p != 2) {
                any_not_ready = true;
            }
            parsed.push(media_id);
        }
        if parsed.len() > 1 && has_audio_or_video {
            return Err(AppError::Unprocessable(
                "Validation failed: Cannot attach a video or audio file to a post that contains other media".into(),
            ));
        }
        if any_not_ready {
            return Err(AppError::Unprocessable(
                "Validation failed: Cannot attach files that have not finished processing. Try again in a moment!".into(),
            ));
        }
        parsed
    } else {
        vec![]
    };

    let is_reply = in_reply_to_id.is_some();
    let visibility_int = crate::db::models::vis::from_str(&visibility);
    // `Api::InteractionPoliciesConcern#quote_approval_policy`, then
    // `downgrade_quote_policy` for a post only some may see.
    let requested_policy = form
        .quote_approval_policy
        .as_deref()
        .filter(|p| !p.is_empty())
        .unwrap_or(&defaults.quote_policy);
    let quote_policy_int = match crate::db::models::quote_policy::from_api(requested_policy) {
        Some(_)
            if !matches!(
                visibility_int,
                crate::db::models::vis::PUBLIC | crate::db::models::vis::UNLISTED
            ) =>
        {
            0
        }
        Some(policy) => policy,
        None => {
            return Err(AppError::Unprocessable(
                "Validation failed: Quote approval policy is invalid".into(),
            ))
        }
    };
    let status = sqlx::query_as!(
        DbStatus,
        r#"INSERT INTO statuses
             (id, account_id, application_id, text, spoiler_text, visibility,
              language, sensitive, in_reply_to_id, in_reply_to_account_id, reply, uri, url,
              quote_approval_policy, local, created_at, updated_at)
           VALUES ($1,$2,$10,$3,$4,$5,$6,$7,$8,$9,$12,$11,$14,$13, true, now(), now())
           RETURNING *"#,
        status_id,
        account.id,
        text,
        spoiler_text,
        visibility_int,
        language,
        sensitive,
        in_reply_to_id,
        in_reply_to_account_id,
        auth.application_id,
        uri,
        is_reply,
        quote_policy_int,
        human_url,
    )
    .fetch_one(&state.db)
    .await?;

    // Create a quotes record if this is a quote post. When the quoted author is
    // remote we ask for consent via a FEP-044f QuoteRequest (sent below) and keep
    // the quote pending until they Accept; local quotes accept by visibility.
    let mut quote_request_activity_uri: Option<String> = None;
    if let Some(qid) = quote_of_id {
        let quoted = sqlx::query!(
            "SELECT s.account_id, s.visibility, s.quote_approval_policy, a.domain
             FROM statuses s JOIN accounts a ON a.id = s.account_id
             WHERE s.id = $1",
            qid,
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();

        let quoted_is_remote = quoted.as_ref().and_then(|q| q.domain.clone()).is_some();
        let quoted_account_id = quoted.as_ref().map(|q| q.account_id).unwrap_or(account.id);

        let (quote_state, activity_uri) = if quoted_is_remote {
            let au = format!(
                "https://{}/users/{}/quote_requests/{}",
                instance.domain,
                account.username,
                crate::snowflake::next_id()
            );
            quote_request_activity_uri = Some(au.clone());
            (crate::db::models::quote_state::PENDING, Some(au))
        } else {
            // `quote.accept! if @quoted_status.local? && StatusPolicy#quote?`:
            // the policy was checked before the post was made, so a local
            // quote is accepted.
            (crate::db::models::quote_state::ACCEPTED, None)
        };

        let quote_row_id = crate::snowflake::next_id();
        let _ = sqlx::query!(
            r#"INSERT INTO quotes (id, status_id, quoted_status_id, account_id, quoted_account_id, activity_uri, state, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, now(), now())
               ON CONFLICT DO NOTHING"#,
            quote_row_id,
            status.id,
            qid,
            account.id,
            quoted_account_id,
            activity_uri,
            quote_state,
        )
        .execute(&state.db)
        .await;

        // Accepted quotes count toward the quoted status's quotes_count.
        if quote_state == crate::db::models::quote_state::ACCEPTED {
            let _ = sqlx::query!(
                r#"INSERT INTO status_stats (status_id, quotes_count, created_at, updated_at)
                   VALUES ($1, 1, now(), now())
                   ON CONFLICT (status_id) DO UPDATE
                     SET quotes_count = status_stats.quotes_count + 1, updated_at = now()"#,
                qid,
            )
            .execute(&state.db)
            .await;
        }
    }

    // Store tags and mentions
    store_statuses_tags(&state, status.id, account.id, &hashtags).await?;
    store_status_mentions(&state, status.id, &resolved).await?;

    // Mastodon assigns a conversation_id to every status. For replies, inherit
    // the parent's conversation; otherwise create a new one.
    let conv_id = if let Some(parent_id) = in_reply_to_id {
        sqlx::query_scalar!(
            "SELECT conversation_id FROM statuses WHERE id = $1",
            parent_id
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .flatten()
    } else {
        None
    };

    let conv_id = if let Some(cid) = conv_id {
        cid
    } else {
        sqlx::query_scalar!(
            "INSERT INTO conversations (created_at, updated_at) VALUES (now(), now()) RETURNING id",
        )
        .fetch_one(&state.db)
        .await?
    };

    sqlx::query!(
        "UPDATE statuses SET conversation_id = $1 WHERE id = $2",
        conv_id,
        status.id
    )
    .execute(&state.db)
    .await?;

    sqlx::query!(
        "UPDATE conversations SET updated_at = now() WHERE id = $1",
        conv_id
    )
    .execute(&state.db)
    .await?;

    // For direct messages, also manage the account_conversations inbox.
    if visibility == "direct" {
        // Build sorted participant ID lists for each party's account_conversations row.
        // Mastodon convention: participant_account_ids = everyone else in the conversation.
        let mut mentioned_ids: Vec<i64> = resolved
            .iter()
            .map(|(_, m)| m.id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        mentioned_ids.sort_unstable();

        // Sender sees the mentioned accounts as participants.
        sqlx::query!(
            r#"INSERT INTO account_conversations
                   (account_id, conversation_id, participant_account_ids, status_ids, last_status_id, unread)
               VALUES ($1, $2, $3, ARRAY[$4::bigint], $4, false)
               ON CONFLICT (account_id, conversation_id, participant_account_ids) DO UPDATE
                   SET unread         = false,
                       last_status_id = EXCLUDED.last_status_id,
                       status_ids     = array_append(account_conversations.status_ids, EXCLUDED.last_status_id),
                       lock_version   = account_conversations.lock_version + 1"#,
            account.id, conv_id, &mentioned_ids, status.id
        )
        .execute(&state.db)
        .await?;

        // Each recipient sees the sender (plus other recipients) as participants.
        for (_, mentioned) in &resolved {
            let mut recipient_participants: Vec<i64> = std::iter::once(account.id)
                .chain(
                    resolved
                        .iter()
                        .filter(|(_, m)| m.id != mentioned.id)
                        .map(|(_, m)| m.id),
                )
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            recipient_participants.sort_unstable();

            sqlx::query!(
                r#"INSERT INTO account_conversations
                       (account_id, conversation_id, participant_account_ids, status_ids, last_status_id, unread)
                   VALUES ($1, $2, $3, ARRAY[$4::bigint], $4, true)
                   ON CONFLICT (account_id, conversation_id, participant_account_ids) DO UPDATE
                       SET unread         = true,
                           last_status_id = EXCLUDED.last_status_id,
                           status_ids     = array_append(account_conversations.status_ids, EXCLUDED.last_status_id),
                           lock_version   = account_conversations.lock_version + 1"#,
                mentioned.id, conv_id, &recipient_participants, status.id
            )
            .execute(&state.db)
            .await?;
        }
    }

    // What happened, not what to count: the rules live in `counters`.
    if let Err(e) = crate::counters::on_status_created(
        &state.db,
        account.id,
        visibility_int,
        in_reply_to_id,
        status.created_at,
    )
    .await
    {
        tracing::error!(status_id = status.id, error = %e, "failed to count a new status");
    }

    // Attach media (IDs already validated above)
    for media_id in &parsed_media_ids {
        sqlx::query!(
            "UPDATE media_attachments SET status_id = $1
             WHERE id = $2 AND account_id = $3 AND status_id IS NULL",
            status.id,
            media_id,
            account.id
        )
        .execute(&state.db)
        .await?;
    }

    // Create poll if requested (options already validated above)
    if let Some(ref poll_form) = form.poll {
        let expires_at = poll_form
            .expires_in
            .map(|secs| chrono::Utc::now().naive_utc() + chrono::Duration::seconds(secs));
        let poll_options: Vec<String> = poll_form.options.clone();
        let poll_id = sqlx::query_scalar!(
            r#"INSERT INTO polls
                 (status_id, account_id, options, multiple, hide_totals, expires_at, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, now(), now())
               RETURNING id"#,
            status.id, account.id, &poll_options as &[String],
            poll_form.multiple.unwrap_or(false),
            poll_form.hide_totals.unwrap_or(false),
            expires_at,
        )
        .fetch_one(&state.db)
        .await?;
        state.queues.polls.notify_one();
        // Link the poll back onto the status, mirroring the federation ingest
        // path so `statuses.poll_id` is consistently populated for local polls.
        sqlx::query!(
            "UPDATE statuses SET poll_id = $1 WHERE id = $2",
            poll_id,
            status.id,
        )
        .execute(&state.db)
        .await?;
    }

    let mut status = status;
    status.uri = Some(uri.clone());

    // Load the application that created this status (for the author's view)
    let application = if let Some(app_id) = auth.application_id {
        sqlx::query!(
            "SELECT name, website FROM oauth_applications WHERE id = $1",
            app_id,
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .map(|r| crate::api::mastodon::types::Application {
            name: r.name,
            website: r.website,
        })
    } else {
        None
    };

    let media = fetch_status_media(&state, status.id).await?;
    let viewer_ctx = build_viewer_context(&state, auth.account_id, status.id)
        .await
        .ok();
    let api_status = crate::api::mastodon::status_serialize::build_status_with_app(
        &state,
        &status,
        &account,
        media,
        None,
        viewer_ctx,
        application,
    )
    .await?;

    // `LinkCrawlWorker.perform_async(@status.id)`.
    crate::preview_card::crawl(&state, status.id);

    if matches!(visibility.as_str(), "public" | "unlisted" | "private") {
        if let Ok(payload) = serde_json::to_string(&api_status) {
            let hashtags: Vec<String> = api_status.tags.iter().map(|t| t.name.clone()).collect();
            state.streaming.publish(Event::NewStatus {
                author_id: account.id,
                is_public: visibility == "public",
                is_direct: visibility == "direct",
                status_id: status.id,
                hashtags,
                has_media: !api_status.media_attachments.is_empty(),
                payload: std::sync::Arc::new(payload),
            });
        }
    }

    // Notify the author of the parent status if this is a reply
    let mut notified = std::collections::HashSet::new();
    if let Some(parent_id) = in_reply_to_id {
        if let Ok(Some(parent)) = sqlx::query!(
            "SELECT account_id FROM statuses WHERE id = $1 AND deleted_at IS NULL",
            parent_id,
        )
        .fetch_optional(&state.db)
        .await
        {
            push::create_and_push(
                &state,
                parent.account_id,
                account.id,
                "mention",
                Some(status.id),
                format!("{} mentioned you", account.display_name),
                account.acct().clone(),
                crate::api::mastodon::convert::account_avatar_url_for(&state.urls, &account),
            )
            .await;
            notified.insert(parent.account_id);
        }
    }

    // Notify each mentioned account not already notified above
    for (_, mentioned) in &resolved {
        if mentioned.id == account.id || notified.contains(&mentioned.id) {
            continue;
        }
        push::create_and_push(
            &state,
            mentioned.id,
            account.id,
            "mention",
            Some(status.id),
            format!("{} mentioned you", account.display_name),
            account.acct().clone(),
            crate::api::mastodon::convert::account_avatar_url_for(&state.urls, &account),
        )
        .await;
        notified.insert(mentioned.id);
    }

    // Notify followers who opted in to per-account posting notifications (the
    // "bell"). Mastodon's FeedInsertWorker#notify? excludes replies to other
    // accounts (self-replies still notify), reblogs, and edits.
    let is_reply_to_other = in_reply_to_id.is_some() && in_reply_to_account_id != Some(account.id);
    if (visibility == "public" || visibility == "unlisted") && !is_reply_to_other {
        if let Ok(followers) = sqlx::query!(
            r#"SELECT account_id FROM follows
               WHERE target_account_id = $1 AND notify = true"#,
            account.id,
        )
        .fetch_all(&state.db)
        .await
        {
            for row in followers {
                if notified.contains(&row.account_id) {
                    continue;
                }
                push::create_and_push(
                    &state,
                    row.account_id,
                    account.id,
                    "status",
                    Some(status.id),
                    format!("{} posted a new status", account.display_name),
                    account.acct().clone(),
                    crate::api::mastodon::convert::account_avatar_url_for(&state.urls, &account),
                )
                .await;
            }
        }
    }

    // Fan-out to follower feeds and list feeds in background (non-blocking)
    {
        let tag_ids: Vec<i64> = sqlx::query_scalar!(
            "SELECT tag_id FROM statuses_tags WHERE status_id = $1",
            status.id
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

        let mut redis = state.redis.clone();
        let redis_keys = state.redis_keys.clone();
        let db = state.db.clone();
        let author_id = account.id;
        let status_id = status.id;
        let reply_to_account = in_reply_to_account_id;
        let vis = visibility.clone();
        if feed::sync_fanout() {
            feed::fanout_new_status(&mut redis, &redis_keys, &db, author_id, status_id, &tag_ids)
                .await;
            feed::fanout_to_lists(
                &mut redis,
                &redis_keys,
                &db,
                author_id,
                status_id,
                reply_to_account,
                &vis,
            )
            .await;
        } else {
            crate::tenants::spawn(async move {
                feed::fanout_new_status(
                    &mut redis,
                    &redis_keys,
                    &db,
                    author_id,
                    status_id,
                    &tag_ids,
                )
                .await;
                feed::fanout_to_lists(
                    &mut redis,
                    &redis_keys,
                    &db,
                    author_id,
                    status_id,
                    reply_to_account,
                    &vis,
                )
                .await;
            });
        }
    }

    // Federate outgoing statuses to remote inboxes
    if matches!(
        visibility.as_str(),
        "public" | "unlisted" | "private" | "direct"
    ) && crate::federation::keypair::has_signing_key(&state, account.id)
        .await
        .unwrap_or(false)
    {
        let domain = &instance.domain;
        let actor_url = crate::federation::tag::account_uri_of(domain, &account);
        let key_id = format!("{}#main-key", actor_url);

        // Build the Create(Note) from the persisted status so the wire shape
        // matches what we serve at the note's own URI (content, media
        // attachments, and the mention/hashtag/emoji tag array).
        let Some(bundle) = crate::api::ap::note::build_note(&state, domain, status.id).await?
        else {
            return Err(AppError::Internal(anyhow::anyhow!(
                "failed to build Note for status {}",
                status.id
            )));
        };
        // Keep a copy of the (context-less) Note to inline as the QuoteRequest
        // `instrument` below, before the bundle is consumed by `into_create`.
        let quote_note = quote_of_id.map(|_| bundle.note.clone());
        let activity = bundle.into_create();

        // FEP-044f: ask a remote quoted author for consent to quote them.
        if let (Some(qr_uri), Some(qid)) = (&quote_request_activity_uri, quote_of_id) {
            let quoted_uri: Option<String> =
                sqlx::query_scalar!("SELECT uri FROM statuses WHERE id = $1", qid)
                    .fetch_optional(&state.db)
                    .await
                    .ok()
                    .flatten()
                    .flatten();
            if let (Some(quoted_status_uri), Ok(Some(qa))) = (
                quoted_uri,
                sqlx::query!(
                    "SELECT a.inbox_url, a.shared_inbox_url
                         FROM statuses s JOIN accounts a ON a.id = s.account_id
                         WHERE s.id = $1",
                    qid,
                )
                .fetch_optional(&state.db)
                .await,
            ) {
                let qinbox = if !qa.shared_inbox_url.is_empty() {
                    qa.shared_inbox_url
                } else {
                    qa.inbox_url
                };
                if !qinbox.is_empty() {
                    if let Ok(mut qr) = crate::federation::consent::quote_request(
                        qr_uri,
                        &actor_url,
                        &quoted_status_uri,
                        &uri,
                    ) {
                        // Inline the quote Note as `instrument` (Mastodon's
                        // `allow_post_inlining`): the quoted author's server
                        // validates the request against the embedded object
                        // rather than dereferencing it, so quoting works even
                        // for non-public posts it cannot fetch from us.
                        if let Some(note) = quote_note.clone() {
                            inline_quote_instrument(&mut qr, note);
                        }
                        if let Err(e) = crate::federation::delivery::deliver_to_inboxes(
                            &state,
                            qr,
                            vec![qinbox],
                            key_id.clone(),
                        )
                        .await
                        {
                            tracing::warn!(error = %e, "failed to enqueue QuoteRequest");
                        }
                    }
                }
            }
        }

        // Reach the full status audience (StatusReachFinder): followers +
        // mentions + replied-to author + quoted author + relays (public).
        use crate::db::models::vis;
        let vis_int = vis::from_str(&visibility);
        let inboxes = crate::federation::delivery::status_reach_inboxes(
            &state,
            status.id,
            account.id,
            in_reply_to_account_id,
            matches!(vis_int, vis::PUBLIC | vis::UNLISTED),
            false,
            vis_int == vis::PUBLIC,
            matches!(vis_int, vis::PUBLIC | vis::UNLISTED | vis::PRIVATE),
            None,
            &[],
        )
        .await
        .unwrap_or_default();
        if !inboxes.is_empty() {
            if let Err(e) =
                crate::federation::delivery::deliver_to_inboxes(&state, activity, inboxes, key_id)
                    .await
            {
                tracing::warn!(error = %e, "failed to enqueue status delivery");
            }
        }
    }

    // Record the idempotency mapping so a retried request replays this status.
    if let Some(ref ik) = idempotency_key {
        use redis::AsyncCommands;
        let redis_key = state
            .redis_keys
            .key(format!("idempotency:{}:{}", auth.account_id, ik));
        let mut redis = state.redis_coordination.clone();
        let _: redis::RedisResult<()> = redis.set_ex(redis_key, status.id, 21600).await;
    }

    // `PostStatusService#postprocess_status!`: `Trends.tags.register`.
    crate::trends::register_tags(&state, status.id).await;

    // `after_create_commit :trigger_create_webhooks` for a local status.
    crate::moderation::webhooks::trigger(
        &state,
        "status.created",
        crate::moderation::webhooks::Object::Status(status.id),
    );
    Ok((axum::http::StatusCode::OK, Json(api_status)).into_response())
}

async fn extract_post_status_form(request: axum::extract::Request) -> AppResult<PostStatusForm> {
    let ct = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if ct.contains("application/json") {
        return axum::extract::Json::<PostStatusForm>::from_request(request, &())
            .await
            .map(|axum::extract::Json(f)| f)
            .map_err(|e| AppError::Unprocessable(e.to_string()));
    }

    if ct.contains("multipart/form-data") {
        let mut multipart = Multipart::from_request(request, &())
            .await
            .map_err(|e| AppError::Unprocessable(e.to_string()))?;
        let mut form = PostStatusForm::default();
        let mut media_ids: Vec<String> = Vec::new();
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|e| AppError::Unprocessable(e.to_string()))?
        {
            let name = field.name().unwrap_or("").to_string();
            let text = field
                .text()
                .await
                .map_err(|e| AppError::Unprocessable(e.to_string()))?;
            match name.as_str() {
                "status" => form.status = Some(text),
                "in_reply_to_id" => {
                    form.in_reply_to_id = if text.is_empty() { None } else { Some(text) }
                }
                "quoted_status_id" | "quote_id" => {
                    form.quoted_status_id = if text.is_empty() { None } else { Some(text) }
                }
                "quote_approval_policy" => {
                    form.quote_approval_policy = if text.is_empty() { None } else { Some(text) }
                }
                "spoiler_text" => {
                    form.spoiler_text = if text.is_empty() { None } else { Some(text) }
                }
                "visibility" => form.visibility = Some(text),
                "language" => form.language = if text.is_empty() { None } else { Some(text) },
                "sensitive" => form.sensitive = Some(text == "true" || text == "1"),
                "scheduled_at" => {
                    form.scheduled_at = if text.is_empty() { None } else { Some(text) }
                }
                "media_ids[]" | "media_ids" => {
                    if !text.is_empty() {
                        media_ids.push(text);
                    }
                }
                name if name.starts_with("poll[options]") || name == "poll[options][]" => {
                    if !text.is_empty() {
                        let p = form.poll.get_or_insert_with(PollForm::default);
                        p.options.push(text);
                    }
                }
                "poll[expires_in]" => {
                    if let Ok(n) = text.parse::<i64>() {
                        form.poll.get_or_insert_with(PollForm::default).expires_in = Some(n);
                    }
                }
                "poll[multiple]" => {
                    form.poll.get_or_insert_with(PollForm::default).multiple =
                        Some(text == "true" || text == "1");
                }
                "poll[hide_totals]" => {
                    form.poll.get_or_insert_with(PollForm::default).hide_totals =
                        Some(text == "true" || text == "1");
                }
                _ => {}
            }
        }
        if !media_ids.is_empty() {
            form.media_ids = Some(media_ids);
        }
        return Ok(form);
    }

    // Fall back to URL-encoded form
    axum::extract::Form::<PostStatusForm>::from_request(request, &())
        .await
        .map(|axum::extract::Form(f)| f)
        .map_err(|e| AppError::Unprocessable(e.to_string()))
}

// ── GET /api/v1/statuses/:id ───────────────────────────────────────────────
