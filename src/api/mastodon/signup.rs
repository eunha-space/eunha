use crate::{
    crypto,
    error::{AppError, AppResult},
    middleware::ResolvedInstance,
    state::AppState,
    templates,
};
use axum::{
    extract::{Extension, Form, Query},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Json, Redirect, Response},
};
use serde::Deserialize;

use urlencoding;

#[derive(Debug, Deserialize)]
pub struct SignUpQuery {
    invite: Option<String>,
    lang: Option<String>,
}

pub async fn signup_get(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Query(q): Query<SignUpQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    let invite = q.invite.as_deref().unwrap_or("").trim().to_string();
    let accept_lang = headers.get("accept-language").and_then(|v| v.to_str().ok());
    let locale = crate::locale::Locale::detect(q.lang.as_deref(), accept_lang);

    if !instance.registrations_open {
        if invite.is_empty() {
            return render(&instance, &invite, false, false, None, locale);
        }
        if let Err(msg) = validate_invite(&state, &invite).await {
            return render(
                &instance,
                &invite,
                false,
                false,
                Some(locale.t(msg)),
                locale,
            );
        }
    }

    render(&instance, &invite, true, false, None, locale)
}

// ── helpers ────────────────────────────────────────────────────────────────

async fn validate_invite(state: &AppState, code: &str) -> Result<i64, &'static str> {
    // Mirror Mastodon Invite#valid_for_use?:
    //   (max_uses.nil? || uses < max_uses) && !expired? && user&.functional?
    // where functional? requires the inviter's user to be confirmed, approved and
    // not disabled, and their account to be available (not suspended), not a
    // memorial, and not moved.
    let row = sqlx::query!(
        r#"SELECT i.id, i.uses, i.max_uses, i.expires_at,
                  u.confirmed_at, u.approved, u.disabled,
                  a.suspended_at, a.requested_deletion_at, a.memorial, a.moved_to_account_id
           FROM invites i
           JOIN users u ON u.id = i.user_id
           JOIN accounts a ON a.id = u.account_id
           WHERE i.code = $1"#,
        code,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let Some(inv) = row else {
        return Err("err_invalid_invite");
    };
    if inv.max_uses.is_some_and(|m| inv.uses >= m) {
        return Err("err_invite_maxed");
    }
    if inv
        .expires_at
        .is_some_and(|e| e < chrono::Utc::now().naive_utc())
    {
        return Err("err_invite_expired");
    }
    let inviter_functional = inv.confirmed_at.is_some()
        && inv.approved
        && !inv.disabled
        && inv.suspended_at.is_none()
        && inv.requested_deletion_at.is_none()
        && !inv.memorial
        && inv.moved_to_account_id.is_none();
    if !inviter_functional {
        return Err("err_invalid_invite");
    }
    Ok(inv.id)
}

