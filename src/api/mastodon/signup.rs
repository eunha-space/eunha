//! Signing up: `POST /api/v1/accounts`, the sign-up page, and confirming
//! the email address.
//!
//! Mastodon: app/controllers/api/v1/accounts_controller.rb
//! Mastodon: app/services/app_sign_up_service.rb
//! Mastodon: app/controllers/auth/registrations_controller.rb
//! Mastodon: app/controllers/auth/confirmations_controller.rb
//! Mastodon: app/controllers/api/v1/emails/confirmations_controller.rb
//! Mastodon: app/controllers/api/v1/invites_controller.rb, app/helpers/registration_helper.rb

use crate::{
    crypto,
    error::{AppError, AppResult},
    middleware::ResolvedInstance,
    state::AppState,
    templates,
};
use axum::{
    extract::{Extension, Query},
    http::StatusCode,
    response::{Html, IntoResponse, Json, Redirect, Response},
};
use serde::Deserialize;

use urlencoding;

#[derive(Debug, Deserialize)]
pub struct SignUpQuery {
    invite: Option<String>,
    lang: Option<String>,
}

/// `GET /api/eunha/v1/invite?invite=`: whether a code is good for use, who
/// made it and what it does, by the same checks as registration.
pub async fn invite_lookup(
    state: AppState,
    Query(q): Query<SignUpQuery>,
) -> AppResult<Json<serde_json::Value>> {
    use sqlx::Row;
    let code = q.invite.as_deref().unwrap_or("").trim();
    let id = match validate_invite(&state, code).await {
        Ok(id) => id,
        Err(reason) => return Ok(Json(serde_json::json!({"valid": false, "reason": reason}))),
    };
    let row = sqlx::query("SELECT i.autofollow, a.id, a.username, a.display_name FROM invites i JOIN users u ON u.id = i.user_id JOIN accounts a ON a.id = u.account_id WHERE i.id = $1")
        .bind(id).fetch_one(&state.db).await?;
    Ok(Json(serde_json::json!({
        "valid": true,
        "bypass_approval": invite_bypasses_approval(&state, id).await,
        "autofollow": row.get::<bool, _>("autofollow"),
        "inviter": {
            "id": row.get::<i64, _>("id").to_string(),
            "acct": row.get::<String, _>("username"),
            "display_name": row.get::<String, _>("display_name"),
        }
    })))
}

/// `GET /invite/:invite_code`, Mastodon's `public_invite` link.
///
/// Asked for as JSON (`req.format == :json`: a `.json` suffix or an `Accept`
/// that names JSON), it is `Api::V1::InvitesController#show`, which the apps
/// open an invite link with: 404 for no such code, 401 for one not good for
/// use, 403 when `allowed_registration?` refuses, and otherwise the code and
/// where the instance's API is. Anything else is `Auth::RegistrationsController
/// #new` with the code filled in: a redirect to the sign-up form.
pub async fn public_invite(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    axum::extract::Path(code): axum::extract::Path<String>,
    client_ip: Option<Extension<crate::remote_ip::ClientIp>>,
    request: axum::extract::Request,
) -> Response {
    let accepts_json = request
        .headers()
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .and_then(|accept| accept.split(',').next())
        .is_some_and(|first| first.trim().starts_with("application/json"));
    let (code, json) = match code.strip_suffix(".json") {
        Some(code) => (code.to_string(), true),
        None => (code, accepts_json),
    };
    if !json {
        // `Auth::RegistrationsController#new` with the code filled in.
        return Redirect::to(&format!(
            "/auth/signup?invite={}",
            urlencoding::encode(&code)
        ))
        .into_response();
    }
    invite_show(
        &state,
        &instance,
        &code,
        client_ip.and_then(|Extension(c)| c.0),
    )
    .await
    .into_response()
}

