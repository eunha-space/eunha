use crate::{
    crypto,
    error::{AppError, AppResult},
    middleware::ResolvedInstance,
    state::AppState,
    templates,
};
use axum::{
    extract::{Extension, Query},
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
    let instance = crate::settings::Snapshot::load(&state)
        .await
        .amend(&instance);

    let invite_id = if invite.is_empty() {
        None
    } else {
        validate_invite(&state, &invite).await.ok()
    };
    let form = FormRequirements {
        min_age: crate::settings::min_age(&state.db).await,
        reason_required: reason_required(&state, &instance, invite_id).await,
        terms_of_service: crate::terms_of_service::live_first(&state)
            .await
            .ok()
            .flatten()
            .is_some(),
    };

    if !instance.registrations_open {
        if invite.is_empty() {
            return render(&instance, &invite, false, false, None, locale, &form);
        }
        if let Err(msg) = validate_invite(&state, &invite).await {
            return render(
                &instance,
                &invite,
                false,
                false,
                Some(locale.t(msg)),
                locale,
                &form,
            );
        }
    }

    render(&instance, &invite, true, false, None, locale, &form)
}

/// What the sign-up form asks for beyond the basics.
struct FormRequirements {
    /// `Setting.min_age`: a date of birth, checked against it.
    min_age: Option<u32>,
    /// `User#invite_text_required?`.
    reason_required: bool,
    /// Whether there are terms of service to agree to besides the privacy
    /// policy (`auth.user_agreement_html` or `user_privacy_agreement_html`).
    terms_of_service: bool,
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
    // `AccountStat`'s `update_index('accounts', :account)`.
    crate::search::elasticsearch::indexing::accounts(state, &[follower_account_id, target_id])
        .await;

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
    // `FollowService#direct_follow!`'s `MergeWorker`s.
    crate::home_feed::merge_into_home_and_lists(state, target_id, follower_account_id).await;
}

// ── POST /api/v1/accounts ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ApiCreateAccountForm {
    pub username: String,
    pub email: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub agreement: Acceptance,
    pub locale: Option<String>,
    pub reason: Option<String>,
    pub invite_code: Option<String>,
    /// Asked for when the instance sets a minimum age (`Setting.min_age`).
    pub date_of_birth: Option<String>,
    /// `users.time_zone`; a name Rails would not know is dropped, as
    /// `User` normalizes it.
    pub time_zone: Option<String>,
}

/// `validates :agreement, acceptance: { accept: [true, 'true', '1'] }`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Acceptance(pub bool);

impl<'de> Deserialize<'de> for Acceptance {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Acceptance(match serde_json::Value::deserialize(d)? {
            serde_json::Value::Bool(b) => b,
            serde_json::Value::String(s) => s == "true" || s == "1",
            _ => false,
        }))
    }
}

/// Why a sign-up was refused: as before, or the model's validations as
/// `ValidationErrorFormatter` renders them.
pub enum SignupError {
    App(AppError),
    Invalid(crate::email_subscriptions::ValidationErrors),
}

impl From<AppError> for SignupError {
    fn from(error: AppError) -> Self {
        Self::App(error)
    }
}

impl From<sqlx::Error> for SignupError {
    fn from(error: sqlx::Error) -> Self {
        Self::App(error.into())
    }
}

impl IntoResponse for SignupError {
    fn into_response(self) -> Response {
        match self {
            Self::App(error) => error.into_response(),
            Self::Invalid(errors) => errors.into_response(),
        }
    }
}

/// `UserInviteRequest::TEXT_SIZE_LIMIT`.
const REASON_SIZE_LIMIT: usize = 420;
/// Devise's `password_length`.
const PASSWORD_LENGTH: std::ops::RangeInclusive<usize> = 8..=72;

/// Devise's validatable for the password: present, and of `password_length`.
fn validate_password(errors: &mut crate::email_subscriptions::ValidationErrors, password: &str) {
    let password_length = password.chars().count();
    if password.is_empty() {
        errors.add("password", "blank", "can't be blank");
    } else if password_length < *PASSWORD_LENGTH.start() {
        errors.add(
            "password",
            "too_short",
            "is too short (minimum is 8 characters)",
        );
    } else if password_length > *PASSWORD_LENGTH.end() {
        errors.add(
            "password",
            "too_long",
            "is too long (maximum is 72 characters)",
        );
    }
}