/// Mastodon `Invite#bypass_approval?`: `user&.role&.can?(:invite_bypass_approval)`.
///
/// The permission is the invite *creator's*, computed the way `UserRole` does
/// — the everyone role unioned in, and `administrator` granting everything —
/// so it answers for a member the same way `verify_credentials` does. An invite
/// whose creator has since gone is not a bypass; `validate_invite` has already
/// refused those, and treating a missing row as `false` keeps the failure in
/// the safe direction.
async fn invite_bypasses_approval(state: &AppState, invite_id: i64) -> bool {
    let account_id = sqlx::query_scalar!(
        r#"SELECT u.account_id FROM invites i JOIN users u ON u.id = i.user_id WHERE i.id = $1"#,
        invite_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let Some(account_id) = account_id else {
        return false;
    };
    match super::admin::computed_permissions(state, account_id).await {
        Ok((_, perms)) => perms & super::admin::perm::INVITE_BYPASS_APPROVAL != 0,
        Err(_) => false,
    }
}

/// Mastodon BootstrapTimelineService#autofollow_inviter!: a new account that
/// signed up through an invite flagged `autofollow` follows the inviter's
/// account. A locked inviter receives a follow request instead, matching
/// FollowService's handling of locked targets.
pub(crate) async fn autofollow_inviter(state: &AppState, follower_account_id: i64, invite_id: i64) {
    let inviter = sqlx::query!(
        r#"SELECT i.autofollow, a.id AS "target_id!", a.locked
           FROM invites i
           JOIN users u ON u.id = i.user_id
           JOIN accounts a ON a.id = u.account_id
           WHERE i.id = $1"#,
        invite_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let Some(inviter) = inviter else { return };
    if !inviter.autofollow || inviter.target_id == follower_account_id {
        return;
    }
    let target_id = inviter.target_id;

    let Ok(follower) =
        crate::api::mastodon::accounts::fetch_account(state, follower_account_id).await
    else {
        return;
    };
    let acct = follower.acct();
    let avatar = crate::api::mastodon::convert::account_avatar_url_for(&state.urls, &follower);

    if inviter.locked {
        let inserted = sqlx::query!(
            r#"INSERT INTO follow_requests (account_id, target_account_id, created_at, updated_at)
               VALUES ($1, $2, now(), now())
               ON CONFLICT (account_id, target_account_id) DO NOTHING"#,
            follower_account_id,
            target_id,
        )
        .execute(&state.db)
        .await;
        if !matches!(inserted, Ok(r) if r.rows_affected() > 0) {
            return;
        }
        crate::push::create_and_push(
            state,
            target_id,
            follower_account_id,
            "follow_request",
            None,
            format!("{} wants to follow you", follower.display_name),
            acct,
            avatar,
        )
        .await;
        return;
    }

    let inserted = sqlx::query!(
        r#"INSERT INTO follows (account_id, target_account_id, created_at, updated_at)
           VALUES ($1, $2, now(), now())
           ON CONFLICT (account_id, target_account_id) DO NOTHING"#,
        follower_account_id,
        target_id,
    )
    .execute(&state.db)
    .await;
    if !matches!(inserted, Ok(r) if r.rows_affected() > 0) {
        return;
    }

    let _ = crate::counters::on_follow_created(&state.db, follower_account_id, target_id).await;
    crate::push::create_and_push(
        state,
        target_id,
        follower_account_id,
        "follow",
        None,
        format!("{} followed you", follower.display_name),
        acct,
        avatar,
    )
    .await;
    let mut redis = state.redis.clone();
    crate::feed::backfill_follow(
        &mut redis,
        &state.redis_keys,
        &state.db,
        follower_account_id,
        target_id,
    )
    .await;
}

// ── POST /api/v1/accounts ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ApiCreateAccountForm {
    pub username: String,
    pub email: String,
    pub password: String,
    pub agreement: Option<bool>,
    pub locale: Option<String>,
    pub reason: Option<String>,
    pub invite_code: Option<String>,
}

pub async fn api_create_account(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    req_headers: HeaderMap,
    super::extractors::FormOrJson(form): super::extractors::FormOrJson<ApiCreateAccountForm>,
) -> AppResult<Json<super::types::Token>> {
    let invite_code = form.invite_code.as_deref().unwrap_or("").trim().to_string();
    let invite_id: Option<i64> = if !invite_code.is_empty() {
        Some(
            validate_invite(&state, &invite_code)
                .await
                .map_err(|_| AppError::Unprocessable("Invalid or expired invite code".into()))?,
        )
    } else if !instance.registrations_open {
        return Err(AppError::Unprocessable(
            "This instance is not open for registration".into(),
        ));
    } else {
        None
    };

    let username = form.username.trim().to_lowercase();
    let email = form.email.trim().to_string();
    let password = &form.password;
    let locale_str = form.locale.clone().unwrap_or_else(|| "en".into());

    if username.is_empty()
        || !username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(AppError::Unprocessable(
            "Username can only contain letters, numbers, and underscores".into(),
        ));
    }
    if !email.contains('@') {
        return Err(AppError::Unprocessable("Invalid email address".into()));
    }
    if password.len() < 8 {
        return Err(AppError::Unprocessable(
            "Password must be at least 8 characters".into(),
        ));
    }

    // Reject if email already belongs to a confirmed account.
    let email_confirmed = sqlx::query_scalar!(
        "SELECT 1 FROM users WHERE lower(email) = lower($1) AND confirmed_at IS NOT NULL",
        email,
    )
    .fetch_optional(&state.db)
    .await?
    .is_some();
    if email_confirmed {
        return Err(AppError::Unprocessable("Email is already taken".into()));
    }

    // Reject if username is taken by a confirmed account or a pending signup for a different email.
    let username_taken = sqlx::query_scalar!(
        r#"SELECT 1 FROM accounts WHERE username = $1 AND domain IS NULL
           UNION ALL
           SELECT 1 FROM eunha.pending_signups
             WHERE username = $1
               AND lower(email) != lower($2)
               AND expires_at > now()
           LIMIT 1"#,
        username,
        email,
    )
    .fetch_optional(&state.db)
    .await?
    .is_some();
    if username_taken {
        return Err(AppError::Unprocessable("Username is already taken".into()));
    }

    let password_hash = crypto::hash_password(password)
        .await
        .map_err(|_| AppError::Internal(anyhow::anyhow!("password hashing failed")))?;
    let reason = form
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let confirmation_token = api_generate_token();
    let app_id = extract_app_from_bearer(&state, &req_headers).await;

    sqlx::query!(
        r#"INSERT INTO eunha.pending_signups
             (username, email, email_normalized, password_hash,
              invite_id, reason, locale, app_id, confirmation_token)
           VALUES ($1,$2,lower($2),$3,$4,$5,$6,$7,$8)
           ON CONFLICT (email_normalized) DO UPDATE SET
             username           = EXCLUDED.username,
             password_hash      = EXCLUDED.password_hash,
             invite_id          = EXCLUDED.invite_id,
             reason             = EXCLUDED.reason,
             locale             = EXCLUDED.locale,
             app_id             = EXCLUDED.app_id,
             confirmation_token = EXCLUDED.confirmation_token,
             expires_at         = now() + interval '24 hours'"#,
        username,
        email,
        password_hash,
        invite_id,
        reason,
        locale_str,
        app_id,
        confirmation_token,
    )
    .execute(&state.db)
    .await
    .map_err(|_| AppError::Internal(anyhow::anyhow!("pending signup failed")))?;

    let confirm_url = format!(
        "https://{}/auth/confirm?token={}",
        instance.domain, confirmation_token
    );
    let email_sender = state.email.clone();
    let to = email.clone();
    let uname = username.clone();
    let locale_for_email = locale_str.clone();
    crate::tenants::spawn(async move {
        if let Err(e) = email_sender
            .send_confirmation(&to, &uname, "", &confirm_url, &locale_for_email)
            .await
        {
            tracing::error!(error = %e, "failed to send confirmation email");
        }
    });

    // Return a profile-scoped token placeholder. The token is not stored — it cannot
    // be used to authenticate. A real token is issued after email confirmation.
    Ok(Json(super::types::Token {
        access_token: api_generate_token(),
        token_type: "Bearer".to_string(),
        scope: "profile".to_string(),
        created_at: chrono::Utc::now().timestamp(),
    }))
}

