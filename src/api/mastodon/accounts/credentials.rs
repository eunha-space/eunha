//! The authenticated account's own credentials and profile: verify,
//! update_credentials (avatar/header/fields), the /profile endpoints, and
//! propagating profile Updates to the fediverse.

use super::*;
use crate::media::picture::{self, Fit, Picture};

// ── GET /api/v1/accounts/verify_credentials ────────────────────────────────

pub async fn verify_credentials(
    state: AppState,
    Extension(ResolvedInstance(_instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<ApiAccount>> {
    auth.require_scope("read:accounts")?;
    let account = fetch_account(&state, auth.account_id).await?;
    let mut api_account = account_from_db(&state.urls, &account);
    api_account.emojis = fetch_account_emojis(&state, &account).await;
    apply_account_stats(&state, &mut api_account, account.id).await;

    let d = user_defaults(&state, account.id).await;
    let (default_privacy, default_sensitive, default_language, default_quote_policy) =
        (d.privacy, d.sensitive, d.language, d.quote_policy);

    let follow_requests: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM (SELECT 1 FROM follow_requests WHERE target_account_id = $1 LIMIT 40) sub",
        account.id
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    api_account.source = Some(crate::api::mastodon::types::AccountSource {
        privacy: default_privacy,
        sensitive: default_sensitive,
        language: default_language,
        note: account.note.clone(),
        fields: crate::api::mastodon::convert::fields_from_db(
            account.fields.as_ref().unwrap_or(&serde_json::json!([])),
        ),
        follow_requests_count: follow_requests,
        discoverable: account.discoverable,
        indexable: account.indexable,
        hide_collections: account.hide_collections,
        attribution_domains: account.attribution_domains.clone().unwrap_or_default(),
        quote_policy: default_quote_policy,
    });

    api_account.roles = fetch_account_roles(&state, account.id).await;
    api_account.role = fetch_account_role(&state, account.id).await;
    api_account.email_subscriptions =
        crate::email_subscriptions::serialized(&state, &account).await;

    Ok(Json(api_account))
}

// ── PATCH /api/v1/accounts/update_credentials ─────────────────────────────

/// `Account::Avatar::AVATAR_GEOMETRY`: `400x400#`.
const AVATAR_FIT: Fit = Fit::Cover(400, 400);
/// `Account::Header::HEADER_MAX_PIXELS`: 1500×500.
const HEADER_FIT: Fit = Fit::Pixels(750_000);

/// An avatar or header as it is stored, with the content type of what is
/// stored: upright, fitted and without its metadata, as Mastodon's
/// `lazy_thumbnail` leaves it. See [`crate::media::picture`].
async fn process_profile_image(
    data: Vec<u8>,
    content_type: String,
    fit: Fit,
) -> AppResult<(Vec<u8>, String)> {
    let processed = crate::tenants::spawn_blocking(move || match Picture::decode(&data) {
        Some(picture) => match picture.original(&data, fit) {
            Some(stored) => {
                let content_type = stored.content_type().to_owned();
                (stored.bytes, content_type)
            }
            None => (data, content_type),
        },
        None => (picture::strip_metadata(&data), content_type),
    })
    .await
    .map_err(|e| anyhow::anyhow!("image processing did not finish: {e}"))?;
    Ok(processed)
}

async fn do_update_credentials(
    state: &AppState,
    auth: &AuthenticatedUser,
    parts: Vec<(String, super::super::extractors::Part)>,
) -> AppResult<Account> {
    let mut display_name: Option<String> = None;
    let mut note: Option<String> = None;
    let mut locked: Option<bool> = None;
    let mut bot: Option<bool> = None;
    let mut discoverable: Option<bool> = None;
    let mut avatar_url: Option<String> = None;
    let mut avatar_content_type: Option<String> = None;
    let mut header_url: Option<String> = None;
    let mut header_content_type: Option<String> = None;
    let mut source_privacy: Option<String> = None;
    let mut source_sensitive: Option<bool> = None;
    let mut source_language: Option<Option<String>> = None;
    let mut source_hide_collections: Option<bool> = None;
    let mut source_quote_policy: Option<String> = None;
    let mut indexable: Option<bool> = None;
    // fields_attributes[N][name] / fields_attributes[N][value]
    let mut fields_map: std::collections::BTreeMap<u32, (String, String)> =
        std::collections::BTreeMap::new();
    let mut fields_submitted = false;
    let mut attribution_domains: Option<Vec<String>> = None;

    for (name, part) in parts {
        // Parse attribution_domains[] array fields
        if name == "attribution_domains[]" {
            let v = part.text();
            attribution_domains.get_or_insert_with(Vec::new).push(v);
            continue;
        }
        // Parse fields_attributes[N][name] and fields_attributes[N][value]
        if let Some(rest) = name.strip_prefix("fields_attributes[") {
            if let Some((idx_str, key)) = rest.split_once(']') {
                if let Ok(idx) = idx_str.parse::<u32>() {
                    let text = part.text();
                    fields_submitted = true;
                    let entry = fields_map.entry(idx).or_default();
                    match key {
                        "[name]" => entry.0 = text,
                        "[value]" => entry.1 = text,
                        _ => {}
                    }
                }
            }
            continue;
        }
        match name.as_str() {
            "display_name" => {
                display_name = Some(part.text());
            }
            "note" => {
                note = Some(part.text());
            }
            "locked" => {
                let v = part.text();
                locked = Some(v == "true" || v == "1");
            }
            "bot" => {
                let v = part.text();
                bot = Some(v == "true" || v == "1");
            }
            "discoverable" => {
                let v = part.text();
                discoverable = Some(v == "true" || v == "1");
            }
            "source[privacy]" => {
                let v = part.text();
                if matches!(v.as_str(), "public" | "unlisted" | "private" | "direct") {
                    source_privacy = Some(v);
                }
            }
            "source[sensitive]" => {
                let v = part.text();
                source_sensitive = Some(v == "true" || v == "1");
            }
            "source[language]" => {
                let v = part.text();
                source_language = Some(if v.is_empty() { None } else { Some(v) });
            }
            "hide_collections" | "source[hide_collections]" => {
                let v = part.text();
                source_hide_collections = Some(v == "true" || v == "1");
            }
            "source[quote_policy]" => {
                let v = part.text();
                if matches!(v.as_str(), "public" | "followers" | "nobody") {
                    source_quote_policy = Some(v);
                }
            }
            "indexable" | "source[indexable]" => {
                let v = part.text();
                indexable = Some(v == "true" || v == "1");
            }
            "avatar" => {
                let (ct, data) = part.file();
                if !data.is_empty() {
                    let (data, ct) = process_profile_image(data.to_vec(), ct, AVATAR_FIT).await?;
                    let key = crate::media::account_avatar_key(auth.account_id, &ct);
                    state.storage.store(&data, &key, &ct).await?;
                    avatar_url = key.rsplit('/').next().map(str::to_string);
                    avatar_content_type = Some(ct);
                }
            }
            "header" => {
                let (ct, data) = part.file();
                if !data.is_empty() {
                    let (data, ct) = process_profile_image(data.to_vec(), ct, HEADER_FIT).await?;
                    let key = crate::media::account_header_key(auth.account_id, &ct);
                    state.storage.store(&data, &key, &ct).await?;
                    header_url = key.rsplit('/').next().map(str::to_string);
                    header_content_type = Some(ct);
                }
            }
            _ => {}
        }
    }

    // Mastodon's `Account#prepare_contents` (a `before_validation` hook that runs
    // `if: :local?`) strips surrounding whitespace from the display name and note,
    // so a stray trailing newline from a client never reaches the database — or
    // the `name`/`summary` we federate out. Strip before validating, matching the
    // callback order: the length limits below apply to the stripped value.
    if let Some(dn) = display_name.as_mut() {
        *dn = dn.trim().to_string();
    }
    if let Some(n) = note.as_mut() {
        *n = n.trim().to_string();
    }

    // Enforce Mastodon's local-account length validations before writing:
    // display_name ≤ 40 chars (Account::DISPLAY_NAME_LENGTH_LIMIT) and note ≤ 500
    // (Account::NOTE_LENGTH_LIMIT, counted via the same URL/mention-aware rule as
    // status length — reuse `countable_length`).
    if let Some(ref dn) = display_name {
        if dn.chars().count() > 40 {
            return Err(AppError::Unprocessable(
                "Validation failed: Display name is too long (maximum is 40 characters)".into(),
            ));
        }
    }
    if let Some(ref n) = note {
        if crate::api::mastodon::formatting::countable_length(n, "") > 500 {
            return Err(AppError::Unprocessable(
                "Validation failed: Note is too long (maximum is 500 characters)".into(),
            ));
        }
    }

    // Persist posting preferences into users.settings (JSON).
    if source_privacy.is_some()
        || source_sensitive.is_some()
        || source_language.is_some()
        || source_quote_policy.is_some()
    {
        let mut settings = user_settings_json(state, auth.account_id).await;
        let obj = settings.as_object_mut().expect("settings json object");
        if let Some(p) = &source_privacy {
            obj.insert("default_privacy".into(), serde_json::json!(p));
        }
        if let Some(s) = source_sensitive {
            obj.insert("default_sensitive".into(), serde_json::json!(s));
        }
        if let Some(l) = &source_language {
            obj.insert(
                "default_language".into(),
                serde_json::to_value(l).unwrap_or(serde_json::Value::Null),
            );
        }
        if let Some(q) = &source_quote_policy {
            obj.insert("default_quote_policy".into(), serde_json::json!(q));
        }
        let s = settings.to_string();
        sqlx::query!(
            "UPDATE users SET settings = $1, updated_at = now() WHERE account_id = $2",
            s,
            auth.account_id,
        )
        .execute(&state.db)
        .await?;
    }

    if let Some(ref dn) = display_name {
        sqlx::query!(
            "UPDATE accounts SET display_name = $1 WHERE id = $2",
            dn,
            auth.account_id
        )
        .execute(&state.db)
        .await?;
    }
    if let Some(ref n) = note {
        // Store the raw bio text, matching Mastodon: the `note` column holds the
        // plain source and the HTML is rendered on the fly at serialize time
        // (see `account_from_db`), keeping `source.note` editable.
        sqlx::query!(
            "UPDATE accounts SET note = $1 WHERE id = $2",
            n,
            auth.account_id
        )
        .execute(&state.db)
        .await?;
    }
    if let Some(l) = locked {
        sqlx::query!(
            "UPDATE accounts SET locked = $1 WHERE id = $2",
            l,
            auth.account_id
        )
        .execute(&state.db)
        .await?;
        // Auto-approve pending follow requests when account becomes unlocked
        if !l {
            // Promote all pending follow requests to accepted follows
            // `authorize_all_follow_requests`: all but those from limited
            // accounts, which stay requests.
            let pending = sqlx::query!(
                r#"DELETE FROM follow_requests fr USING accounts a
                   WHERE fr.target_account_id = $1 AND a.id = fr.account_id AND a.silenced_at IS NULL
                   RETURNING fr.account_id"#,
                auth.account_id,
            )
            .fetch_all(&state.db)
            .await?;
            if !pending.is_empty() {
                // Mirror Mastodon's FollowRequest dependent: :destroy — auto-approving
                // the pending requests removes their follow_request notifications too.
                let approved: Vec<i64> = pending.iter().map(|r| r.account_id).collect();
                sqlx::query!(
                    r#"DELETE FROM notifications WHERE account_id = $1 AND type = 'follow_request'
                         AND from_account_id = ANY($2)"#,
                    auth.account_id,
                    &approved,
                )
                .execute(&state.db)
                .await?;
            }
            for row in &pending {
                let _ = sqlx::query!(
                    r#"INSERT INTO follows (account_id, target_account_id, created_at, updated_at)
                       VALUES ($1, $2, now(), now()) ON CONFLICT DO NOTHING"#,
                    row.account_id,
                    auth.account_id
                )
                .execute(&state.db)
                .await;
                // `AccountStat`'s `update_index('accounts', :account)`.
                crate::search::elasticsearch::indexing::accounts(
                    state,
                    &[row.account_id, auth.account_id],
                )
                .await;
                let _ =
                    crate::counters::on_follow_created(&state.db, row.account_id, auth.account_id)
                        .await;
                crate::push::create_and_push(
                    state,
                    auth.account_id,
                    row.account_id,
                    "follow",
                    None,
                    "New follower".into(),
                    "".into(),
                    "".into(),
                )
                .await;
            }
        }
    }
    if let Some(b) = bot {
        let actor_type = if b { "Service" } else { "Person" };
        sqlx::query!(
            "UPDATE accounts SET actor_type = $1 WHERE id = $2",
            actor_type,
            auth.account_id
        )
        .execute(&state.db)
        .await?;
    }
    // Whether this update changed `discoverable`, which providers are told
    // of even when it turned it off (`saved_change_to_discoverable?`).
    let mut discoverable_changed = false;
    if let Some(d) = discoverable {
        discoverable_changed = sqlx::query!(
            "UPDATE accounts SET discoverable = $1 WHERE id = $2 AND discoverable IS DISTINCT FROM $1",
            d,
            auth.account_id
        )
        .execute(&state.db)
        .await?
        .rows_affected()
            > 0;
    }
    if let Some(ix) = indexable {
        let changed = sqlx::query(
            "UPDATE accounts SET indexable = $1 WHERE id = $2 AND indexable IS DISTINCT FROM $1",
        )
        .bind(ix)
        .bind(auth.account_id)
        .execute(&state.db)
        .await?
        .rows_affected()
            > 0;
        // `after_update_commit :enqueue_update_public_statuses_index, if:
        // :saved_change_to_indexable?`.
        if changed {
            let state = state.clone();
            let account_id = auth.account_id;
            crate::tenants::spawn(async move {
                crate::search::elasticsearch::indexing::account_indexable_changed(
                    &state, account_id,
                )
                .await;
            });
        }
    }
    if let Some(ref filename) = avatar_url {
        sqlx::query!(
            "UPDATE accounts SET avatar_file_name = $1, avatar_content_type = $2, avatar_updated_at = now() WHERE id = $3",
            filename, avatar_content_type, auth.account_id
        )
        .execute(&state.db).await?;
    }
    if let Some(ref filename) = header_url {
        sqlx::query!(
            "UPDATE accounts SET header_file_name = $1, header_content_type = $2, header_updated_at = now() WHERE id = $3",
            filename, header_content_type, auth.account_id
        )
        .execute(&state.db).await?;
    }

    // Collect non-empty fields and save as JSONB
    if fields_submitted {
        // Drop fully-blank entries, then enforce Mastodon's limits: at most 4
        // fields (Account::DEFAULT_FIELDS_SIZE), each name/value <= 255 chars
        // (Account::Field::MAX_CHARACTERS_LOCAL).
        let fields: Vec<(String, String)> = fields_map
            .into_values()
            .filter(|(n, v)| !(n.is_empty() && v.is_empty()))
            .collect();
        if fields.len() > 4 {
            return Err(AppError::Unprocessable(
                "Validation failed: Fields can't have more than 4 entries".into(),
            ));
        }
        for (n, v) in &fields {
            if n.chars().count() > 255 || v.chars().count() > 255 {
                return Err(AppError::Unprocessable(
                    "Validation failed: Field name and value can't be longer than 255 characters"
                        .into(),
                ));
            }
        }
        // Preserve an existing `verified_at` when a field's value is unchanged,
        // mirroring Mastodon's `Account#fields_attributes=`; a changed value
        // clears the badge and re-verification is enqueued below.
        let old_fields: Vec<serde_json::Value> =
            sqlx::query_scalar!("SELECT fields FROM accounts WHERE id = $1", auth.account_id,)
                .fetch_one(&state.db)
                .await?
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();
        let fields_json: serde_json::Value = fields
            .into_iter()
            .filter(|(n, _)| !n.is_empty())
            .map(|(n, v)| {
                let verified_at = old_fields
                    .iter()
                    .find(|of| of.get("value").and_then(|ov| ov.as_str()) == Some(v.as_str()))
                    .and_then(|of| of.get("verified_at").cloned())
                    .filter(|va| !va.is_null())
                    .unwrap_or(serde_json::Value::Null);
                serde_json::json!({"name": n, "value": v, "verified_at": verified_at})
            })
            .collect();
        sqlx::query!(
            "UPDATE accounts SET fields = $1 WHERE id = $2",
            fields_json,
            auth.account_id
        )
        .execute(&state.db)
        .await?;
    }

    // default_privacy, default_sensitive, default_language are stored in users.settings (YAML)
    // in Mastodon's schema; we don't persist them here.
    let _ = (&source_privacy, source_sensitive, &source_language);
    if let Some(hc) = source_hide_collections {
        sqlx::query!(
            "UPDATE accounts SET hide_collections = $1 WHERE id = $2",
            hc,
            auth.account_id
        )
        .execute(&state.db)
        .await?;
    }
    let mut attribution_domains_changed = false;
    if let Some(domains) = attribution_domains {
        // `normalizes :attribution_domains`: stripped of a scheme and of
        // `*.`, blanks dropped, each once.
        let mut normalized: Vec<String> = Vec::new();
        for domain in domains {
            let domain = domain.trim();
            let domain = domain.strip_prefix("http://").unwrap_or(domain);
            let domain = domain.strip_prefix("https://").unwrap_or(domain);
            let domain = domain.strip_prefix("*.").unwrap_or(domain);
            if !domain.is_empty() && !normalized.iter().any(|d| d == domain) {
                normalized.push(domain.to_owned());
            }
        }
        attribution_domains_changed = sqlx::query!(
            r#"UPDATE accounts SET attribution_domains = $1
               WHERE id = $2 AND attribution_domains IS DISTINCT FROM $1::varchar[]"#,
            &normalized,
            auth.account_id
        )
        .execute(&state.db)
        .await?
        .rows_affected()
            > 0;
    }
    // default_quote_policy is in users.settings (YAML) in Mastodon's schema; not persisted here.
    let _ = &source_quote_policy;

    sqlx::query!(
        "UPDATE accounts SET updated_at = now() WHERE id = $1",
        auth.account_id
    )
    .execute(&state.db)
    .await?;
    // `Account`'s `update_index('accounts', :self)`.
    crate::search::elasticsearch::indexing::account(state, auth.account_id).await;

    let account = fetch_account(state, auth.account_id).await?;
    // `UpdateAccountService`'s `account.update`: its `after_update_commit`s.
    crate::moderation::webhooks::account_updated(state, auth.account_id).await;
    // `UpdateAccountService#process_hashtags` and
    // `#process_attribution_domains`.
    process_hashtags(state, &account).await?;
    if attribution_domains_changed {
        crate::preview_card::attribution::attribution_domains_changed(state, auth.account_id)
            .await?;
    }
    crate::fasp::events::account_updated(state, auth.account_id, discoverable_changed).await;
    Ok(account)
}

/// `UpdateAccountService#process_hashtags`: the account's profile hashtags
/// (`accounts_tags`) become those its bio has (`Account#tags_as_strings=`),
/// which its actor names in `tag`.
pub(crate) async fn process_hashtags(state: &AppState, account: &Account) -> AppResult<()> {
    let mut tag_ids: Vec<i64> = Vec::new();
    for name in crate::api::mastodon::statuses::extract_hashtags(&account.note) {
        if let Some(id) = crate::tags::find_or_create(&state.db, &name).await? {
            if !tag_ids.contains(&id) {
                tag_ids.push(id);
            }
        }
    }
    sqlx::query!(
        "DELETE FROM accounts_tags WHERE account_id = $1 AND NOT (tag_id = ANY($2))",
        account.id,
        &tag_ids,
    )
    .execute(&state.db)
    .await?;
    sqlx::query!(
        r#"INSERT INTO accounts_tags (account_id, tag_id)
           SELECT $1, t FROM unnest($2::bigint[]) AS t
           WHERE NOT EXISTS (SELECT 1 FROM accounts_tags WHERE account_id = $1 AND tag_id = t)"#,
        account.id,
        &tag_ids,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

async fn distribute_account_update(state: &AppState, domain: &str, account: &Account) {
    if let Err(e) = crate::accounts::distribute_profile(state, domain, account, None).await {
        tracing::warn!(error = %e, "failed to enqueue account Update fanout");
    }
}

pub async fn update_credentials(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Extension(crate::middleware::ResolvedInstance(instance)): Extension<
        crate::middleware::ResolvedInstance,
    >,
    super::super::extractors::Parts(parts): super::super::extractors::Parts,
) -> AppResult<Json<ApiAccount>> {
    auth.require_scope("write:accounts")?;
    let account = do_update_credentials(&state, &auth, parts).await?;
    distribute_account_update(&state, &instance.domain, &account).await;
    crate::link_verification::verify(&state, auth.account_id).await;
    build_credential_account_response(&state, &auth, account).await
}

// ── PATCH /api/v1/profile (profile-specific update) ──────────────────────

pub async fn patch_profile(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Extension(crate::middleware::ResolvedInstance(instance)): Extension<
        crate::middleware::ResolvedInstance,
    >,
    super::super::extractors::Parts(parts): super::super::extractors::Parts,
) -> AppResult<Json<crate::api::mastodon::types::Profile>> {
    auth.require_scope("write:accounts")?;
    let account = do_update_credentials(&state, &auth, parts).await?;
    distribute_account_update(&state, &instance.domain, &account).await;
    crate::link_verification::verify(&state, auth.account_id).await;

    let domain = &instance.domain;
    let featured_tag_rows = sqlx::query!(
        r#"SELECT ft.id, t.name, ft.statuses_count, ft.last_status_at,
                  -- `FeaturedTag#display_name`
                  COALESCE(ft.name, t.display_name, t.name) AS "display_name!"
           FROM featured_tags ft
           JOIN tags t ON t.id = ft.tag_id
           WHERE ft.account_id = $1
           ORDER BY ft.id"#,
        account.id,
    )
    .fetch_all(&state.db)
    .await?;

    let featured_tags = featured_tag_rows
        .into_iter()
        .map(|r| crate::api::mastodon::types::FeaturedTag {
            id: r.id.to_string(),
            name: r.display_name,
            url: format!("https://{}/@{}/tagged/{}", domain, account.username, r.name),
            statuses_count: r.statuses_count.to_string(),
            last_status_at: r.last_status_at.map(|t| t.format("%Y-%m-%d").to_string()),
        })
        .collect();

    let a = &account;
    let fields = crate::api::mastodon::convert::fields_from_db(
        a.fields.as_ref().unwrap_or(&serde_json::json!([])),
    );
    // `ProfileSerializer`: `account_bio_format` and `account_field_value_format`.
    let mut texts: Vec<&str> = vec![&a.note];
    texts.extend(fields.iter().map(|f| f.value.as_str()));
    let lookup = crate::api::mastodon::formatting::mention_lookup(&state, &texts).await;
    let formatted_fields =
        crate::api::mastodon::formatting::field_values(domain, fields.clone(), true, &lookup);
    Ok(Json(crate::api::mastodon::types::Profile {
        id: a.id.to_string(),
        username: a.username.clone(),
        display_name: a.display_name.clone(),
        note: a.note.clone(),
        fields,
        formatted_note: crate::formatter::local_bio(&a.note, domain, &lookup),
        formatted_fields,
        avatar: Some(crate::api::mastodon::convert::account_avatar_url_for(
            &state.urls,
            a,
        )),
        avatar_static: Some(crate::api::mastodon::convert::account_avatar_url_for(
            &state.urls,
            a,
        )),
        header: Some(crate::api::mastodon::convert::account_header_url_for(
            &state.urls,
            a,
        )),
        header_static: Some(crate::api::mastodon::convert::account_header_url_for(
            &state.urls,
            a,
        )),
        locked: a.locked,
        bot: a.actor_type.as_deref() == Some("Service"),
        hide_collections: a.hide_collections,
        discoverable: a.discoverable,
        indexable: a.indexable,
        attribution_domains: a.attribution_domains.clone().unwrap_or_default(),
        featured_tags,
    }))
}

async fn build_credential_account_response(
    state: &AppState,
    auth: &AuthenticatedUser,
    account: Account,
) -> AppResult<Json<ApiAccount>> {
    let fields = crate::api::mastodon::convert::fields_from_db(
        account.fields.as_ref().unwrap_or(&serde_json::json!([])),
    );
    let mut api_account = account_from_db(&state.urls, &account);
    api_account.emojis = fetch_account_emojis(state, &account).await;
    apply_account_stats(state, &mut api_account, account.id).await;
    let follow_requests_count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM (SELECT 1 FROM follow_requests WHERE target_account_id = $1 LIMIT 40) sub",
        auth.account_id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    // Reflect the user's actual stored posting defaults (Mastodon's
    // CredentialAccountSerializer#source reads the user's settings), not
    // hardcoded values.
    let defaults = user_defaults(state, auth.account_id).await;

    api_account.source = Some(crate::api::mastodon::types::AccountSource {
        privacy: defaults.privacy,
        sensitive: defaults.sensitive,
        language: defaults.language,
        note: account.note.clone(),
        fields: fields.clone(),
        follow_requests_count,
        discoverable: account.discoverable,
        indexable: account.indexable,
        hide_collections: account.hide_collections,
        attribution_domains: account.attribution_domains.clone().unwrap_or_default(),
        quote_policy: defaults.quote_policy,
    });
    api_account.roles = fetch_account_roles(state, auth.account_id).await;
    api_account.role = fetch_account_role(state, auth.account_id).await;
    api_account.email_subscriptions = crate::email_subscriptions::serialized(state, &account).await;
    Ok(Json(api_account))
}

// ── GET /api/v1/preferences ───────────────────────────────────────────────

pub async fn get_preferences(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Preferences>> {
    auth.require_scope("read:accounts")?;
    let d = user_defaults(&state, auth.account_id).await;
    let (privacy, sensitive, language, quote_policy) =
        (d.privacy, d.sensitive, d.language, d.quote_policy);

    Ok(Json(Preferences {
        posting_default_visibility: privacy,
        posting_default_sensitive: sensitive,
        posting_default_language: language,
        posting_default_quote_policy: quote_policy,
        reading_expand_media: "default".into(),
        reading_expand_spoilers: false,
        reading_autoplay_gifs: false,
    }))
}

// ── GET /api/v1/profile ───────────────────────────────────────────────────

pub async fn get_profile(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Extension(crate::middleware::ResolvedInstance(instance)): Extension<
        crate::middleware::ResolvedInstance,
    >,
) -> AppResult<Json<crate::api::mastodon::types::Profile>> {
    auth.require_scope("read:accounts")?;
    Ok(Json(
        build_profile(&state, &instance.domain, auth.account_id).await?,
    ))
}

/// PUT /api/v1/profile — accepts a JSON body and returns the current profile.
/// (Profile field edits go through update_credentials / the multipart PATCH.)
pub async fn put_profile(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Extension(crate::middleware::ResolvedInstance(instance)): Extension<
        crate::middleware::ResolvedInstance,
    >,
    _body: Option<Json<serde_json::Value>>,
) -> AppResult<Json<crate::api::mastodon::types::Profile>> {
    auth.require_scope("write:accounts")?;
    Ok(Json(
        build_profile(&state, &instance.domain, auth.account_id).await?,
    ))
}

async fn build_profile(
    state: &AppState,
    domain: &str,
    account_id: i64,
) -> AppResult<crate::api::mastodon::types::Profile> {
    let account = sqlx::query_as!(Account, "SELECT * FROM accounts WHERE id = $1", account_id,)
        .fetch_one(&state.db)
        .await?;

    let domain = &domain.to_string();
    let featured_tag_rows = sqlx::query!(
        r#"SELECT ft.id, t.name, ft.statuses_count, ft.last_status_at,
                  -- `FeaturedTag#display_name`
                  COALESCE(ft.name, t.display_name, t.name) AS "display_name!"
           FROM featured_tags ft
           JOIN tags t ON t.id = ft.tag_id
           WHERE ft.account_id = $1
           ORDER BY ft.id"#,
        account.id,
    )
    .fetch_all(&state.db)
    .await?;

    let featured_tags = featured_tag_rows
        .into_iter()
        .map(|r| crate::api::mastodon::types::FeaturedTag {
            id: r.id.to_string(),
            name: r.display_name,
            url: format!("https://{}/@{}/tagged/{}", domain, account.username, r.name),
            statuses_count: r.statuses_count.to_string(),
            last_status_at: r.last_status_at.map(|t| t.format("%Y-%m-%d").to_string()),
        })
        .collect();

    let a = &account;
    let fields = crate::api::mastodon::convert::fields_from_db(
        a.fields.as_ref().unwrap_or(&serde_json::json!([])),
    );
    // `ProfileSerializer`: `account_bio_format` and `account_field_value_format`.
    let mut texts: Vec<&str> = vec![&a.note];
    texts.extend(fields.iter().map(|f| f.value.as_str()));
    let lookup = crate::api::mastodon::formatting::mention_lookup(state, &texts).await;
    let formatted_fields =
        crate::api::mastodon::formatting::field_values(domain, fields.clone(), true, &lookup);
    let profile = crate::api::mastodon::types::Profile {
        id: a.id.to_string(),
        username: a.username.clone(),
        display_name: a.display_name.clone(),
        note: a.note.clone(),
        fields,
        formatted_note: crate::formatter::local_bio(&a.note, domain, &lookup),
        formatted_fields,
        avatar: Some(crate::api::mastodon::convert::account_avatar_url_for(
            &state.urls,
            a,
        )),
        avatar_static: Some(crate::api::mastodon::convert::account_avatar_url_for(
            &state.urls,
            a,
        )),
        header: Some(crate::api::mastodon::convert::account_header_url_for(
            &state.urls,
            a,
        )),
        header_static: Some(crate::api::mastodon::convert::account_header_url_for(
            &state.urls,
            a,
        )),
        locked: a.locked,
        bot: a.actor_type.as_deref() == Some("Service"),
        hide_collections: a.hide_collections,
        discoverable: a.discoverable,
        indexable: a.indexable,
        attribution_domains: a.attribution_domains.clone().unwrap_or_default(),
        featured_tags,
    };
    Ok(profile)
}

// ── DELETE /api/v1/profile/avatar ────────────────────────────────────────

pub async fn delete_profile_avatar(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
) -> AppResult<Json<crate::api::mastodon::types::Account>> {
    auth.require_scope("write:accounts")?;
    sqlx::query!(
        "UPDATE accounts SET avatar_file_name = NULL, avatar_content_type = NULL, avatar_file_size = NULL, avatar_updated_at = NULL, updated_at = now() WHERE id = $1",
        auth.account_id,
    )
    .execute(&state.db)
    .await?;
    // `UpdateAccountService` with `avatar: nil`.
    crate::moderation::webhooks::account_updated(&state, auth.account_id).await;
    let account = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        auth.account_id,
    )
    .fetch_one(&state.db)
    .await?;
    distribute_account_update(&state, &instance.domain, &account).await;
    let mut api = account_from_db(&state.urls, &account);
    api.emojis = fetch_account_emojis(&state, &account).await;
    api.roles = fetch_account_roles(&state, account.id).await;
    Ok(Json(api))
}

// ── DELETE /api/v1/profile/header ────────────────────────────────────────

pub async fn delete_profile_header(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
) -> AppResult<Json<crate::api::mastodon::types::Account>> {
    auth.require_scope("write:accounts")?;
    sqlx::query!(
        "UPDATE accounts SET header_file_name = NULL, header_content_type = NULL, header_file_size = NULL, header_updated_at = NULL, updated_at = now() WHERE id = $1",
        auth.account_id,
    )
    .execute(&state.db)
    .await?;
    // `UpdateAccountService` with `header: nil`.
    crate::moderation::webhooks::account_updated(&state, auth.account_id).await;
    let account = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        auth.account_id,
    )
    .fetch_one(&state.db)
    .await?;
    distribute_account_update(&state, &instance.domain, &account).await;
    let mut api = account_from_db(&state.urls, &account);
    api.emojis = fetch_account_emojis(&state, &account).await;
    api.roles = fetch_account_roles(&state, account.id).await;
    Ok(Json(api))
}