async fn invite_show(
    state: &AppState,
    instance: &crate::config::InstanceConfig,
    code: &str,
    ip: Option<std::net::IpAddr>,
) -> AppResult<Json<serde_json::Value>> {
    // `Invite.find_by!(code:)`.
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM invites WHERE code = $1)")
        .bind(code)
        .fetch_one(&state.db)
        .await?;
    if !exists {
        return Err(AppError::NotFound);
    }
    // `check_valid_usage!`, before registrations are asked about at all.
    if validate_invite(state, code).await.is_err() {
        return Err(AppError::UnauthorizedMsg("This invite is not valid".into()));
    }
    // `allowed_registration?`: the invite is good, so only an IP block on
    // signing up (or a full instance, as for every sign-up here) refuses.
    if !state.growth_limits.registrations() || crate::remote_ip::sign_up_blocked(state, ip).await {
        return Err(AppError::Forbidden);
    }
    Ok(Json(serde_json::json!({
        "invite_code": code,
        "instance_api_url": format!("https://{}/api/v2/instance", instance.web_domain()),
    })))
}

/// `GET /auth/sign_up`, Devise's `new_user_registration_path`: eunha's form is
/// at `/auth/signup`, so this sends the browser there with its query (an
/// invite code, a language).
pub async fn mastodon_sign_up(uri: axum::http::Uri) -> Response {
    let location = match uri.query() {
        Some(query) if !query.is_empty() => format!("/auth/signup?{query}"),
        _ => "/auth/signup".to_owned(),
    };
    (
        axum::http::StatusCode::FOUND,
        [(axum::http::header::LOCATION, location)],
    )
        .into_response()
}

pub async fn signup_get(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Query(q): Query<SignUpQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    // Someone with only the link may paste the whole of it.
    let invite = invite_code_from(q.invite.as_deref().unwrap_or(""));
    let accept_lang = headers.get("accept-language").and_then(|v| v.to_str().ok());
    let locale = crate::locale::Locale::detect(q.lang.as_deref(), accept_lang);
    let instance = crate::settings::Snapshot::load(&state)
        .await
        .amend(&instance);

    let invite_check = if invite.is_empty() {
        None
    } else {
        Some(validate_invite(&state, &invite).await)
    };
    let invitation = match invite_check {
        Some(Ok(id)) => invitation(&state, id).await,
        _ => None,
    };
    let form = FormRequirements {
        // A language picked on the form is the account's; otherwise
        // `POST /auth` goes by the browser's, from every language Mastodon
        // offers.
        picked_language: q.lang.is_some().then(|| locale.as_str()),
        min_age: crate::settings::min_age(&state.db).await,
        ask_reason: instance.approval_required
            && !invitation.as_ref().is_some_and(|i| i.bypasses_approval),
        reason_required: reason_required(&state, &instance, invitation.as_ref().map(|i| i.id))
            .await,
        terms_of_service: crate::terms_of_service::live_first(&state)
            .await
            .ok()
            .flatten()
            .is_some(),
    };
    let error = match invite_check {
        Some(Err(msg)) => Some(locale.t(msg)),
        _ => None,
    };
    let show_form = instance.registrations_open || invitation.is_some();
    render(
        &instance,
        &invite,
        show_form,
        error,
        invitation.as_ref(),
        locale,
        &form,
    )
}

/// What the sign-up form asks for beyond the basics.
struct FormRequirements {
    /// The language picked on the form, to save as the account's.
    picked_language: Option<&'static str>,
    /// `Setting.min_age`: a date of birth, checked against it.
    min_age: Option<u32>,
    /// Whether to ask why they want to join: the instance reviews sign-ups
    /// and the invite, if any, does not skip that.
    ask_reason: bool,
    /// `User#invite_text_required?`.
    reason_required: bool,
    /// Whether there are terms of service to agree to besides the privacy
    /// policy (`auth.user_agreement_html` or `user_privacy_agreement_html`).
    terms_of_service: bool,
}