// ── GET /auth/confirm ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ConfirmQuery {
    pub token: String,
}

pub async fn confirm_email(state: AppState, Query(q): Query<ConfirmQuery>) -> Response {
    let pending = sqlx::query!(
        r#"DELETE FROM eunha.pending_signups
           WHERE confirmation_token = $1 AND expires_at > now()
           RETURNING username, email, email_normalized,
                     password_hash, invite_id, reason, locale, app_id"#,
        q.token,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let Some(pending) = pending else {
        // A dead end told someone their link was broken and left them there. The
        // usual reason a link is dead is that it already worked, so send them to
        // the place they were headed anyway and say so there.
        return Redirect::to("/account/login?confirmed=invalid").into_response();
    };

    // Mastodon `User#set_approved`: an invite skips approval only when its
    // creator may bypass it — `Invite#bypass_approval?` asks the inviting
    // user's role for `invite_bypass_approval`, not merely whether an invite
    // was used. eunha's everyone role carries `Flags::DEFAULT`, which is
    // `invite_users` alone, so an ordinary member's invite gets its holder
    // reviewed like anyone else until the instance says otherwise.
    let needs_approval = state.instance.approval_required
        && !match pending.invite_id {
            Some(id) => invite_bypasses_approval(&state, id).await,
            None => false,
        };
    let crate::accounts::LocalUser {
        account_id,
        user_id,
    } = match crate::accounts::create_local(
        &state.db,
        state.encryptor.as_ref(),
        &state.instance.domain,
        crate::accounts::NewLocalUser {
            username: &pending.username,
            email: &pending.email,
            password_hash: &pending.password_hash,
            role_id: None,
            approved: !needs_approval,
            invite_id: pending.invite_id,
            locale: Some(pending.locale.as_str()),
            app_id: pending.app_id,
        },
    )
    .await
    {
        Ok(created) => created,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "could not create the confirmed account");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if let Some(id) = pending.invite_id {
        let _ = sqlx::query!("UPDATE invites SET uses = uses + 1 WHERE id = $1", id)
            .execute(&state.db)
            .await;
    }

    // `User#after_confirmation_tasks`: an approved user is prepared (the
    // inviter followed, staff told with `admin.sign_up`); one awaiting
    // approval is mailed to the staff who can approve it.
    if needs_approval {
        let state2 = state.clone();
        crate::tenants::spawn(async move {
            crate::accounts::notify_staff_about_pending_account(&state2, account_id).await;
        });
    } else {
        crate::accounts::prepare_new_user(&state, account_id).await;
    }

    if let Some(app_id) = pending.app_id {
        if let Ok(Some(app)) = sqlx::query!(
            "SELECT redirect_uri, scopes FROM oauth_applications WHERE id = $1",
            app_id,
        )
        .fetch_optional(&state.db)
        .await
        {
            let redirect_uri = app.redirect_uri.lines().next().unwrap_or("").to_string();
            if !redirect_uri.is_empty() && redirect_uri != "urn:ietf:wg:oauth:2.0:oob" {
                let code = api_generate_token();
                if sqlx::query!(
                    r#"INSERT INTO oauth_access_grants
                         (application_id, resource_owner_id, token, redirect_uri, scopes, expires_in, created_at)
                       VALUES ($1, $2, $3, $4, $5, 600, now())"#,
                    app_id, user_id, code, redirect_uri, app.scopes,
                ).execute(&state.db).await.is_ok() {
                    let sep = if redirect_uri.contains('?') { '&' } else { '?' };
                    return Redirect::to(&format!("{}{}code={}", redirect_uri, sep, code))
                        .into_response();
                }
            }
        }
    }

    // No app to hand back to — a signup from eunha's own form, or one whose app
    // registered no redirect. Sign-in is the next step either way.
    if needs_approval {
        Redirect::to("/account/login?confirmed=pending").into_response()
    } else {
        Redirect::to("/account/login?confirmed=1").into_response()
    }
}

// ── GET /api/v1/emails/check_confirmation ────────────────────────────────

pub async fn check_email_confirmation(
    state: AppState,
    Extension(auth): Extension<crate::middleware::AuthenticatedUser>,
) -> AppResult<Json<bool>> {
    let confirmed = sqlx::query_scalar!(
        "SELECT confirmed_at IS NOT NULL FROM users WHERE account_id = $1",
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .flatten()
    .unwrap_or(false);
    Ok(Json(confirmed))
}

// ── POST /auth/password  (request reset) ──────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PasswordResetRequestForm {
    pub email: Option<String>,
}

pub async fn request_password_reset(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Form(form): Form<PasswordResetRequestForm>,
) -> impl IntoResponse {
    // Always return 200 to avoid email enumeration
    let email = match form.email {
        Some(e) if !e.is_empty() => e.trim().to_lowercase(),
        _ => return StatusCode::OK.into_response(),
    };

    let row = sqlx::query!(
        "SELECT u.id, u.email, a.username FROM users u
         JOIN accounts a ON a.id = u.account_id
         WHERE lower(u.email) = lower($1) AND u.confirmed_at IS NOT NULL",
        email,
    )
    .fetch_optional(&state.db)
    .await;

    let Ok(Some(row)) = row else {
        return StatusCode::OK.into_response();
    };

    let token = crypto::generate_token(32);
    let _ = sqlx::query!(
        "UPDATE users SET reset_password_token = $1, reset_password_sent_at = now() WHERE id = $2",
        token,
        row.id,
    )
    .execute(&state.db)
    .await;

    let reset_url = format!(
        "https://{}/auth/password/reset?token={}",
        instance.domain, token
    );
    let email = state.email.clone();
    let to = row.email.clone();
    let name = row.username.clone();
    crate::tenants::spawn(async move {
        if let Err(e) = email
            .send_password_reset(&to, &name, &reset_url, "en")
            .await
        {
            tracing::error!(error = %e, "failed to send password reset email");
        }
    });

    StatusCode::OK.into_response()
}

// ── PUT /auth/password  (apply reset) ─────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PasswordResetForm {
    pub token: Option<String>,
    pub password: Option<String>,
}

