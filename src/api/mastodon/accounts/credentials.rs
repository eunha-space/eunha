//! The authenticated account's own credentials and profile: verify,
//! update_credentials (avatar/header/fields), the /profile endpoints, and
//! propagating profile Updates to the fediverse.

use super::*;
use crate::email_subscriptions::ValidationErrors;
use crate::media::profile;

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
        r#"SELECT COUNT(*) FROM (
             SELECT 1 FROM follow_requests fr
             JOIN accounts a ON a.id = fr.account_id
             WHERE fr.target_account_id = $1
               AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
             LIMIT 40
           ) sub"#,
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
            true,
        ),
        follow_requests_count: follow_requests,
        discoverable: account.discoverable,
        indexable: account.indexable,
        hide_collections: account.hide_collections,
        attribution_domains: account.attribution_domains.clone().unwrap_or_default(),
        quote_policy: default_quote_policy,
    });

    api_account.set_roles(fetch_account_roles(&state, account.id).await);
    api_account.role = fetch_account_role(&state, account.id).await;
    api_account.email_subscriptions =
        crate::email_subscriptions::serialized(&state, &account).await;

    Ok(Json(api_account))
}

// ── PATCH /api/v1/accounts/update_credentials ─────────────────────────────

/// Validate, process and store an avatar or header, as assigning it to
/// `Account` does: the validations first, then `lazy_thumbnail`, whose
/// failure to read the file is another validation error, recorded in
/// `errors`. Returns the stored file's name, content type and size.
async fn store_profile_image(
    state: &AppState,
    account_id: i64,
    kind: profile::Kind,
    (content_type, data): (String, Vec<u8>),
    errors: &mut ValidationErrors,
) -> AppResult<Option<(String, &'static str, i32)>> {
    let mut own = ValidationErrors::default();
    profile::validate(kind, &content_type, data.len(), &mut own);
    if !own.is_empty() {
        errors.extend(own);
        return Ok(None);
    }
    let Some(stored) = profile::process(kind, data).await? else {
        profile::not_identified(kind, errors);
        return Ok(None);
    };
    let key = match kind {
        profile::Kind::Avatar => crate::media::account_avatar_key(account_id, stored.content_type),
        profile::Kind::Header => crate::media::account_header_key(account_id, stored.content_type),
    };
    state
        .storage
        .store(&stored.bytes, &key, stored.content_type)
        .await?;
    if let Some(png) = &stored.static_png {
        state
            .storage
            .store(png, &profile::Kind::static_key(&key), "image/png")
            .await?;
    }
    let file_name = key.rsplit('/').next().unwrap_or_default().to_owned();
    Ok(Some((
        file_name,
        stored.content_type,
        stored.bytes.len() as i32,
    )))
}

/// Which of Mastodon's two account updates a request is, for the parameters
/// each permits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UpdateEndpoint {
    /// `PATCH /api/v1/accounts/update_credentials`.
    Credentials,
    /// `PATCH` or `PUT /api/v1/profile`, which also takes the profile tab's
    /// settings.
    Profile,
}