/// An invite good for use, as the sign-up form describes it.
struct Invitation {
    id: i64,
    /// The inviter's username.
    inviter: String,
    autofollow: bool,
    bypasses_approval: bool,
}

async fn invitation(state: &AppState, invite_id: i64) -> Option<Invitation> {
    let (autofollow, inviter) = sqlx::query_as::<_, (bool, String)>(
        "SELECT i.autofollow, a.username
         FROM invites i
         JOIN users u ON u.id = i.user_id
         JOIN accounts a ON a.id = u.account_id
         WHERE i.id = $1",
    )
    .bind(invite_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()?;
    Some(Invitation {
        id: invite_id,
        inviter,
        autofollow,
        bypasses_approval: invite_bypasses_approval(state, invite_id).await,
    })
}

/// The code in what someone typed or pasted: a bare code, or an invite link
/// in Mastodon's form (`/invite/CODE`) or the sign-up form's (`?invite=CODE`).
fn invite_code_from(input: &str) -> String {
    let text = input.trim();
    if !(text.contains("/invite/") || text.contains("invite=") || text.contains("invite_code=")) {
        return text.to_owned();
    }
    let link = text.split('#').next().unwrap_or_default();
    let (path, query) = link.split_once('?').unwrap_or((link, ""));
    for pair in query.split('&') {
        if let Some(("invite" | "invite_code", value)) = pair.split_once('=') {
            if let Ok(value) = urlencoding::decode(value) {
                if !value.trim().is_empty() {
                    return value.trim().to_owned();
                }
            }
        }
    }
    match path.rsplit_once("/invite/") {
        Some((_, code)) if !code.trim_end_matches('/').is_empty() => {
            let code = code.trim_end_matches('/');
            code.strip_suffix(".json").unwrap_or(code).to_owned()
        }
        _ => text.to_owned(),
    }
}

/// `GET /auth/signup/username?username=`: what the sign-up form would say
/// about a username, so the form can say it while it is being typed.
pub async fn username_check(
    state: AppState,
    Query(q): Query<UsernameQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let mut errors = crate::email_subscriptions::ValidationErrors::default();
    validate_username(&state, &mut errors, q.username.trim()).await?;
    Ok(Json(errors.body()))
}

#[derive(Debug, Deserialize)]
pub struct UsernameQuery {
    #[serde(default)]
    username: String,
}

// ── helpers ────────────────────────────────────────────────────────────────

async fn validate_invite(state: &AppState, code: &str) -> Result<i64, &'static str> {
    // Mirror Mastodon Invite#valid_for_use?:
    //   (max_uses.nil? || uses < max_uses) && !expired? && user&.functional?
    // (`UserStanding`).
    let row = sqlx::query!(
        r#"SELECT i.id, i.uses, i.max_uses, i.expires_at, i.user_id
           FROM invites i
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
    let inviter_functional = crate::user_standing::UserStanding::of_user(&state.db, inv.user_id)
        .await
        .ok()
        .flatten()
        .is_some_and(|standing| standing.functional());
    if !inviter_functional {
        return Err("err_invalid_invite");
    }
    Ok(inv.id)
}

async fn is_granted_invite(state: &AppState, invite_id: i64) -> AppResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM eunha.granted_invites WHERE invite_id = $1)",
    )
    .bind(invite_id)
    .fetch_one(&state.db)
    .await?)
}