pub async fn apply_password_reset(
    state: AppState,
    Form(form): Form<PasswordResetForm>,
) -> impl IntoResponse {
    let token = match form.token {
        Some(t) if !t.is_empty() => t,
        _ => return (StatusCode::UNPROCESSABLE_ENTITY, "Missing token").into_response(),
    };
    let password = match form.password {
        Some(p) if p.len() >= 8 => p,
        _ => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                "Password must be at least 8 characters",
            )
                .into_response()
        }
    };

    let row = sqlx::query!(
        r#"SELECT id FROM users
           WHERE reset_password_token = $1
             AND reset_password_sent_at > now() - interval '1 hour'"#,
        token,
    )
    .fetch_optional(&state.db)
    .await;

    let Ok(Some(row)) = row else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "Invalid or expired token").into_response();
    };

    let hash = match crypto::hash_password(&password).await {
        Ok(h) => h,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "Server error").into_response(),
    };

    let _ = sqlx::query!(
        "UPDATE users SET encrypted_password = $1, reset_password_token = NULL, reset_password_sent_at = NULL WHERE id = $2",
        hash, row.id,
    )
    .execute(&state.db)
    .await;

    StatusCode::OK.into_response()
}

// ── helpers ────────────────────────────────────────────────────────────────

async fn extract_app_from_bearer(state: &AppState, headers: &HeaderMap) -> Option<i64> {
    let val = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = val.strip_prefix("Bearer ")?.trim();
    sqlx::query_scalar!(
        "SELECT application_id FROM oauth_access_tokens WHERE token = $1 AND resource_owner_id IS NULL",
        token
    ).fetch_optional(&state.db).await.ok().flatten().flatten()
}