/// `UserInviteRequest`'s validations on its text, filed under `reason`
/// (`'invite_request.text': :reason`): present when it is required, and at
/// most 420 characters.
fn validate_reason(
    errors: &mut crate::email_subscriptions::ValidationErrors,
    reason: Option<&str>,
    required: bool,
) {
    let reason = reason.map(str::trim).unwrap_or("");
    if reason.is_empty() {
        if required {
            errors.add_as("reason", "Reason", "blank", "can't be blank");
        }
    } else if reason.chars().count() > REASON_SIZE_LIMIT {
        errors.add_as(
            "reason",
            "Reason",
            "too_long",
            "is too long (maximum is 420 characters)",
        );
    }
}

/// `agreement`'s acceptance, then `date_of_birth` against `Setting.min_age`.
async fn validate_agreement_and_age(
    state: &AppState,
    errors: &mut crate::email_subscriptions::ValidationErrors,
    form: &ApiCreateAccountForm,
) {
    if !form.agreement.0 {
        errors.add_as(
            "agreement",
            "Service agreement",
            "accepted",
            "must be accepted",
        );
    }
    if let Some(min_age) = crate::settings::min_age(&state.db).await {
        match form.date_of_birth.as_deref().and_then(parse_date_of_birth) {
            None => errors.add("date_of_birth", "blank", "can't be blank"),
            Some(born) if !old_enough(born, min_age, chrono::Utc::now().date_naive()) => {
                errors.add("date_of_birth", "below_limit", "is below the age limit");
            }
            Some(_) => {}
        }
    }
}

/// `attribute :date_of_birth, :date`: an ISO 8601 date.
fn parse_date_of_birth(value: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d").ok()
}

/// `DateOfBirthValidator`: born no later than `min_age` years before today.
fn old_enough(born: chrono::NaiveDate, min_age: u32, today: chrono::NaiveDate) -> bool {
    match today.checked_sub_months(chrono::Months::new(min_age.saturating_mul(12))) {
        Some(limit) => born <= limit,
        None => false,
    }
}

/// `User#invite_text_required?`: `Setting.require_invite_text` while
/// registrations are not open to all, unless the invite bypasses approval.
async fn reason_required(
    state: &AppState,
    instance: &crate::config::InstanceConfig,
    invite_id: Option<i64>,
) -> bool {
    let open = instance.registrations_open && !instance.approval_required;
    if open || !crate::settings::boolean(state, "require_invite_text").await {
        return false;
    }
    match invite_id {
        Some(id) => !invite_bypasses_approval(state, id).await,
        None => true,
    }
}

/// A user made by a sign-up, and the access token its app was given.
pub struct Registered {
    pub user_id: i64,
    pub account_id: i64,
    pub token: Option<super::types::Token>,
}

/// The app a sign-up came through: `doorkeeper_token.application`, and the
/// scopes its token is issued with (`@app.scopes`).
pub struct SignUpApp {
    pub id: i64,
    pub scopes: String,
}