/// Staff grants authorize admission; other codes use Mastodon's
/// `Invite#bypass_approval?`: `user&.role&.can?(:invite_bypass_approval)`.
///
/// The permission is the invite *creator's*, computed the way `UserRole` does
/// — the everyone role unioned in, and `administrator` granting everything —
/// so it answers for a member the same way `verify_credentials` does. An invite
/// whose creator has since gone is not a bypass; `validate_invite` has already
/// refused those, and treating a missing row as `false` keeps the failure in
/// the safe direction.
pub(crate) async fn invite_bypasses_approval(state: &AppState, invite_id: i64) -> bool {
    // Staff grants authorize admission irrespective of the holder's role.
    // Legacy and self-created codes keep Mastodon's role-based decision.
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM eunha.granted_invites WHERE invite_id = $1)",
    )
    .bind(invite_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(false)
    {
        return true;
    }
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

    if crate::api::mastodon::accounts::fetch_account(state, follower_account_id)
        .await
        .is_err()
    {
        return;
    }

    if inviter.locked {
        let inserted = sqlx::query!(
            r#"INSERT INTO follow_requests (account_id, target_account_id, uri, created_at, updated_at)
               VALUES ($1, $2, $3, now(), now())
               ON CONFLICT (account_id, target_account_id) DO NOTHING"#,
            follower_account_id,
            target_id,
            crate::federation::relationships::generate_uri(&state.instance.web_domain()),
        )
        .execute(&state.db)
        .await;
        if !matches!(inserted, Ok(r) if r.rows_affected() > 0) {
            return;
        }
        // `FollowService`: `ActivityTracker.increment('activity:interactions')`.
        crate::activity_tracker::increment(state, crate::activity_tracker::INTERACTIONS).await;
        crate::push::create_and_push(
            state,
            target_id,
            follower_account_id,
            "follow_request",
            None,
        )
        .await;
        return;
    }

    let inserted = sqlx::query!(
        r#"INSERT INTO follows (account_id, target_account_id, uri, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())
           ON CONFLICT (account_id, target_account_id) DO NOTHING"#,
        follower_account_id,
        target_id,
        crate::federation::relationships::generate_uri(&state.instance.web_domain()),
    )
    .execute(&state.db)
    .await;
    if !matches!(inserted, Ok(r) if r.rows_affected() > 0) {
        return;
    }
    // `FollowService`: `ActivityTracker.increment('activity:interactions')`.
    crate::activity_tracker::increment(state, crate::activity_tracker::INTERACTIONS).await;
    // `AccountStat`'s `update_index('accounts', :account)`.
    crate::search::elasticsearch::indexing::accounts(state, &[follower_account_id, target_id])
        .await;

    let _ = crate::counters::on_follow_created(state, follower_account_id, target_id).await;
    crate::push::create_and_push(state, target_id, follower_account_id, "follow", None).await;
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
    /// A staff-granted invite was used up while this sign-up was being
    /// saved; [`register`] retries, so no response is ever made of it.
    GrantedInviteUsedUp,
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
            Self::GrantedInviteUsedUp => {
                AppError::Unprocessable("This invite was just used. Please try again.".into())
                    .into_response()
            }
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

/// The account's validations of its username: present, not taken (whatever
/// the case), of the right characters and length, and not reserved.
async fn validate_username(
    state: &AppState,
    errors: &mut crate::email_subscriptions::ValidationErrors,
    username: &str,
) -> Result<(), sqlx::Error> {
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
        if crate::moderation::signup::username_blocked(state, username, false).await {
            let (attribute, label, key, message) =
                crate::moderation::signup::Refusal::UsernameReserved.detail();
            errors.add_as(attribute, label, key, message);
        }
    }
    Ok(())
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
#[derive(Clone)]
pub struct SignUpApp {
    pub id: i64,
    pub scopes: String,
}

/// `AppSignUpService#call` and `Auth::RegistrationsController#create`: the
/// user and its account saved together, unconfirmed, as `User.create!` saves
/// them — the account with its signing key, the user with its
/// `confirmation_token`, approval (`set_approved`), address and reason
/// (`user_invite_requests`), the invite's use counted (`increment_invite_uses`),
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
    // A staff-granted code that another sign-up used up between the check
    // and the insert: start again, and this time it is a code no longer good
    // for use, which Mastodon's rules then decide for exactly as they would
    // for someone who arrived after it was used.
    for _ in 0..2 {
        match register_once(state, instance, form, sign_up_ip, app.clone()).await {
            Err(SignupError::GrantedInviteUsedUp) => continue,
            result => return result,
        }
    }
    Err(AppError::Unprocessable("This invite was just used. Please try again.".into()).into())
}