fn api_generate_token() -> String {
    use rand::RngCore;
    let mut rng = rand::rng();
    (0..64)
        .map(|_| format!("{:02x}", rng.next_u32() as u8))
        .collect()
}

// ── helpers ────────────────────────────────────────────────────────────────

fn render(
    instance: &crate::config::InstanceConfig,
    invite: &str,
    show_form: bool,
    pending: bool,
    error: Option<&'static str>,
    locale: crate::locale::Locale,
) -> Response {
    let enc_invite = urlencoding::encode(invite);
    let toggle_en_url = if invite.is_empty() {
        "/auth/signup?lang=en".to_string()
    } else {
        format!("/auth/signup?invite={}&lang=en", enc_invite)
    };
    let toggle_ko_url = if invite.is_empty() {
        "/auth/signup?lang=ko".to_string()
    } else {
        format!("/auth/signup?invite={}&lang=ko", enc_invite)
    };
    let html = templates::render(
        "signup.html",
        minijinja::context! {
            instance_title => &instance.title,
            instance_domain => &instance.domain,
            show_form,
            pending,
            approval_required => instance.approval_required,
            invite,
            error,
            lang => locale.as_str(),
            toggle_en_url => toggle_en_url,
            toggle_ko_url => toggle_ko_url,
            t_create_account => locale.t("create_account"),
            t_username => locale.t("username"),
            t_email => locale.t("email"),
            t_password => locale.t("password"),
            t_confirm_password => locale.t("confirm_password"),
            t_already_account => locale.t("already_account"),
            t_sign_in => locale.t("sign_in"),
            t_registrations_closed => locale.t("registrations_closed"),
            t_invite_code => locale.t("invite_code"),
            t_continue_btn => locale.t("continue_btn"),
            t_reason => locale.t("reason"),
            t_reason_hint => locale.t("reason_hint"),
            t_pending_approval => locale.t("pending_approval"),
            t_apply_for_account => locale.t("apply_for_account"),
            t_check_email => locale.t("check_email"),
            t_err_password_mismatch => locale.t("err_password_mismatch"),
            t_err_server => locale.t("err_server"),
        },
    );
    Html(html).into_response()
}