/// `AppSignUpService#call` and `Auth::RegistrationsController#create`: the
/// user and its account saved together, unconfirmed, as `User.create!` saves
/// them — the account with its signing key, the user with its
/// `confirmation_token`, approval (`set_approved`), address and reason
/// (`user_invite_requests`), the invite's use counted (`counter_cache: :uses`),
/// and, for an app, the access token it is handed back — in one transaction;
/// then, as the commit's callbacks do, `account.created` and the confirmation
/// mail.
pub async fn register(
    state: &AppState,
    instance: &crate::config::InstanceConfig,
    form: &ApiCreateAccountForm,
    sign_up_ip: Option<std::net::IpAddr>,
    app: Option<SignUpApp>,
) -> Result<Registered, SignupError> {
    // `User#invite_code=`: the invite with that code, whatever its state.
    let invite_code = form.invite_code.as_deref().unwrap_or("").trim().to_string();
    let invite_id: Option<i64> = if invite_code.is_empty() {
        None
    } else {
        sqlx::query_scalar!("SELECT id FROM invites WHERE code = $1", invite_code)
            .fetch_optional(&state.db)
            .await?
    };
    let valid_invitation =
        !invite_code.is_empty() && validate_invite(state, &invite_code).await.is_ok();
    // `check_enabled_registrations`: `allowed_registration?` — registrations
    // open, or an invite good for use, and no IP block on signing up.
    if !(instance.registrations_open || valid_invitation)
        || crate::remote_ip::sign_up_blocked(state, sign_up_ip).await
    {
        return Err(AppError::Forbidden.into());
    }

    // `normalizes :username, with: squish`: the case is kept as entered.
    let username = form.username.trim().to_string();
    let email = form.email.trim().to_lowercase();
    let locale = form.locale.clone().unwrap_or_else(|| "en".into());
    // `User`'s validations, every one of them, in the order the model runs
    // them, so that a refusal names everything wrong at once.
    let mut errors = crate::email_subscriptions::ValidationErrors::default();
    let email_label = "E-mail address";
    // Devise's validatable: the address present, unique among users confirmed
    // or not, and shaped like one; then the password.
    let email_shaped = crate::accounts::valid_email(&email);
    if email.is_empty() {
        errors.add_as("email", email_label, "blank", "can't be blank");
    } else {
        let taken = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM users WHERE lower(email) = $1) AS "e!""#,
            email,
        )
        .fetch_one(&state.db)
        .await?;
        if taken {
            errors.add_as("email", email_label, "taken", "has already been taken");
        }
        if !email_shaped {
            errors.add_as("email", email_label, "invalid", "is invalid");
        }
    }
    validate_password(&mut errors, &form.password);

    // The account's, filed under `username` (`'account.username': :username`).
    if username.is_empty() {
        errors.add_as("username", "Username", "blank", "can't be blank");
    } else {
        // `UniqueUsernameValidator`, which ignores case.
        let taken = sqlx::query_scalar!(
            r#"SELECT EXISTS (
                 SELECT 1 FROM accounts WHERE lower(username) = lower($1) AND domain IS NULL
               ) AS "e!""#,
            username,
        )
        .fetch_one(&state.db)
        .await?;
        if taken {
            errors.add_as("username", "Username", "taken", "has already been taken");
        }
        if !username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            errors.add_as(
                "username",
                "Username",
                "invalid",
                "must contain only letters, numbers and underscores",
            );
        }
        if username.chars().count() > 30 {
            errors.add_as(
                "username",
                "Username",
                "too_long",
                "is too long (maximum is 30 characters)",
            );
        }
        // `UnreservedUsernameValidator`.
        if crate::moderation::signup::username_blocked(state, &username, false).await {
            let (attribute, label, key, message) =
                crate::moderation::signup::Refusal::UsernameReserved.detail();
            errors.add_as(attribute, label, key, message);
        }
    }

    // The invite request's text.
    validate_reason(
        &mut errors,
        form.reason.as_deref(),
        reason_required(state, instance, invite_id.filter(|_| valid_invitation)).await,
    );

    // `EmailMxValidator`, then `UserEmailValidator` unless the invite is good.
    let mut mx_records = Vec::new();
    if email_shaped {
        let refusals =
            crate::moderation::signup::email_refusals(state, &email, sign_up_ip, valid_invitation)
                .await;
        mx_records = refusals.mx_records;
        for refusal in refusals.refusals {
            let (attribute, label, key, message) = refusal.detail();
            errors.add_as(attribute, label, key, message);
        }
    }

    validate_agreement_and_age(state, &mut errors, form).await;

    if !errors.is_empty() {
        return Err(SignupError::Invalid(errors));
    }
    // `User#requires_approval?`.
    let requires_approval = crate::moderation::signup::approval_required(
        state, &username, &email, mx_records, sign_up_ip,
    )
    .await;

    // `User#set_approved`.
    let approved = !requires_approval
        && (crate::settings::registrations_mode(state).await.open()
            || match invite_id.filter(|_| valid_invitation) {
                Some(id) => invite_bypasses_approval(state, id).await,
                None => false,
            });

    let password_hash = crypto::hash_password(&form.password)
        .await
        .map_err(|_| AppError::Internal(anyhow::anyhow!("password hashing failed")))?;
    let reason = form
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let confirmation_token = crypto::generate_token(32);
    let time_zone = crate::time_zones::normalize(form.time_zone.as_deref());

    let prepared = crate::accounts::prepare_local(&state.db, true)
        .await
        .map_err(AppError::Internal)?;
    let mut tx = state.db.begin().await?;
    let created = crate::accounts::insert_local(
        &mut tx,
        state.encryptor.as_ref(),
        &state.instance.domain,
        crate::accounts::NewLocalUser {
            username: &username,
            email: &email,
            password_hash: &password_hash,
            role_id: None,
            approved,
            invite_id,
            locale: Some(locale.as_str()),
            app_id: app.as_ref().map(|a| a.id),
            sign_up_ip,
            invite_request: reason,
            time_zone: time_zone.as_deref(),
            confirmed: false,
            confirmation_token: Some(&confirmation_token),
            account_id: None,
        },
        prepared,
    )
    .await
    .map_err(|e| match e.downcast_ref::<sqlx::Error>() {
        // Saved by someone else between the check and the insert:
        // `ActiveRecord::RecordNotUnique`.
        Some(sqlx::Error::Database(db)) if db.is_unique_violation() => {
            let mut errors = crate::email_subscriptions::ValidationErrors::default();
            errors.add_as("username", "Username", "taken", "has already been taken");
            SignupError::Invalid(errors)
        }
        _ => SignupError::App(AppError::Internal(e)),
    })?;
    if let Some(id) = invite_id {
        sqlx::query!(
            "UPDATE invites SET uses = uses + 1, updated_at = now() WHERE id = $1",
            id
        )
        .execute(&mut *tx)
        .await?;
    }
    // `AppSignUpService#create_access_token!`.
    let token = match &app {
        Some(app) => {
            let access_token = crypto::generate_token(64);
            sqlx::query!(
                r#"INSERT INTO oauth_access_tokens
                     (application_id, resource_owner_id, token, scopes, created_at)
                   VALUES ($1, $2, $3, $4, now())"#,
                app.id,
                created.user_id,
                access_token,
                app.scopes,
            )
            .execute(&mut *tx)
            .await?;
            Some(super::types::Token {
                access_token,
                token_type: "Bearer".to_string(),
                scope: app.scopes.clone(),
                created_at: chrono::Utc::now().timestamp(),
            })
        }
        None => None,
    };
    tx.commit().await?;

    // `User#trigger_webhooks` and the account's `after_commit`, then Devise's
    // `send_on_create_confirmation_instructions`.
    crate::moderation::webhooks::trigger(
        state,
        "account.created",
        crate::moderation::webhooks::Object::Account(created.account_id),
    )
    .await;
    crate::fasp::events::account_created(state, created.account_id).await;
    if let Err(error) =
        crate::accounts::send_confirmation_instructions(state, created.user_id).await
    {
        tracing::error!(error = %format!("{error:#}"), "could not send confirmation instructions");
    }

    Ok(Registered {
        user_id: created.user_id,
        account_id: created.account_id,
        token,
    })
}