async fn do_update_credentials(
    state: &AppState,
    auth: &AuthenticatedUser,
    parts: Vec<(String, super::super::extractors::Part)>,
    endpoint: UpdateEndpoint,
) -> AppResult<Account> {
    let mut display_name: Option<String> = None;
    let mut note: Option<String> = None;
    // A boolean given blank is nil, as `ActiveModel::Type::Boolean` casts
    // it: `Some(None)` here. `locked` and `indexable` are `null: false`, so
    // nil is the `NotNullViolation` `update!` raises; `discoverable` and
    // `hide_collections` are nullable and take it; `bot=` casts it to false.
    let mut locked: Option<Option<bool>> = None;
    let mut bot: Option<bool> = None;
    let mut discoverable: Option<Option<bool>> = None;
    let mut avatar_upload: Option<(String, Vec<u8>)> = None;
    let mut header_upload: Option<(String, Vec<u8>)> = None;
    let mut avatar_description: Option<String> = None;
    let mut header_description: Option<String> = None;
    let mut source_privacy: Option<String> = None;
    // Blank is nil, which `UserSettings#[]=` deletes, leaving the default.
    let mut source_sensitive: Option<Option<bool>> = None;
    let mut source_language: Option<Option<String>> = None;
    let mut source_hide_collections: Option<Option<bool>> = None;
    let mut source_quote_policy: Option<String> = None;
    let mut indexable: Option<Option<bool>> = None;
    // A `source[privacy]` or `source[quote_policy]` that `UserSettings#[]=`
    // refuses, with its message.
    let mut invalid_setting: Option<String> = None;
    // fields_attributes[N][name] / fields_attributes[N][value]
    let mut fields_map: std::collections::BTreeMap<u32, (String, String)> =
        std::collections::BTreeMap::new();
    let mut fields_submitted = false;
    let mut attribution_domains: Option<Vec<String>> = None;
    // The profile tab's settings, which `ProfilesController` permits and
    // `CredentialsController` does not. `null: false`, as `locked` is.
    let mut tab_settings: Vec<(&'static str, Option<bool>)> = vec![];

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
                locked = Some(cast_bool(&part.text()));
            }
            "bot" => {
                bot = Some(cast_bool(&part.text()).unwrap_or(false));
            }
            "discoverable" => {
                discoverable = Some(cast_bool(&part.text()));
            }
            "source[privacy]" => {
                // `setting :default_privacy, in: %w(public unlisted private)`.
                let v = part.text();
                if matches!(v.as_str(), "public" | "unlisted" | "private") {
                    source_privacy = Some(v);
                } else if invalid_setting.is_none() {
                    invalid_setting =
                        Some(format!("Invalid value for setting default_privacy: {v}"));
                }
            }
            "source[sensitive]" => {
                source_sensitive = Some(cast_bool(&part.text()));
            }
            "source[language]" => {
                let v = part.text();
                source_language = Some(if v.is_empty() { None } else { Some(v) });
            }
            "hide_collections" | "source[hide_collections]" => {
                source_hide_collections = Some(cast_bool(&part.text()));
            }
            "source[quote_policy]" => {
                let v = part.text();
                if matches!(v.as_str(), "public" | "followers" | "nobody") {
                    source_quote_policy = Some(v);
                } else if invalid_setting.is_none() {
                    invalid_setting = Some(format!(
                        "Invalid value for setting default_quote_policy: {v}"
                    ));
                }
            }
            "indexable" | "source[indexable]" => {
                indexable = Some(cast_bool(&part.text()));
            }
            "avatar" => {
                let (ct, data) = part.file();
                if !data.is_empty() {
                    avatar_upload = Some((ct, data));
                }
            }
            "header" => {
                let (ct, data) = part.file();
                if !data.is_empty() {
                    header_upload = Some((ct, data));
                }
            }
            "avatar_description" => avatar_description = Some(part.text()),
            "header_description" => header_description = Some(part.text()),
            "show_media" | "show_media_replies" | "show_featured"
                if endpoint == UpdateEndpoint::Profile =>
            {
                let column = match name.as_str() {
                    "show_media" => "show_media",
                    "show_media_replies" => "show_media_replies",
                    _ => "show_featured",
                };
                tab_settings.retain(|(c, _)| *c != column);
                tab_settings.push((column, cast_bool(&part.text())));
            }
            _ => {}
        }
    }

    // `Account::Avatar` and `Account::Header`: each image validated, and
    // processed when it is valid; their descriptions' lengths. Nothing is
    // saved when any of them fails.
    let mut image_errors = ValidationErrors::default();
    let mut avatar = None;
    let mut header = None;
    if let Some(upload) = avatar_upload {
        avatar = store_profile_image(
            state,
            auth.account_id,
            profile::Kind::Avatar,
            upload,
            &mut image_errors,
        )
        .await?;
    }
    if let Some(upload) = header_upload {
        header = store_profile_image(
            state,
            auth.account_id,
            profile::Kind::Header,
            upload,
            &mut image_errors,
        )
        .await?;
    }
    if let Some(description) = &avatar_description {
        profile::validate_description(profile::Kind::Avatar, description, &mut image_errors);
    }
    if let Some(description) = &header_description {
        profile::validate_description(profile::Kind::Header, description, &mut image_errors);
    }
    if !image_errors.is_empty() {
        return Err(AppError::Unprocessable(image_errors.message()));
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

    // `Account#fields_attributes=`: the fields given, but those with neither
    // a name nor a value, as they were written.
    let fields: Option<Vec<(String, String)>> = fields_submitted.then(|| {
        fields_map
            .into_values()
            .filter(|(n, v)| !(n.trim().is_empty() && v.trim().is_empty()))
            .collect()
    });

    // The account's validations, all run before anything is written, as
    // `update!` runs them: display_name up to 40 characters
    // (`DISPLAY_NAME_LENGTH_LIMIT`), note up to 500 as statuses count them
    // (`NOTE_LENGTH_LIMIT`), at most 4 fields (`DEFAULT_FIELDS_SIZE`, whose
    // length message speaks of characters), and none with a value but no
    // name (`EmptyProfileFieldNamesValidator`). A long field name or value
    // is no error: `Account::Field` truncates it when it is read.
    let mut errors = crate::email_subscriptions::ValidationErrors::default();
    if display_name
        .as_ref()
        .is_some_and(|dn| dn.chars().count() > 40)
    {
        errors.add(
            "display_name",
            "too_long",
            "is too long (maximum is 40 characters)",
        );
    }
    if note
        .as_ref()
        .is_some_and(|n| crate::api::mastodon::formatting::countable_length(n, "") > 500)
    {
        errors.add(
            "note",
            "too_long",
            "is too long (maximum is 500 characters)",
        );
    }
    if let Some(fields) = &fields {
        if fields.len() > 4 {
            errors.add(
                "fields",
                "too_long",
                "is too long (maximum is 4 characters)",
            );
        }
        if fields.iter().any(|(n, v)| {
            crate::api::mastodon::convert::sanitize_field(n, true).is_empty()
                && !crate::api::mastodon::convert::sanitize_field(v, true).is_empty()
        }) {
            errors.add(
                "fields",
                "fields_with_values_missing_labels",
                "contains values with missing labels",
            );
        }
    }
    if !errors.is_empty() {
        return Err(errors.into());
    }
    // A blank `locked` or `indexable` passes the validations and is written
    // as NULL, which the column refuses: `ActiveRecord::NotNullViolation`,
    // which nothing rescues. The transaction leaves the account as it was,
    // and the settings, saved after it, are not reached.
    let nullable = [("locked", locked), ("indexable", indexable)]
        .into_iter()
        .chain(
            tab_settings
                .iter()
                .map(|&(column, value)| (column, Some(value))),
        );
    for (column, value) in nullable {
        if value == Some(None) {
            return Err(AppError::Unrescued(format!(
                "PG::NotNullViolation: null value in column \"{column}\" of relation \"accounts\""
            )));
        }
    }

    // Persist posting preferences into users.settings (JSON), unless one is
    // refused, which `current_user.update(user_params)` raises on.
    if invalid_setting.is_none()
        && (source_privacy.is_some()
            || source_sensitive.is_some()
            || source_language.is_some()
            || source_quote_policy.is_some())
    {
        let mut settings = user_settings_json(state, auth.account_id).await;
        let obj = settings.as_object_mut().expect("settings json object");
        if let Some(p) = &source_privacy {
            obj.insert("default_privacy".into(), serde_json::json!(p));
        }
        match source_sensitive {
            Some(Some(s)) => {
                obj.insert("default_sensitive".into(), serde_json::json!(s));
            }
            Some(None) => {
                obj.remove("default_sensitive");
            }
            None => {}
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
    if let Some(Some(l)) = locked {
        let was_locked =
            sqlx::query_scalar!("SELECT locked FROM accounts WHERE id = $1", auth.account_id)
                .fetch_one(&state.db)
                .await?;
        sqlx::query!(
            "UPDATE accounts SET locked = $1 WHERE id = $2",
            l,
            auth.account_id
        )
        .execute(&state.db)
        .await?;
        // `authorize_all_follow_requests(account) if was_locked &&
        // !account.locked`: an `AuthorizeFollowWorker` for each request, but
        // those from limited accounts, which stay requests.
        if was_locked && !l {
            let requests = sqlx::query!(
                r#"SELECT fr.account_id FROM follow_requests fr
                   JOIN accounts a ON a.id = fr.account_id
                   WHERE fr.target_account_id = $1 AND a.silenced_at IS NULL"#,
                auth.account_id,
            )
            .fetch_all(&state.db)
            .await?;
            crate::jobs::perform_bulk(
                state,
                requests.into_iter().map(|r| super::AuthorizeFollowWorker {
                    source_account_id: r.account_id,
                    target_account_id: auth.account_id,
                }),
            )
            .await
            .map_err(AppError::Internal)?;
        }
    }
    for (column, value) in &tab_settings {
        if let Some(value) = value {
            // The column is one of three fixed names, never the request's.
            sqlx::query(&format!("UPDATE accounts SET {column} = $1 WHERE id = $2"))
                .bind(value)
                .bind(auth.account_id)
                .execute(&state.db)
                .await?;
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
    if let Some(Some(ix)) = indexable {
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
    if let Some((filename, content_type, size)) = avatar {
        sqlx::query!(
            "UPDATE accounts SET avatar_file_name = $1, avatar_content_type = $2,
                    avatar_file_size = $3, avatar_storage_schema_version = 1,
                    avatar_updated_at = now()
             WHERE id = $4",
            filename,
            content_type,
            size,
            auth.account_id
        )
        .execute(&state.db)
        .await?;
    }
    if let Some((filename, content_type, size)) = header {
        sqlx::query!(
            "UPDATE accounts SET header_file_name = $1, header_content_type = $2,
                    header_file_size = $3, header_storage_schema_version = 1,
                    header_updated_at = now()
             WHERE id = $4",
            filename,
            content_type,
            size,
            auth.account_id
        )
        .execute(&state.db)
        .await?;
    }
    if let Some(description) = &avatar_description {
        sqlx::query!(
            "UPDATE accounts SET avatar_description = $1 WHERE id = $2",
            description,
            auth.account_id
        )
        .execute(&state.db)
        .await?;
    }
    if let Some(description) = &header_description {
        sqlx::query!(
            "UPDATE accounts SET header_description = $1 WHERE id = $2",
            description,
            auth.account_id
        )
        .execute(&state.db)
        .await?;
    }

    // `self[:fields] = fields`: each as given, keeping an existing
    // `verified_at` while the value is the same.
    if let Some(fields) = fields {
        let old_fields: Vec<serde_json::Value> =
            sqlx::query_scalar!("SELECT fields FROM accounts WHERE id = $1", auth.account_id,)
                .fetch_one(&state.db)
                .await?
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();
        let fields_json: serde_json::Value = fields
            .into_iter()
            .map(|(n, v)| {
                let mut field = serde_json::json!({"name": n, "value": v});
                if let Some(verified_at) = old_fields
                    .iter()
                    .find(|of| of.get("value").and_then(|ov| ov.as_str()) == Some(v.as_str()))
                    .and_then(|of| of.get("verified_at"))
                    .filter(|va| va.as_str().is_some_and(|va| !va.is_empty()))
                {
                    field["verified_at"] = verified_at.clone();
                }
                field
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
    if let Some(message) = invalid_setting {
        // `UpdateAccountService` has saved the account and queued its link
        // checks; the settings then raise an `ArgumentError` nothing
        // rescues, before the update is distributed.
        crate::link_verification::verify(state, auth.account_id).await;
        return Err(AppError::Unrescued(message));
    }
    Ok(account)
}

/// `ActiveModel::Type::Boolean#cast` of a form value; blank is nil.
fn cast_bool(value: &str) -> Option<bool> {
    crate::api::mastodon::extractors::rails::cast_bool(&serde_json::Value::String(value.to_owned()))
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
    let account = do_update_credentials(&state, &auth, parts, UpdateEndpoint::Credentials).await?;
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
    let account = do_update_credentials(&state, &auth, parts, UpdateEndpoint::Profile).await?;
    distribute_account_update(&state, &instance.domain, &account).await;
    crate::link_verification::verify(&state, auth.account_id).await;

    Ok(Json(
        build_profile(&state, &instance.domain, account.id).await?,
    ))
}

async fn build_credential_account_response(
    state: &AppState,
    auth: &AuthenticatedUser,
    account: Account,
) -> AppResult<Json<ApiAccount>> {
    let fields = crate::api::mastodon::convert::fields_from_db(
        account.fields.as_ref().unwrap_or(&serde_json::json!([])),
        true,
    );
    let mut api_account = account_from_db(&state.urls, &account);
    api_account.emojis = fetch_account_emojis(state, &account).await;
    apply_account_stats(state, &mut api_account, account.id).await;
    let follow_requests_count: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) FROM (
             SELECT 1 FROM follow_requests fr
             JOIN accounts a ON a.id = fr.account_id
             WHERE fr.target_account_id = $1
               AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
             LIMIT 40
           ) sub"#,
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
    api_account.set_roles(fetch_account_roles(state, auth.account_id).await);
    api_account.role = fetch_account_role(state, auth.account_id).await;
    api_account.email_subscriptions = crate::email_subscriptions::serialized(state, &account).await;
    Ok(Json(api_account))
}

// ── GET /api/v1/preferences ───────────────────────────────────────────────

/// `Localized`'s `params[:lang]`.
#[derive(Debug, Deserialize, Default)]
pub struct LangParam {
    #[serde(default)]
    lang: Option<String>,
}

/// `REST::PreferencesSerializer`.
pub async fn get_preferences(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Query(params): Query<LangParam>,
) -> AppResult<Json<Preferences>> {
    auth.require_scope("read:accounts")?;
    let d = user_defaults(&state, auth.account_id).await;
    let settings = user_settings_json(&state, auth.account_id).await;
    let user_locale: Option<String> = sqlx::query_scalar!(
        "SELECT locale FROM users WHERE account_id = $1",
        auth.account_id
    )
    .fetch_optional(&state.db)
    .await?
    .flatten();
    // `I18n.locale`, as `Localized#set_locale` chose it for this request.
    let request_locale = crate::api::mastodon::translations::requested_locale(
        &state,
        &auth,
        &headers,
        params.lang.as_deref(),
    )
    .await?;
    // `User#preferred_posting_language`.
    let language = crate::languages::valid_locale_cascade(&[
        d.language.as_deref(),
        user_locale.as_deref(),
        Some(&request_locale),
    ]);

    Ok(Json(Preferences {
        posting_default_visibility: d.privacy,
        posting_default_sensitive: d.sensitive,
        posting_default_language: language,
        posting_default_quote_policy: d.quote_policy,
        // `setting_display_media`, `setting_expand_spoilers` and
        // `setting_auto_play_gif`: the `web.*` settings, with `UserSettings`'
        // defaults.
        reading_expand_media: settings
            .get("web.display_media")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("default")
            .to_owned(),
        reading_expand_spoilers: settings
            .get("web.expand_content_warnings")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        reading_autoplay_gifs: settings
            .get("web.auto_play")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
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
            url: crate::formatter::text::short_account_tag_url(domain, &account.username, &r.name),
            statuses_count: r.statuses_count.to_string(),
            last_status_at: r.last_status_at.map(|t| t.format("%Y-%m-%d").to_string()),
        })
        .collect();

    let a = &account;
    let fields = crate::api::mastodon::convert::fields_from_db(
        a.fields.as_ref().unwrap_or(&serde_json::json!([])),
        true,
    );
    // `ProfileSerializer`: `account_bio_format` and `account_field_value_format`.
    let mut texts: Vec<&str> = vec![&a.note];
    texts.extend(fields.iter().map(|f| f.value.as_str()));
    let lookup = crate::api::mastodon::formatting::mention_lookup(state, &texts).await;
    let formatted_fields =
        crate::api::mastodon::formatting::field_values(domain, fields.clone(), true, &lookup);
    // `avatar_file_name.present? ? full_asset_url(…) : nil`: no picture is
    // `null` here, not the placeholder the account entity shows.
    let avatar = a.avatar_file_name.as_deref().is_some_and(|f| !f.is_empty());
    let header = a.header_file_name.as_deref().is_some_and(|f| !f.is_empty());
    let profile = crate::api::mastodon::types::Profile {
        id: a.id.to_string(),
        display_name: a.display_name.clone(),
        note: a.note.clone(),
        fields,
        formatted_note: crate::formatter::local_bio(&a.note, domain, &lookup),
        formatted_fields,
        avatar: avatar
            .then(|| crate::api::mastodon::convert::account_avatar_url_for(&state.urls, a)),
        avatar_static: avatar
            .then(|| crate::api::mastodon::convert::account_avatar_static_url(&state.urls, a)),
        avatar_description: a.avatar_description.clone(),
        header: header
            .then(|| crate::api::mastodon::convert::account_header_url_for(&state.urls, a)),
        header_static: header
            .then(|| crate::api::mastodon::convert::account_header_static_url(&state.urls, a)),
        header_description: a.header_description.clone(),
        locked: a.locked,
        bot: a.actor_type.as_deref() == Some("Service"),
        hide_collections: a.hide_collections,
        discoverable: a.discoverable,
        indexable: a.indexable,
        show_media: a.show_media,
        show_media_replies: a.show_media_replies,
        show_featured: a.show_featured,
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
    api.set_roles(fetch_account_roles(&state, account.id).await);
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
    api.set_roles(fetch_account_roles(&state, account.id).await);
    Ok(Json(api))
}