async fn register_once(
    state: &AppState,
    instance: &crate::config::InstanceConfig,
    form: &ApiCreateAccountForm,
    sign_up_ip: Option<std::net::IpAddr>,
    app: Option<SignUpApp>,
) -> Result<Registered, SignupError> {
    if !state.growth_limits.registrations() {
        return Err(AppError::Unprocessable("Registrations are closed because this instance is over its user allowance. Its owner can review usage or upgrade the plan on eunha space.".into()).into());
    }
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
    // `locale` as given, none when not, for `I18n.default_locale` to stand
    // in; one Mastodon is not offered in becomes none as the user is saved.
    let locale = form.locale.clone().filter(|l| !l.is_empty());
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
    validate_username(state, &mut errors, &username).await?;

    // The invite request's text.
    validate_reason(
        &mut errors,
        form.reason.as_deref(),
        // `!invite&.bypass_approval?`: the invite the code names, whether
        // or not it is still good for use.
        reason_required(state, instance, invite_id).await,
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

    // `self.disabled ||= !approved && Setting.postpone_confirmation_emails`:
    // someone waiting for approval hears nothing until they are approved.
    let disabled =
        !approved && crate::settings::boolean(state, "postpone_confirmation_emails").await;

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
        state.instance.web_domain(),
        crate::accounts::NewLocalUser {
            username: &username,
            email: &email,
            password_hash: &password_hash,
            role_id: None,
            approved,
            disabled,
            invite_id,
            locale: locale.as_deref(),
            app_id: app.as_ref().map(|a| a.id),
            sign_up_ip,
            invite_request: reason,
            time_zone: time_zone.as_deref(),
            confirmed: false,
            // `mails_disabled?`: no link, and so no `confirmation_sent_at`.
            confirmation_token: (!disabled).then_some(confirmation_token.as_str()),
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
        // eunha's staff grants admit without review, so their `max_uses` is
        // an admission limit: check it again under the row's lock, where
        // a sign-up racing this one waits. Other codes are counted as
        // Mastodon's `increment_invite_uses` counts them, unconditionally.
        if valid_invitation && is_granted_invite(state, id).await? {
            let still_good: bool = sqlx::query_scalar(
                "SELECT (max_uses IS NULL OR uses < max_uses)
                        AND (expires_at IS NULL OR expires_at >= now())
                 FROM invites WHERE id = $1 FOR UPDATE",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            if !still_good {
                return Err(SignupError::GrantedInviteUsedUp);
            }
        }
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
async fn app_token(state: &AppState, token: Option<&str>) -> Option<(Option<i64>, String, i64)> {
    let token = token?;
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
    bearer: Option<Extension<crate::middleware::BearerToken>>,
    super::extractors::Params(form): super::extractors::Params<ApiCreateAccountForm>,
) -> Result<
    (
        [(axum::http::HeaderName, &'static str); 1],
        Json<super::types::Token>,
    ),
    SignupError,
> {
    // `doorkeeper_authorize!` and `require_client_credentials!`.
    let token = bearer.as_ref().map(|Extension(t)| t.0.as_str());
    let Some((owner, scopes, app_id)) = app_token(&state, token).await else {
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
    let token = registered
        .token
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("no token was issued")))?;
    // `deprecate_api '2026-10-01', only: :create`.
    Ok((
        [(
            axum::http::HeaderName::from_static("deprecation"),
            "@1790812800",
        )],
        Json(token),
    ))
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
/// when the link asks to go back to it (`redirect_to_app`); otherwise the
/// sign-in page with the confirmation result, even with a browser session,
/// from which signing in goes on into the web app.
pub async fn confirm_email(
    state: AppState,
    headers: axum::http::HeaderMap,
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
    let approved = sqlx::query_scalar!("SELECT approved FROM users WHERE id = $1", user.id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(false);
    if approved {
        // Signing in from here goes on into the web app (Mastodon's `start`),
        // unless the browser was already on its way somewhere, such as an
        // app's authorization page.
        let mut response = Redirect::to("/account/login?confirmed=1").into_response();
        if crate::api::account::stored_location(&headers).is_none() {
            if let Ok(cookie) = crate::api::account::return_to_cookie("/start").parse() {
                response
                    .headers_mut()
                    .append(axum::http::header::SET_COOKIE, cookie);
            }
        }
        response
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
    super::extractors::Params(form): super::extractors::Params<ResendConfirmationForm>,
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
    error: Option<&'static str>,
    invitation: Option<&Invitation>,
    locale: crate::locale::Locale,
    form: &FormRequirements,
) -> Response {
    // Only a code good for use goes along with the sign-up; with another, the
    // form is the one anyone would get.
    let invite = if invitation.is_some() { invite } else { "" };
    let enc_invite = urlencoding::encode(invite);
    let toggle_url = |lang: &str| {
        if invite.is_empty() {
            format!("/auth/signup?lang={lang}")
        } else {
            format!("/auth/signup?invite={enc_invite}&lang={lang}")
        }
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
    let invited_by = invitation.map(|i| {
        locale
            .t("invited_by")
            .replace("%{acct}", &format!("@{}", i.inviter))
    });
    let html = templates::render(
        "signup.html",
        minijinja::context! {
            instance_title => &instance.title,
            instance_domain => &instance.local_domain,
            show_form,
            post_lang => form.picked_language,
            ask_reason => form.ask_reason,
            invite,
            error,
            invited_by,
            invite_autofollow => invitation.is_some_and(|i| i.autofollow),
            lang => locale.as_str(),
            toggle_en_url => toggle_url("en"),
            toggle_ko_url => toggle_url("ko"),
            t_create_account => locale.t("create_account"),
            t_username => locale.t("username"),
            t_handle_hint => locale.t("handle_hint"),
            t_username_available => locale.t("username_available"),
            t_email => locale.t("email"),
            t_password => locale.t("password"),
            t_confirm_password => locale.t("confirm_password"),
            t_already_account => locale.t("already_account"),
            t_sign_in => locale.t("sign_in"),
            t_registrations_closed => locale.t("registrations_closed"),
            t_invite_code => locale.t("invite_code"),
            t_continue_btn => locale.t("continue_btn"),
            t_invite_autofollow => locale.t("invite_autofollow"),
            t_invite_review => locale.t("invite_review"),
            t_reason => locale.t("reason"),
            t_reason_hint => locale.t("reason_hint"),
            t_apply_for_account => locale.t("apply_for_account"),
            t_err_password_mismatch => locale.t("err_password_mismatch"),
            t_err_server => locale.t("err_server"),
            min_age => form.min_age,
            reason_required => form.reason_required,
            t_date_of_birth => locale.t("date_of_birth"),
            agreement_html,
        },
    );
    Html(html).into_response()
}

#[cfg(test)]
mod tests {
    use super::invite_code_from;

    #[test]
    fn a_pasted_invite_link_gives_its_code() {
        for (input, code) in [
            ("abcd1234", "abcd1234"),
            ("  abcd1234 ", "abcd1234"),
            ("https://example.com/invite/abcd1234", "abcd1234"),
            ("https://example.com/invite/abcd1234/", "abcd1234"),
            ("https://example.com/invite/abcd1234.json", "abcd1234"),
            ("/invite/abcd1234", "abcd1234"),
            (
                "https://example.com/auth/signup?invite=abcd1234",
                "abcd1234",
            ),
            (
                "https://example.com/auth/signup?lang=ko&invite=abcd1234#x",
                "abcd1234",
            ),
            (
                "https://example.com/signup?invite_code=abcd1234",
                "abcd1234",
            ),
        ] {
            assert_eq!(invite_code_from(input), code, "{input}");
        }
    }
}