/// The bearer token's app, when the token is one the client-credentials grant
/// gave it, with its scopes.
async fn app_token(state: &AppState, headers: &HeaderMap) -> Option<(Option<i64>, String, i64)> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    let row = sqlx::query!(
        r#"SELECT t.application_id, t.resource_owner_id, t.scopes,
                  t.revoked_at IS NOT NULL AS "revoked!",
                  (t.expires_in IS NOT NULL
                   AND t.created_at + make_interval(secs => t.expires_in) < now()) AS "expired!"
           FROM oauth_access_tokens t WHERE t.token = $1"#,
        token
    )
    .fetch_optional(&state.db)
    .await
    .ok()??;
    if row.revoked || row.expired {
        return None;
    }
    Some((
        row.resource_owner_id,
        row.scopes.unwrap_or_default(),
        row.application_id?,
    ))
}

/// Whether `scopes` grant `write:accounts`, as `doorkeeper_authorize!(:write,
/// :'write:accounts')` asks.
fn grants_write_accounts(scopes: &str) -> bool {
    scopes
        .split(|c: char| c.is_whitespace() || c == ',')
        .any(|s| s == "write" || s == "write:accounts")
}

/// `POST /api/v1/accounts`: `Api::V1::AccountsController#create`, for an app
/// with a client-credentials token carrying `write:accounts`. Answers with the
/// new user's access token, as `Doorkeeper::OAuth::TokenResponse` does.
pub async fn api_create_account(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    client_ip: Option<Extension<crate::remote_ip::ClientIp>>,
    req_headers: HeaderMap,
    super::extractors::FormOrJson(form): super::extractors::FormOrJson<ApiCreateAccountForm>,
) -> Result<Json<super::types::Token>, SignupError> {
    // `doorkeeper_authorize!` and `require_client_credentials!`.
    let Some((owner, scopes, app_id)) = app_token(&state, &req_headers).await else {
        return Err(AppError::UnauthorizedMsg("The access token is invalid".into()).into());
    };
    if !grants_write_accounts(&scopes) {
        return Err(AppError::ForbiddenScope.into());
    }
    if owner.is_some() {
        return Err(AppError::ForbiddenMsg(
            "This method requires an client credentials authentication".into(),
        )
        .into());
    }
    // The app's own scopes, which the new token is issued with.
    let app_scopes = sqlx::query_scalar!(
        "SELECT scopes FROM oauth_applications WHERE id = $1",
        app_id
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or_default();
    let sign_up_ip = client_ip.and_then(|Extension(c)| c.0);
    let instance = crate::settings::Snapshot::load(&state)
        .await
        .amend(&instance);
    let registered = register(
        &state,
        &instance,
        &form,
        sign_up_ip,
        Some(SignUpApp {
            id: app_id,
            scopes: app_scopes,
        }),
    )
    .await?;
    registered
        .token
        .map(Json)
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("no token was issued")).into())
}

