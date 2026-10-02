//! The steps of a web sign-in after the password, as
//! `Auth::TwoFactorAuthenticationConcern` and `Auth::SessionsController` take
//! them: the second factor when the user has one, and the two-factor setup
//! their role asks for when they have none.
//!
//! Mastodon keeps the pending sign-in in the session (`attempt_user_id` and
//! `attempt_user_updated_at`). Eunha's sign-in forms have no session, so the
//! pending sign-in lives in Redis under a random token the form carries, and
//! expires after an hour. As in Mastodon, a change to the user in between
//! (`users.updated_at`) sends the person back to the password.

use axum::{
    extract::Form,
    response::{Html, IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{
    locale::Locale,
    state::AppState,
    two_factor::{self, NoticeKind, OwnedNotice, UserTwoFactor},
};

/// How long a pending sign-in lasts.
const ATTEMPT_TTL_SECONDS: u64 = 3600;

/// Where a sign-in goes once it is done.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Continuation {
    /// The OAuth authorization form: a grant for this client.
    Oauth {
        client_id: String,
        redirect_uri: String,
        scope: String,
        lang: String,
    },
    /// The account pages: a browser session.
    Account,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
enum Stage {
    /// Waiting for a second factor.
    Challenge,
    /// Waiting for the first code from a new authenticator, the setup the
    /// user's role requires; `secret` is Mastodon's `session[:new_otp_secret]`.
    Setup { secret: String },
    /// Setup done and the recovery codes shown; waiting to carry on.
    RecoveryCodes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Attempt {
    user_id: i64,
    updated_at: String,
    #[serde(flatten)]
    stage: Stage,
    webauthn_challenge: Option<String>,
    continuation: Continuation,
}

fn attempt_key(state: &AppState, token: &str) -> String {
    state.redis_keys.key(format!("sign_in_attempt:{token}"))
}

async fn store(state: &AppState, token: &str, attempt: &Attempt) -> bool {
    let Ok(value) = serde_json::to_string(attempt) else {
        return false;
    };
    let mut redis = state.redis_coordination.clone();
    let stored: redis::RedisResult<()> = redis::cmd("SET")
        .arg(attempt_key(state, token))
        .arg(value)
        .arg("EX")
        .arg(ATTEMPT_TTL_SECONDS)
        .query_async(&mut redis)
        .await;
    if let Err(error) = &stored {
        tracing::warn!(%error, "could not store a pending sign-in");
    }
    stored.is_ok()
}

async fn load(state: &AppState, token: &str) -> Option<Attempt> {
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut redis = state.redis_coordination.clone();
    let value: Option<String> = redis::cmd("GET")
        .arg(attempt_key(state, token))
        .query_async(&mut redis)
        .await
        .ok()
        .flatten();
    serde_json::from_str(&value?).ok()
}

async fn discard(state: &AppState, token: &str) {
    let mut redis = state.redis_coordination.clone();
    let _: redis::RedisResult<()> = redis::cmd("DEL")
        .arg(attempt_key(state, token))
        .query_async(&mut redis)
        .await;
}

fn stamp(user: &UserTwoFactor) -> String {
    user.updated_at.and_utc().timestamp_micros().to_string()
}

/// What a sign-in page shows next.
pub struct Page {
    token: String,
    mode: &'static str,
    continuation: Continuation,
    error: Option<&'static str>,
    notice: Option<&'static str>,
    webauthn_enabled: bool,
    secret: Option<String>,
    /// The email address the authenticator app files the secret under.
    account: String,
    recovery_codes: Vec<String>,
}

/// Where a sign-in stands after a step.
pub enum Step {
    /// Done: carry on to `Continuation`.
    SignedIn(i64, Continuation),
    /// Show another page of the sign-in.
    Render(Box<Page>),
    /// Start again from the password, with `devise.failure.timeout`.
    Restart(Continuation),
    /// Something broke that the person cannot fix.
    Failed(Continuation),
}

/// After a good password: sign in, or ask for what comes first.
pub async fn after_password(
    state: &AppState,
    user_id: i64,
    continuation: Continuation,
    ip: Option<std::net::IpAddr>,
    user_agent: Option<&str>,
) -> Step {
    let Ok(Some(user)) = two_factor::load(&state.db, user_id).await else {
        return Step::Failed(continuation);
    };
    if user.enabled() {
        // `prompt_for_two_factor`.
        let token = crate::crypto::generate_token(32);
        let attempt = Attempt {
            user_id,
            updated_at: stamp(&user),
            stage: Stage::Challenge,
            webauthn_challenge: None,
            continuation: continuation.clone(),
        };
        if !store(state, &token, &attempt).await {
            return Step::Failed(continuation);
        }
        return Step::Render(Box::new(Page {
            token,
            mode: "challenge",
            continuation,
            error: None,
            notice: None,
            webauthn_enabled: user.webauthn_enabled(),
            secret: None,
            account: String::new(),
            recovery_codes: Vec::new(),
        }));
    }
    on_authentication_success(state, &user, "password", ip, user_agent).await;
    if user.missing() {
        return begin_setup(state, &user, continuation).await;
    }
    Step::SignedIn(user_id, continuation)
}

/// `require_functional!` sending a user whose role requires two-factor
/// authentication to set it up, before anything else.
async fn begin_setup(state: &AppState, user: &UserTwoFactor, continuation: Continuation) -> Step {
    if state.encryptor.is_none() {
        // Without the encryption keys no secret can be stored, so the
        // requirement cannot be met here.
        return Step::Failed(continuation);
    }
    let token = crate::crypto::generate_token(32);
    let secret = two_factor::generate_otp_secret();
    let attempt = Attempt {
        user_id: user.user_id,
        updated_at: stamp(user),
        stage: Stage::Setup {
            secret: secret.clone(),
        },
        webauthn_challenge: None,
        continuation: continuation.clone(),
    };
    if !store(state, &token, &attempt).await {
        return Step::Failed(continuation);
    }
    Step::Render(Box::new(Page {
        token,
        mode: "setup",
        continuation,
        error: None,
        notice: None,
        webauthn_enabled: false,
        secret: Some(secret),
        account: user.email.clone(),
        recovery_codes: Vec::new(),
    }))
}

/// What the form posted for a pending sign-in.
#[derive(Debug, Default, Deserialize)]
pub struct Submitted {
    pub attempt: Option<String>,
    pub otp_attempt: Option<String>,
    pub credential: Option<String>,
    pub setup_otp_attempt: Option<String>,
    pub resume: Option<String>,
}

impl Submitted {
    /// Whether this is a step of a pending sign-in rather than a password.
    pub fn is_attempt(&self) -> bool {
        self.attempt.as_deref().is_some_and(|a| !a.is_empty())
    }
}

/// Take the next step of the pending sign-in the form names.
pub async fn continue_attempt(
    state: &AppState,
    submitted: &Submitted,
    fallback: Continuation,
    ip: Option<std::net::IpAddr>,
    user_agent: Option<&str>,
) -> Step {
    let token = submitted.attempt.clone().unwrap_or_default();
    let Some(mut attempt) = load(state, &token).await else {
        return Step::Restart(fallback);
    };
    let continuation = attempt.continuation.clone();
    let Ok(Some(user)) = two_factor::load(&state.db, attempt.user_id).await else {
        discard(state, &token).await;
        return Step::Restart(continuation);
    };

    match attempt.stage.clone() {
        Stage::RecoveryCodes => {
            if submitted.resume.is_some() {
                discard(state, &token).await;
                return Step::SignedIn(user.user_id, continuation);
            }
            Step::Restart(continuation)
        }
        Stage::Setup { secret } => {
            if attempt.updated_at != stamp(&user) {
                discard(state, &token).await;
                return Step::Restart(continuation);
            }
            let code = submitted.setup_otp_attempt.as_deref().unwrap_or("");
            let page = |error: Option<&'static str>| Page {
                token: token.clone(),
                mode: "setup",
                continuation: continuation.clone(),
                error,
                notice: None,
                webauthn_enabled: false,
                secret: Some(secret.clone()),
                account: user.email.clone(),
                recovery_codes: Vec::new(),
            };
            if !two_factor::validate_and_consume_otp(state, &user, code, Some(&secret)).await {
                return Step::Render(Box::new(page(Some("otp_wrong_code"))));
            }
            let codes = match two_factor::enable(state, user.user_id, &secret).await {
                Ok(codes) => codes,
                Err(error) => {
                    tracing::error!(%error, "could not enable two-factor authentication");
                    return Step::Failed(continuation);
                }
            };
            two_factor::notify(
                state,
                &user,
                NoticeKind::Security(OwnedNotice::TwoFactorEnabled),
            );
            attempt.stage = Stage::RecoveryCodes;
            store(state, &token, &attempt).await;
            Step::Render(Box::new(Page {
                token,
                mode: "recovery_codes",
                continuation,
                error: None,
                notice: Some("two_factor_enabled_success"),
                webauthn_enabled: false,
                secret: None,
                account: String::new(),
                recovery_codes: codes,
            }))
        }
        Stage::Challenge => {
            if attempt.updated_at != stamp(&user) {
                // `restart_session`.
                discard(state, &token).await;
                return Step::Restart(continuation);
            }
            let retry = |error: &'static str| {
                Step::Render(Box::new(Page {
                    token: token.clone(),
                    mode: "challenge",
                    continuation: continuation.clone(),
                    error: Some(error),
                    notice: None,
                    webauthn_enabled: user.webauthn_enabled(),
                    secret: None,
                    account: String::new(),
                    recovery_codes: Vec::new(),
                }))
            };
            if let (true, Some(raw)) = (
                user.webauthn_enabled(),
                submitted.credential.as_deref().filter(|c| !c.is_empty()),
            ) {
                // `authenticate_with_two_factor_via_webauthn`.
                if valid_webauthn_credential(
                    state,
                    &user,
                    raw,
                    attempt.webauthn_challenge.as_deref(),
                )
                .await
                {
                    discard(state, &token).await;
                    on_authentication_success(state, &user, "webauthn", ip, user_agent).await;
                    return Step::SignedIn(user.user_id, continuation);
                }
                on_authentication_failure(
                    state,
                    &user,
                    "webauthn",
                    "invalid_credential",
                    ip,
                    user_agent,
                )
                .await;
                return retry("invalid_security_key");
            }
            let Some(code) = submitted.otp_attempt.as_deref() else {
                return retry("");
            };
            // `authenticate_with_two_factor_via_otp`.
            if two_factor::second_factor_rate_limited(state, user.user_id).await {
                return retry("rate_limited");
            }
            let valid = two_factor::validate_and_consume_otp(state, &user, code, None).await
                || two_factor::invalidate_backup_code(state, &user, code).await;
            if valid {
                discard(state, &token).await;
                on_authentication_success(state, &user, "otp", ip, user_agent).await;
                return Step::SignedIn(user.user_id, continuation);
            }
            on_authentication_failure(state, &user, "otp", "invalid_otp_token", ip, user_agent)
                .await;
            retry("invalid_otp_token")
        }
    }
}

/// `valid_webauthn_credential?`.
async fn valid_webauthn_credential(
    state: &AppState,
    user: &UserTwoFactor,
    raw: &str,
    challenge: Option<&str>,
) -> bool {
    let Some(challenge) = challenge else {
        return false;
    };
    let Ok(credential) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    let Ok(id) = crate::webauthn::credential_id(&credential) else {
        return false;
    };
    let Ok(Some(stored)) = sqlx::query!(
        r#"SELECT id, public_key, sign_count FROM webauthn_credentials
           WHERE user_id = $1 AND external_id = $2"#,
        user.user_id,
        id,
    )
    .fetch_optional(&state.db)
    .await
    else {
        return false;
    };
    let rp = crate::webauthn::RelyingParty::for_domain(&state.instance.domain);
    match crate::webauthn::verify_assertion(
        &credential,
        challenge,
        &rp,
        &stored.public_key,
        stored.sign_count,
    ) {
        Ok(sign_count) => sqlx::query!(
            "UPDATE webauthn_credentials SET sign_count = $1, updated_at = now() WHERE id = $2",
            sign_count,
            stored.id,
        )
        .execute(&state.db)
        .await
        .is_ok(),
        Err(_) => false,
    }
}

/// `on_authentication_success`, as far as eunha's sign-in needs it: the
/// second-factor attempts forgotten, the sign-in recorded, and the user told
/// if it looks like someone else (`SuspiciousSignInDetector`).
async fn on_authentication_success(
    state: &AppState,
    user: &UserTwoFactor,
    method: &str,
    ip: Option<std::net::IpAddr>,
    user_agent: Option<&str>,
) {
    two_factor::clear_second_factor_attempts(state, user.user_id).await;
    let suspicious = suspicious_sign_in(state, user, ip).await;
    crate::accounts::record_login(&state.db, user.user_id, ip, user_agent, method, true, None)
        .await;
    if suspicious {
        let email = state.email.clone();
        let to = user.email.clone();
        let domain = state.instance.domain.clone();
        let ip = ip.map(|ip| ip.to_string()).unwrap_or_default();
        let browser = crate::browser_detection::describe(user_agent.unwrap_or(""));
        let time_zone = user.time_zone.clone();
        crate::tenants::spawn(async move {
            if let Err(error) = email
                .send_sign_in_alert(
                    &to,
                    &domain,
                    true,
                    &ip,
                    &browser,
                    chrono::Utc::now(),
                    time_zone.as_deref(),
                )
                .await
            {
                tracing::warn!(%error, "could not send a suspicious sign-in email");
            }
        });
    }
}

/// `on_authentication_failure`: recorded, and `UserMailer.failed_2fa`.
async fn on_authentication_failure(
    state: &AppState,
    user: &UserTwoFactor,
    method: &str,
    reason: &str,
    ip: Option<std::net::IpAddr>,
    user_agent: Option<&str>,
) {
    crate::accounts::record_login(
        &state.db,
        user.user_id,
        ip,
        user_agent,
        method,
        false,
        Some(reason),
    )
    .await;
    two_factor::notify_failed_second_factor(state, user, ip, user_agent).await;
}

/// `SuspiciousSignInDetector#suspicious?`: no TOTP, not the first sign-in,
/// and no sign-in or session from nearby before (the same /16 for IPv4, /64
/// for IPv6).
async fn suspicious_sign_in(
    state: &AppState,
    user: &UserTwoFactor,
    ip: Option<std::net::IpAddr>,
) -> bool {
    let Some(ip) = ip else {
        return false;
    };
    if user.otp_required_for_login {
        return false;
    }
    let prefix = if ip.is_ipv6() { 64 } else { 16 };
    let Ok(network) = ipnet::IpNet::new(ip, prefix) else {
        return false;
    };
    let network = network.trunc().to_string();
    sqlx::query_scalar!(
        r#"SELECT u.current_sign_in_at IS NOT NULL
                  AND NOT EXISTS (SELECT 1 FROM user_ips i
                                  WHERE i.user_id = u.id AND i.ip <<= $2::text::inet)
           AS "suspicious!"
           FROM users u WHERE u.id = $1"#,
        user.user_id,
        network,
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(false)
}

/// Render a page of the pending sign-in.
pub fn render(state: &AppState, locale: Locale, page: &Page) -> Response {
    let (action, oauth) = match &page.continuation {
        Continuation::Oauth {
            client_id,
            redirect_uri,
            scope,
            lang,
        } => (
            "/oauth/authorize",
            Some(minijinja::context! {
                client_id, redirect_uri, scope, lang,
            }),
        ),
        Continuation::Account => ("/account/login", None),
    };
    let domain = state.instance.domain.as_str();
    let rp = crate::webauthn::RelyingParty::for_domain(domain);
    let qr_code = page.secret.as_deref().map(|secret| {
        two_factor::qr_code_svg(&two_factor::provisioning_uri(secret, &page.account, &rp.id))
    });
    let html = crate::templates::render(
        "two_factor.html",
        minijinja::context! {
            lang => locale.as_str(),
            domain,
            mode => page.mode,
            action,
            oauth,
            attempt => &page.token,
            error => page.error.map(|key| locale.t(key)).filter(|t| !t.is_empty()),
            notice => page.notice.map(|key| locale.t(key)),
            webauthn_enabled => page.webauthn_enabled,
            secret => &page.secret,
            qr_code,
            recovery_codes => &page.recovery_codes,
            role_requirement => locale.t("two_factor_role_requirement").replace("%{domain}", domain),
            t_two_factor_title => locale.t("two_factor_title"),
            t_otp_hint => locale.t("otp_hint"),
            t_otp_attempt => locale.t("otp_attempt"),
            t_sign_in => locale.t("sign_in"),
            t_link_to_webauthn => locale.t("link_to_webauthn"),
            t_webauthn_title => locale.t("webauthn_title"),
            t_webauthn_hint => locale.t("webauthn_hint"),
            t_webauthn_not_supported => locale.t("webauthn_not_supported"),
            t_invalid_security_key => locale.t("invalid_security_key"),
            t_use_security_key => locale.t("use_security_key"),
            t_link_to_otp => locale.t("link_to_otp"),
            t_otp_instructions => locale.t("otp_instructions"),
            t_otp_manual_instructions => locale.t("otp_manual_instructions"),
            t_otp_code_hint => locale.t("otp_code_hint"),
            t_otp_enable => locale.t("otp_enable"),
            t_recovery_instructions => locale.t("recovery_instructions"),
            t_resume_app_authorization => locale.t("resume_app_authorization"),
            t_continue => locale.t("continue"),
        },
    );
    Html(html).into_response()
}

#[derive(Debug, Deserialize)]
pub struct SecurityKeyOptionsForm {
    pub attempt: String,
}

/// `Auth::Sessions::SecurityKeyOptionsController#show`, for the pending
/// sign-in the form names. A POST rather than Mastodon's GET, so the pending
/// sign-in's token stays out of URLs.
pub async fn security_key_options(
    state: AppState,
    Form(form): Form<SecurityKeyOptionsForm>,
) -> Response {
    let not_enabled = || {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "You haven't enabled WebAuthn yet" })),
        )
            .into_response()
    };
    let Some(mut attempt) = load(&state, &form.attempt).await else {
        return not_enabled();
    };
    if !matches!(attempt.stage, Stage::Challenge) {
        return not_enabled();
    }
    let allow: Vec<String> = sqlx::query_scalar!(
        "SELECT external_id FROM webauthn_credentials WHERE user_id = $1 ORDER BY id",
        attempt.user_id,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    if allow.is_empty() {
        return not_enabled();
    }
    let challenge = crate::webauthn::generate_challenge();
    attempt.webauthn_challenge = Some(challenge.clone());
    store(&state, &form.attempt, &attempt).await;
    Json(crate::webauthn::request_options(&challenge, &allow)).into_response()
}
