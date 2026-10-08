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

    // `preprocess_attributes!`'s `@scheduled_at`: a time in the past is
    // ignored, and the status posts now. Whether the request schedules is
    // decided before `with_idempotency`, which looks its duplicate up by it.
    let scheduled_at = match form.scheduled_at.as_deref() {
        Some(s) => {
            let t = chrono::DateTime::parse_from_rfc3339(s)
                .map(|t| t.with_timezone(&chrono::Utc).naive_utc())
                .map_err(|_| AppError::Unprocessable("Invalid scheduled_at format".into()))?;
            (t > chrono::Utc::now().naive_utc()).then_some(t)
        }
        None => None,
    };

    // `with_idempotency`: under `with_redis_lock`, a key already recorded
    // answers with what it recorded, so that a request sent twice at once does
    // not post twice. The lock is held until this handler returns, after the
    // key is recorded below.
    let idempotency_key = idempotency_key.filter(|k| !k.trim().is_empty());
    let _idempotency_lock = match idempotency_key.as_deref() {
        Some(ik) => {
            let lock = crate::redis_lock::try_acquire(
                &state,
                &format!("lock:idempotency:lock:status:{}:{ik}", account.id),
                crate::redis_lock::DEFAULT_TTL_MS,
            )
            .await
            .ok_or_else(|| {
                AppError::ServiceUnavailable(
                    "There was a temporary problem serving your request, please try again".into(),
                )
            })?;
            if let Some(existing_id) = idempotency_duplicate(&state, account.id, ik).await {
                return replay_idempotent(
                    &state,
                    &auth,
                    &account,
                    existing_id,
                    scheduled_at.is_some(),
                )
                .await;
            }
            Some(lock)
        }
        None => None,
    };

    // `validate_media!`, the first thing `with_idempotency` runs, before the
    // status is built or scheduled.
    let parsed_media_ids =
        validate_media(&state, account.id, form.media_ids.as_deref(), None).await?;

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
    if let Some(scheduled_at) = scheduled_at {
        let now = chrono::Utc::now().naive_utc();
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
        // `scheduled_statuses.create!(media_attachments: @media, …)` in one
        // transaction: the uploads are the scheduled status's until it is
        // published, and `REST::ScheduledStatusSerializer` shows them.
        let mut tx = state.db.begin().await?;
        let scheduled_id = sqlx::query_scalar!(
            r#"INSERT INTO scheduled_statuses (account_id, scheduled_at, params)
               VALUES ($1, $2, $3)
               RETURNING id"#,
            account.id,
            scheduled_at,
            params,
        )
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE media_attachments SET scheduled_status_id = $1, updated_at = now()
             WHERE id = ANY($2) AND account_id = $3",
            scheduled_id,
            &parsed_media_ids,
            account.id,
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        state.queues.scheduled_statuses.notify_one();
        let resp = crate::api::mastodon::scheduled_statuses::load(&state, account.id, scheduled_id)
            .await?
            .ok_or(AppError::NotFound)?;
        record_idempotency(&state, account.id, idempotency_key.as_deref(), scheduled_id).await;
        // `render json: @status` with no status: 200, as for a status.
        return Ok((axum::http::StatusCode::OK, Json(resp)).into_response());
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
    let language = crate::languages::valid_locale_cascade(&[
        form.language.as_deref(),
        defaults.language.as_deref(),
        Some(crate::api::mastodon::DEFAULT_LOCALE),
    ]);
    let in_reply_to_id = form
        .in_reply_to_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());

    // The thread (`Status#thread`, a boost's original in its place) and
    // `carried_over_reply_to_account_id`.
    let (in_reply_to_id, in_reply_to_account_id) = if let Some(parent_id) = in_reply_to_id {
        let Some(thread) = crate::conversation::thread(&state.db, parent_id).await? else {
            return Err(AppError::Unprocessable(
                "in_reply_to_id does not exist".into(),
            ));
        };
        (Some(thread.id), thread.reply_to_account_id(auth.account_id))
    } else {
        (None, None)
    };

    // `set_quoted_status`: `Status.find(quoted_status_id)&.proper`, then
    // `authorize(@quoted_status, :quote?)`; any failure is the same 404.
    let mut quoted_author_id: Option<i64> = None;
    let quote_of_id: Option<i64> = if let Some(ref qid_str) = form.quoted_status_id {
        let quoted_not_found = || {
            AppError::NotFoundMsg(
                "The post you are trying to quote does not appear to exist.".into(),
            )
        };
        let qid = qid_str.parse::<i64>().map_err(|_| quoted_not_found())?;
        let found = sqlx::query!(
            r#"SELECT COALESCE(s.reblog_of_id, s.id) AS "id!" FROM statuses s
               WHERE s.id = $1 AND s.deleted_at IS NULL"#,
            qid,
        )
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(quoted_not_found)?;
        let quoted = sqlx::query_as!(
            DbStatus,
            "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
            found.id,
        )
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(quoted_not_found)?;
        // `StatusPolicy#quote?`: `show? && !blocking_author? &&
        // quote_policy_for_account(current_account) != :denied`.
        let relation = sqlx::query!(
            r#"SELECT
                 (a.suspended_at IS NOT NULL OR a.requested_deletion_at IS NOT NULL) AS "unavailable!",
                 EXISTS (SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2) AS "follows_author!",
                 EXISTS (SELECT 1 FROM follows WHERE account_id = $2 AND target_account_id = $1) AS "followed_by_author!",
                 EXISTS (SELECT 1 FROM blocks
                         WHERE (account_id = $1 AND target_account_id = $2)
                            OR (account_id = $2 AND target_account_id = $1)) AS "blocked!",
                 EXISTS (SELECT 1 FROM mentions WHERE status_id = $3 AND account_id = $1) AS "mentioned!"
               FROM accounts a WHERE a.id = $2"#,
            account.id,
            quoted.account_id,
            quoted.id,
        )
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(quoted_not_found)?;
        let own = account.id == quoted.account_id;
        use crate::db::models::vis;
        let shown = !relation.unavailable
            && match quoted.visibility {
                vis::DIRECT | vis::LIMITED => own || relation.mentioned,
                vis::PRIVATE => own || relation.follows_author || relation.mentioned,
                _ => own || !relation.blocked,
            };
        let policy_denies = quoted.visibility == vis::DIRECT
            || quoted.visibility == vis::LIMITED
            || crate::db::models::quote_policy::for_account(
                quoted.quote_approval_policy,
                own,
                relation.follows_author,
                relation.followed_by_author,
            ) == crate::db::models::quote_policy::ForAccount::Denied;
        if !shown || (relation.blocked && !own) || policy_denies {
            return Err(quoted_not_found());
        }
        // Quoting a followers-only post forces the quote down to followers-only,
        // so the quoted content is never exposed to a wider audience than the
        // original (Mastodon PostStatusService#preprocess_attributes).
        if quoted.visibility == vis::PRIVATE && matches!(visibility.as_str(), "public" | "unlisted")
        {
            visibility = "private".to_string();
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
                    "Validation failed: Cannot quote a non-mentioned user in a Private Mention post."
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
            // `raise ActiveRecord::RecordInvalid`, with no record to name.
            return Err(AppError::Unprocessable("Record invalid".into()));
        }
    };
    let status = sqlx::query_as!(
        DbStatus,
        r#"INSERT INTO statuses
             (id, account_id, application_id, text, spoiler_text, visibility,
              language, sensitive, in_reply_to_id, in_reply_to_account_id, reply, uri, url,
              quote_approval_policy, ordered_media_attachment_ids, local, created_at, updated_at)
           VALUES ($1,$2,$10,$3,$4,$5,$6,$7,$8,$9,$12,$11,$14,$13,$15, true, now(), now())
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
        &parsed_media_ids,
    )
    .fetch_one(&state.db)
    .await?;

    // `attach_quote!`: `Quote.create(quoted_status:, status:)`, accepted at
    // once when the quoted post is ours (the policy was checked above); a
    // remote author is asked (`QuoteRequestWorker`, below), and the request
    // is named under the quoter (`Quote#set_activity_uri`).
    let mut quote_row: Option<crate::quotes::Quote> = None;
    if let Some(qid) = quote_of_id {
        let quoted = sqlx::query!(
            "SELECT s.account_id, a.domain FROM statuses s JOIN accounts a ON a.id = s.account_id WHERE s.id = $1",
            qid,
        )
        .fetch_one(&state.db)
        .await?;
        let quoted_is_remote = quoted.domain.is_some();
        let activity_uri = quoted_is_remote.then(|| {
            format!(
                "{}/quote_requests/{}",
                crate::federation::tag::account_uri_of(&instance.domain, &account),
                uuid::Uuid::new_v4()
            )
        });
        let quote_state = if quoted_is_remote {
            crate::db::models::quote_state::PENDING
        } else {
            crate::db::models::quote_state::ACCEPTED
        };
        let quote_id = sqlx::query_scalar!(
            r#"INSERT INTO quotes (id, status_id, quoted_status_id, account_id, quoted_account_id, activity_uri, state, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, now(), now())
               ON CONFLICT DO NOTHING
               RETURNING id"#,
            crate::snowflake::next_id(),
            status.id,
            qid,
            account.id,
            quoted.account_id,
            activity_uri,
            quote_state,
        )
        .fetch_optional(&state.db)
        .await?;
        if let Some(quote_id) = quote_id {
            crate::quotes::created(&state.db, Some(qid), quote_state).await;
            quote_row = crate::quotes::find(&state.db, quote_id).await?;
        }
    }

    // Store tags and mentions
    store_statuses_tags(&state, status.id, account.id, &hashtags).await?;
    store_status_mentions(&state, status.id, &resolved).await?;
    // `status.quote.ensure_quoted_access`
    if let Some(quote) = &quote_row {
        crate::quotes::ensure_quoted_access(&state.db, quote).await;
    }

    // `set_conversation` and `update_conversation`.
    crate::conversation::assign(&state.db, status.id).await?;

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
    // `Status#update_statistics` and `PostStatusService#bump_potential_friendship!`.
    crate::activity_tracker::local_status_created(&state, visibility_int).await;
    if in_reply_to_id.is_some() && in_reply_to_account_id != Some(account.id) {
        crate::activity_tracker::increment(&state, crate::activity_tracker::INTERACTIONS).await;
    }
    // `update_index('statuses', :proper)` and the account's stats.
    crate::search::elasticsearch::indexing::status(&state, status.id).await;
    crate::search::elasticsearch::indexing::account(&state, account.id).await;

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
            // `PostStatusService#poll_attributes` (`voters_count: 0`) and
            // `Poll#prepare_cached_tallies`, a zero for each option.
            r#"INSERT INTO polls
                 (status_id, account_id, options, multiple, hide_totals, expires_at,
                  cached_tallies, votes_count, voters_count, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6,
                       ARRAY(SELECT 0::bigint FROM unnest($3::varchar[])), 0, 0, now(), now())
               RETURNING id"#,
            status.id,
            account.id,
            &poll_options as &[String],
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
    crate::preview_card::crawl(&state, status.id).await;
    // `process_email_subscriptions!`
    crate::email_subscriptions::status_posted(
        &state,
        &crate::email_subscriptions::PostedStatus {
            id: status.id,
            account_id: account.id,
            visibility: visibility.clone(),
            in_reply_to_id,
            in_reply_to_account_id,
        },
    )
    .await;

    let mut notified = std::collections::HashSet::new();
    // `notify_mentioned_accounts!`: the accounts it mentions, and only
    // those; a reply's parent author is told only if mentioned.
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

    // `notify_quoted_account!`: a local author whose post this accepted quote
    // quotes.
    if let Some(quote) = quote_row.as_ref().filter(|q| q.accepted()) {
        crate::quotes::notify(&state, quote).await;
    }

    // The "bell" — `FeedInsertWorker#notify?` — is the fan-out's
    // (`crate::feed::distribute`).

    // Fan-out to follower feeds and list feeds in background (non-blocking)
    crate::feed::distribute_later(&state, status.id).await;

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

        // `QuoteRequestWorker`: ask a remote quoted author for consent, at
        // their own inbox (`quoted_account.inbox_url`).
        if let Some(quote) = quote_row.as_ref().filter(|q| q.activity_uri.is_some()) {
            let quoted = match quote.quoted_status_id {
                Some(qid) => {
                    sqlx::query!(
                        r#"SELECT s.uri AS "uri?", a.inbox_url
                       FROM statuses s JOIN accounts a ON a.id = s.account_id
                       WHERE s.id = $1"#,
                        qid,
                    )
                    .fetch_optional(&state.db)
                    .await?
                }
                None => None,
            };
            if let Some(quoted) = quoted.filter(|q| !q.inbox_url.is_empty()) {
                if let (Some(quoted_status_uri), Some(request_uri)) =
                    (quoted.uri, quote.activity_uri.as_deref())
                {
                    if let Ok(mut qr) = crate::federation::consent::quote_request(
                        request_uri,
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
                        // `Quote#sign?` (`QuoteRequestWorker`).
                        if let Err(e) = crate::federation::delivery::deliver_to_inboxes_signed(
                            &state,
                            qr,
                            vec![quoted.inbox_url],
                            key_id.clone(),
                            crate::federation::delivery::LinkedData::UnlessAuthorizedFetch,
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
            let signed = crate::federation::delivery::LinkedData::for_status(
                matches!(vis_int, vis::PUBLIC | vis::UNLISTED),
                crate::federation::delivery::LinkedData::UnlessAuthorizedFetch,
            );
            let synchronize = crate::federation::followers_synchronization::synchronizes(
                &state, account.id, vis_int,
            )
            .await;
            if let Err(e) = crate::federation::delivery::deliver_status_to_inboxes(
                &state,
                activity,
                inboxes,
                key_id,
                signed,
                synchronize,
            )
            .await
            {
                tracing::warn!(error = %e, "failed to enqueue status delivery");
            }
        }
    }

    // Record the idempotency key so a retried request replays this status.
    record_idempotency(&state, account.id, idempotency_key.as_deref(), status.id).await;

    // `PostStatusService#postprocess_status!`: `Trends.tags.register`.
    crate::trends::register_tags(&state, status.id).await;

    // `after_create_commit :trigger_create_webhooks` for a local status.
    crate::moderation::webhooks::trigger(
        &state,
        "status.created",
        crate::moderation::webhooks::Object::Status(status.id),
    )
    .await;
    crate::fasp::events::status_created(&state, status.id).await;
    Ok((axum::http::StatusCode::OK, Json(api_status)).into_response())
}

/// Mastodon's `Integer#to_i` on a string: its leading digits, or 0.
fn ruby_to_i(s: &str) -> i64 {
    let s = s.trim_start();
    let (sign, digits) = match s.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    digits[..end].parse::<i64>().map_or(0, |n| sign * n)
}

/// `PostStatusService#validate_media!`: the account's uploads not yet on a
/// status that `media_ids` names, in the order it names them. An upload on a
/// scheduled status counts as free, as upstream's `where(status_id: nil)`
/// has it. A refusal is `Mastodon::ValidationError`'s 422, which carries the
/// message without `Validation failed:`.
///
/// With `editing`, `UpdateStatusService#validate_media!`: the edited
/// status's own uploads count too, and one on a scheduled status does not.
pub(crate) async fn validate_media(
    state: &AppState,
    account_id: i64,
    media_ids: Option<&[String]>,
    editing: Option<i64>,
) -> AppResult<Vec<i64>> {
    let Some(ids) = media_ids.filter(|ids| !ids.is_empty()) else {
        return Ok(vec![]);
    };
    if ids.len() > MEDIA_ATTACHMENTS_LIMIT {
        return Err(AppError::Unprocessable(format!(
            "Cannot attach more than {MEDIA_ATTACHMENTS_LIMIT} files"
        )));
    }
    let wanted: Vec<i64> = ids.iter().map(|id| ruby_to_i(id)).collect();
    let rows = sqlx::query!(
        r#"SELECT id, "type", processing FROM media_attachments
           WHERE account_id = $1 AND id = ANY($2)
             AND (status_id IS NULL OR status_id = $3)
             AND ($3::bigint IS NULL OR scheduled_status_id IS NULL)"#,
        account_id,
        &wanted,
        editing,
    )
    .fetch_all(&state.db)
    .await?;
    let not_found: Vec<String> = wanted
        .iter()
        .filter(|id| !rows.iter().any(|r| r.id == **id))
        .map(i64::to_string)
        .collect();
    if !not_found.is_empty() {
        return Err(AppError::Unprocessable(format!(
            "Media {} not found or already attached to another post",
            not_found.join(", ")
        )));
    }
    // `audio_or_video?`: audio (3) or video (2), not gifv.
    if rows.len() > 1 && rows.iter().any(|r| matches!(r.r#type, 2 | 3)) {
        return Err(AppError::Unprocessable(
            "Cannot attach a video to a post that already contains images".into(),
        ));
    }
    // `not_processed?`: processing set and not `complete` (2).
    if rows.iter().any(|r| r.processing.is_some_and(|p| p != 2)) {
        return Err(AppError::Unprocessable(
            "Cannot attach files that have not finished processing. Try again in a moment!".into(),
        ));
    }
    let mut found = Vec::with_capacity(wanted.len());
    for id in wanted {
        if !found.contains(&id) {
            found.push(id);
        }
    }
    Ok(found)
}

/// `PostStatusService#idempotency_key`.
fn idempotency_redis_key(state: &AppState, account_id: i64, key: &str) -> String {
    state
        .redis_keys
        .key(format!("idempotency:status:{account_id}:{key}"))
}

/// `idempotency_duplicate?`: the id a request with this key already created.
async fn idempotency_duplicate(state: &AppState, account_id: i64, key: &str) -> Option<i64> {
    use redis::AsyncCommands;
    let mut redis = state.redis_coordination.clone();
    let id: Option<String> = redis
        .get(idempotency_redis_key(state, account_id, key))
        .await
        .ok()
        .flatten();
    id.and_then(|id| id.parse().ok())
}

/// `redis.setex(idempotency_key, 3_600, @status.id)`, for a status or a
/// scheduled status alike.
async fn record_idempotency(state: &AppState, account_id: i64, key: Option<&str>, id: i64) {
    use redis::AsyncCommands;
    let Some(key) = key else {
        return;
    };
    let mut redis = state.redis_coordination.clone();
    let _: redis::RedisResult<()> = redis
        .set_ex(idempotency_redis_key(state, account_id, key), id, 3_600)
        .await;
}

/// `raise IdempotencyError, idempotency_duplicate`: what the first request
/// created, looked up as this request would have created it — among the
/// account's scheduled statuses when it schedules, among its statuses
/// otherwise. `find` raises when it is not there, which is a 404.
async fn replay_idempotent(
    state: &AppState,
    auth: &AuthenticatedUser,
    account: &Account,
    id: i64,
    scheduled: bool,
) -> AppResult<axum::response::Response> {
    use axum::response::IntoResponse;
    if scheduled {
        let scheduled_status =
            crate::api::mastodon::scheduled_statuses::load(state, account.id, id)
                .await?
                .ok_or(AppError::NotFound)?;
        return Ok((axum::http::StatusCode::OK, Json(scheduled_status)).into_response());
    }
    let status = sqlx::query_as!(
        DbStatus,
        "SELECT * FROM statuses WHERE id = $1 AND account_id = $2 AND deleted_at IS NULL",
        id,
        account.id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let media = fetch_status_media(state, status.id).await?;
    let viewer_ctx = build_viewer_context(state, auth.account_id, status.id)
        .await
        .ok();
    let api_status = crate::api::mastodon::status_serialize::build_status_with_app(
        state, &status, account, media, None, viewer_ctx, None,
    )
    .await?;
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