// ── GET /auth/confirm ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ConfirmQuery {
    pub token: String,
    /// Set on the link mailed to a user, as Mastodon's
    /// `confirmation_instructions` sets it.
    #[serde(default)]
    pub redirect_to_app: Option<String>,
}

/// `Auth::ConfirmationsController#show`: Devise's `confirm_by_token`, for a
/// link sent within `confirm_within`, then `after_confirmation_path_for`: the
/// app the user signed up through, at its first redirect URI as it stands,
/// when the link asks to go back to it (`redirect_to_app`); the web app for a
/// browser already signed in; the sign-in page otherwise.
pub async fn confirm_email(
    state: AppState,
    client_ip: Option<Extension<crate::remote_ip::ClientIp>>,
    headers: HeaderMap,
    Query(q): Query<ConfirmQuery>,
) -> Response {
    let user = sqlx::query!(
        r#"SELECT u.id, a.redirect_uri AS "redirect_uri?"
           FROM users u
           LEFT JOIN oauth_applications a ON a.id = u.created_by_application_id
           WHERE u.confirmation_token = $1
             AND u.confirmation_sent_at > now() - make_interval(days => $2)"#,
        q.token,
        crate::accounts::CONFIRM_WITHIN_DAYS,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let Some(user) = user else {
        // A dead end told someone their link was broken and left them there. The
        // usual reason a link is dead is that it already worked, so send them to
        // the place they were headed anyway and say so there.
        return Redirect::to("/account/login?confirmed=invalid").into_response();
    };
    if let Err(e) = crate::accounts::confirm_user(&state, user.id, true).await {
        tracing::error!(error = %format!("{e:#}"), "could not confirm a user");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // `confirmation_redirect_uri`: `redirect_uri.lines.first.strip`.
    let redirect_to_app = matches!(q.redirect_to_app.as_deref(), Some("true" | "1" | "on"));
    if let (true, Some(uri)) = (redirect_to_app, user.redirect_uri.as_deref()) {
        let uri = uri.lines().next().unwrap_or("").trim();
        if let Ok(location) = axum::http::HeaderValue::from_str(uri) {
            return (
                StatusCode::SEE_OTHER,
                [(axum::http::header::LOCATION, location)],
            )
                .into_response();
        }
    }
    if crate::api::account::signed_in(&state, &headers, client_ip.and_then(|Extension(c)| c.0))
        .await
    {
        return Redirect::to("/").into_response();
    }
    let approved = sqlx::query_scalar!("SELECT approved FROM users WHERE id = $1", user.id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(false);
    if approved {
        Redirect::to("/account/login?confirmed=1").into_response()
    } else {
        Redirect::to("/account/login?confirmed=pending").into_response()
    }
}

// ── POST /api/v1/emails/confirmations ────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct ResendConfirmationForm {
    pub email: Option<String>,
}

/// `Api::V1::Emails::ConfirmationsController#create`: for the app the user
/// signed up through, while the address awaits confirmation, an address put
/// right (`update!(email:)`, which Devise's reconfirmable holds in
/// `unconfirmed_email`) and the confirmation mailed again
/// (`resend_confirmation_instructions`).
pub async fn resend_email_confirmation(
    state: AppState,
    auth: Option<Extension<crate::middleware::AuthenticatedUser>>,
    super::extractors::FormOrJson(form): super::extractors::FormOrJson<ResendConfirmationForm>,
) -> AppResult<Json<serde_json::Value>> {
    // `doorkeeper_authorize! :write, :'write:accounts'`.
    let Some(Extension(auth)) = auth else {
        return Err(AppError::UnauthorizedMsg(
            "The access token is invalid".into(),
        ));
    };
    auth.require_scope("write:accounts")?;
    let user = match auth.user_id {
        Some(user_id) => {
            sqlx::query!(
                r#"SELECT id, email, confirmed_at IS NOT NULL AS "confirmed!",
                      COALESCE(unconfirmed_email, '') <> '' AS "reconfirming!",
                      created_by_application_id
               FROM users WHERE id = $1"#,
                user_id,
            )
            .fetch_optional(&state.db)
            .await?
        }
        None => None,
    };
    // `require_user_owned_by_application!`.
    let Some(user) = user.filter(|u| {
        u.created_by_application_id.is_some() && u.created_by_application_id == auth.application_id
    }) else {
        return Err(AppError::ForbiddenMsg(
            "This method is only available to the application the user originally signed-up with"
                .into(),
        ));
    };
    // `require_user_not_confirmed!`.
    if user.confirmed && !user.reconfirming {
        return Err(AppError::ForbiddenMsg(
            "This method is only available while the e-mail is awaiting confirmation".into(),
        ));
    }
    if let Some(email) = form.email.as_deref() {
        let email = email.trim().to_lowercase();
        if email != user.email {
            if !crate::accounts::valid_email(&email) {
                return Err(AppError::Unprocessable(
                    "Validation failed: E-mail address is invalid".into(),
                ));
            }
            let taken = sqlx::query_scalar!(
                r#"SELECT EXISTS (SELECT 1 FROM users WHERE lower(email) = $1 AND id <> $2) AS "e!""#,
                email,
                user.id,
            )
            .fetch_one(&state.db)
            .await?;
            if taken {
                return Err(AppError::Unprocessable(
                    "Validation failed: E-mail address has already been taken".into(),
                ));
            }
            if let Err(refusal) =
                crate::moderation::signup::check_email(&state, &email, user.confirmed).await
            {
                let (_, label, _, message) = refusal.detail();
                return Err(AppError::Unprocessable(format!(
                    "Validation failed: {label} {message}"
                )));
            }
            crate::accounts::set_unconfirmed_email(&state.db, user.id, &email).await?;
            // `after_commit :send_reconfirmation_instructions`, for the
            // address `update!` held back.
            crate::accounts::send_confirmation_instructions(&state, user.id).await?;
        }
    }
    // `resend_confirmation_instructions`: the same link, once more.
    crate::accounts::send_confirmation_instructions(&state, user.id).await?;
    Ok(Json(serde_json::json!({})))
}

// ── GET /api/v1/emails/check_confirmation ────────────────────────────────

/// `Api::V1::Emails::ConfirmationsController#check`: whether the user has
/// confirmed their address, for any token of theirs carrying
/// `read:accounts`.
pub async fn check_email_confirmation(
    state: AppState,
    auth: Option<Extension<crate::middleware::AuthenticatedUser>>,
) -> AppResult<Json<bool>> {
    // `require_authenticated_user!`.
    let Some(Extension(auth)) = auth.filter(|Extension(a)| a.user_id.is_some()) else {
        return Err(AppError::UnauthorizedMsg(
            "This method requires an authenticated user".into(),
        ));
    };
    // `authorize_if_got_token! :read, :'read:accounts'`.
    auth.require_scope("read:accounts")?;
    let confirmed = sqlx::query_scalar!(
        r#"SELECT confirmed_at IS NOT NULL AS "confirmed!" FROM users WHERE id = $1"#,
        auth.user_id,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    Ok(Json(confirmed))
}

// ── helpers ────────────────────────────────────────────────────────────────

fn render(
    instance: &crate::config::InstanceConfig,
    invite: &str,
    show_form: bool,
    pending: bool,
    error: Option<&'static str>,
    locale: crate::locale::Locale,
    form: &FormRequirements,
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
    // `auth.user_agreement_html`, or `user_privacy_agreement_html` when there
    // are no terms of service.
    let link = |href: &str, text: &str| format!("<a href=\"{href}\" target=\"_blank\">{text}</a>");
    let privacy = link("/privacy-policy", locale.t("privacy_policy"));
    let agreement_html = if form.terms_of_service {
        locale
            .t("agree_terms")
            .replace(
                "%{terms}",
                &link("/terms-of-service", locale.t("terms_of_service")),
            )
            .replace("%{privacy}", &privacy)
    } else {
        locale.t("agree_privacy").replace("%{privacy}", &privacy)
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
            min_age => form.min_age,
            reason_required => form.reason_required,
            terms_of_service => form.terms_of_service,
            t_date_of_birth => locale.t("date_of_birth"),
            agreement_html,
        },
    );
    Html(html).into_response()
}
