use axum::{
    extract::Query,
    http::{header, HeaderMap, HeaderName, HeaderValue},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Router,
};
use serde::Deserialize;

pub mod password_reset;
pub mod sign_in;

use crate::{
    crypto::{hash_password, verify_password},
    locale::Locale,
    middleware::ResolvedInstance,
    state::AppState,
    templates,
};

const COOKIE_NAME: &str = "account_session";
/// A year, as Mastodon's `_session_id` cookie lasts.
const COOKIE_MAX_AGE: u32 = 31_536_000;

pub fn router() -> Router {
    Router::new()
        .route("/account", get(account_home))
        .route("/account/login", get(login_page).post(login_post))
        .route("/auth", post(registration_post))
        .route(
            "/auth/setup",
            get(setup_page).post(setup_post).put(setup_post),
        )
        .route("/account/logout", post(logout_post))
        .route("/account/sso", post(sso_post))
        .route("/backups/{id}/download", get(backup_download))
        .route("/account/password", get(password_page).post(password_post))
        .route("/account/delete", get(delete_page).post(delete_post))
        .route(
            "/auth/sessions/security_key_options",
            post(sign_in::security_key_options),
        )
        .route("/auth/password/new", get(password_reset::new_page))
        .route(
            "/auth/password",
            post(password_reset::request).put(password_reset::update),
        )
        .route(
            "/auth/password/edit",
            get(password_reset::edit_page).post(password_reset::update),
        )
        .route(
            "/auth/password/reset",
            axum::routing::put(password_reset::update),
        )
}

// ── Session lookup ─────────────────────────────────────────────────────────────

struct AccountSession {
    user_id: i64,
    username: String,
    /// `session_activations.id`, Mastodon's `current_session`.
    activation_id: i64,
}

fn extract_session_token(headers: &HeaderMap) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(val) = part.strip_prefix(&format!("{COOKIE_NAME}=")) {
            return Some(val.to_string());
        }
    }
    None
}

type ClientIpExt = Option<axum::extract::Extension<crate::remote_ip::ClientIp>>;

fn client_addr(client_ip: ClientIpExt) -> Option<std::net::IpAddr> {
    client_ip.and_then(|axum::extract::Extension(c)| c.0)
}

/// The signed-in user behind the session cookie: Mastodon's
/// `SessionActivationRememberable` strategy and `after_fetch` hook.
async fn get_session(
    headers: &HeaderMap,
    state: &AppState,
    ip: Option<std::net::IpAddr>,
) -> Option<AccountSession> {
    let session_id = extract_session_token(headers)?;
    let session = crate::sessions::fetch(&state.db, &session_id, ip).await?;
    let row = sqlx::query!(
        r#"SELECT u.id as user_id, a.username
           FROM users u
           JOIN accounts a ON a.id = u.account_id
           WHERE u.id = $1
             AND u.disabled = false
             AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
             AND a.domain IS NULL"#,
        session.user_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()??;

    Some(AccountSession {
        user_id: row.user_id,
        username: row.username,
        activation_id: session.id,
    })
}

fn set_cookie(token: &str) -> String {
    format!("{COOKIE_NAME}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={COOKIE_MAX_AGE}")
}

fn clear_cookie() -> &'static str {
    "account_session=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"
}

fn accept_language(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
}

fn is_htmx(headers: &HeaderMap) -> bool {
    headers.get("HX-Request").and_then(|v| v.to_str().ok()) == Some("true")
}

// ── GET /account ───────────────────────────────────────────────────────────────

pub async fn account_home(
    state: AppState,
    axum::extract::Extension(ResolvedInstance(instance)): axum::extract::Extension<
        ResolvedInstance,
    >,
    client_ip: ClientIpExt,
    headers: HeaderMap,
) -> Response {
    let locale = Locale::detect(None, accept_language(&headers));

    let Some(session) = get_session(&headers, &state, client_addr(client_ip)).await else {
        return Redirect::to("/account/login").into_response();
    };
    // `require_functional!`: an unconfirmed user is sent to confirm.
    if !user_confirmed(&state, session.user_id).await {
        return Redirect::to("/auth/setup").into_response();
    }

    let domain = instance.domain.clone();

    let html = templates::render(
        "account_home.html",
        minijinja::context! {
            lang => locale.as_str(),
            domain,
            username => session.username,
            t_account => locale.t("account"),
            t_change_password => locale.t("change_password"),
            t_sign_out => locale.t("sign_out"),
            t_go_to_timeline => locale.t("go_to_timeline"),
            t_delete_account => locale.t("delete_account"),
        },
    );
    Html(html).into_response()
}

// ── GET /account/login ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    /// Set by the account-deletion redirect to show Mastodon's `success_msg`.
    pub deleted: Option<String>,
    /// Set by the email-confirmation redirect: `1` once the account is live,
    /// `pending` when it still waits on an admin, `invalid` when the link had
    /// already been used or had expired. The sign-in form is what the person
    /// needs next in every one of those cases, so they all land here.
    pub confirmed: Option<String>,
}

pub async fn login_page(
    axum::extract::Extension(ResolvedInstance(instance)): axum::extract::Extension<
        ResolvedInstance,
    >,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    let locale = Locale::detect(None, accept_language(&headers));
    let domain = instance.domain.clone();

    let (notice, error) = match query.confirmed.as_deref() {
        Some("1") => (locale.t("confirm_success"), ""),
        Some("pending") => (locale.t("pending_approval"), ""),
        Some("invalid") => ("", locale.t("confirm_invalid")),
        _ if query.deleted.as_deref() == Some("1") => (locale.t("delete_success"), ""),
        _ => ("", ""),
    };

    let html = templates::render(
        "account_login.html",
        minijinja::context! {
            lang => locale.as_str(),
            domain,
            error,
            notice,
            t_email => locale.t("email"),
            t_password => locale.t("password"),
            t_sign_in => locale.t("sign_in"),
            t_account => locale.t("account"),
            t_forgot_password => locale.t("forgot_password"),
        },
    );
    Html(html).into_response()
}

// ── POST /account/login ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    pub email: Option<String>,
    pub password: Option<String>,
    #[serde(flatten)]
    pub step: sign_in::Submitted,
}

/// `Auth::SessionsController#create`: the password, then whatever
/// [`sign_in`] asks for, then a session.
pub async fn login_post(
    state: AppState,
    axum::extract::Extension(ResolvedInstance(instance)): axum::extract::Extension<
        ResolvedInstance,
    >,
    client_ip: ClientIpExt,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let locale = Locale::detect(None, accept_language(&headers));
    let domain = instance.domain.clone();
    let htmx = is_htmx(&headers);
    let ip = client_addr(client_ip);
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok());

    let render_error = |error: &'static str| -> Response {
        if htmx {
            return Html(format!("<div class=\"error\">{error}</div>")).into_response();
        }
        let html = templates::render(
            "account_login.html",
            minijinja::context! {
                lang => locale.as_str(),
                domain => domain.clone(),
                error,
                t_email => locale.t("email"),
                t_password => locale.t("password"),
                t_sign_in => locale.t("sign_in"),
                t_account => locale.t("account"),
                t_forgot_password => locale.t("forgot_password"),
            },
        );
        Html(html).into_response()
    };

    let step = if form.step.is_attempt() {
        sign_in::continue_attempt(
            &state,
            &form.step,
            sign_in::Continuation::Account,
            ip,
            user_agent,
        )
        .await
    } else {
        let email = form.email.as_deref().unwrap_or("").trim();
        let row = match sqlx::query!(
            r#"SELECT u.id, u.encrypted_password
               FROM users u
               JOIN accounts a ON a.id = u.account_id
               WHERE lower(u.email) = lower($1)
                 AND u.disabled = false
                 AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
                 AND a.domain IS NULL"#,
            email,
        )
        .fetch_optional(&state.db)
        .await
        {
            Ok(Some(r)) if !r.encrypted_password.is_empty() => r,
            _ => return render_error(locale.t("invalid_credentials")),
        };
        if verify_password(
            form.password.as_deref().unwrap_or(""),
            &row.encrypted_password,
        )
        .await
        .is_err()
        {
            crate::accounts::record_login(
                &state.db,
                row.id,
                ip,
                user_agent,
                "password",
                false,
                Some("invalid"),
            )
            .await;
            return render_error(locale.t("invalid_credentials"));
        }
        sign_in::after_password(
            &state,
            row.id,
            sign_in::Continuation::Account,
            ip,
            user_agent,
        )
        .await
    };

    let user_id = match step {
        sign_in::Step::SignedIn(user_id, _) => user_id,
        sign_in::Step::Render(page) => {
            let page = sign_in::render(&state, locale, &page);
            if htmx {
                // The form swaps into a message box; the next step is a page.
                let mut h = HeaderMap::new();
                h.insert(
                    HeaderName::from_static("hx-retarget"),
                    HeaderValue::from_static("body"),
                );
                h.insert(
                    HeaderName::from_static("hx-reswap"),
                    HeaderValue::from_static("innerHTML"),
                );
                return (h, page).into_response();
            }
            return page;
        }
        sign_in::Step::Restart(_) => return render_error(locale.t("session_timeout")),
        sign_in::Step::Failed(_) => return render_error(locale.t("err_server")),
    };

    // `sign_in(user)`, and the session it activates.
    let session_id = match crate::sessions::activate(&state.db, user_id, ip, user_agent).await {
        Ok(id) => id,
        Err(error) => {
            tracing::error!(%error, "could not activate a session");
            return render_error(locale.t("err_server"));
        }
    };

    // `after_sign_in_path_for`: where the browser was sent here from, when
    // it was, and the account page otherwise — unless the address is still
    // unconfirmed, where `require_functional!` sends the user on to
    // `auth/setup`.
    let stored = stored_location(&headers);
    let target = if user_confirmed(&state, user_id).await {
        stored.clone().unwrap_or_else(|| "/account".to_owned())
    } else {
        "/auth/setup".to_owned()
    };
    let mut h = HeaderMap::new();
    h.append(header::SET_COOKIE, set_cookie(&session_id).parse().unwrap());
    if stored.is_some() {
        h.append(
            header::SET_COOKIE,
            HeaderValue::from_static(CLEAR_RETURN_TO),
        );
    }
    if htmx {
        if let Ok(value) = HeaderValue::from_str(&target) {
            h.insert(HeaderName::from_static("hx-redirect"), value);
        }
        return (h, "").into_response();
    }
    (h, Redirect::to(&target)).into_response()
}

// ── Stored location ───────────────────────────────────────────────────────────

/// Devise's `user_return_to`: the page a signed-out browser asked for, kept
/// until it has signed in.
const RETURN_TO_COOKIE: &str = "account_return_to";
const CLEAR_RETURN_TO: &str = "account_return_to=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0";

/// The stored location, if it is a path on this site.
fn stored_location(headers: &HeaderMap) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    let value = cookie_header
        .split(';')
        .find_map(|part| part.trim().strip_prefix(&format!("{RETURN_TO_COOKIE}=")))?;
    let path = urlencoding::decode(value).ok()?.into_owned();
    (path.starts_with('/') && !path.starts_with("//") && !path.contains('\\')).then_some(path)
}

/// `authenticate_user!` failing: Devise's failure app remembers the page
/// (`store_location_for`) and redirects to the sign-in page with a `302`.
fn redirect_to_sign_in(path: &str) -> Response {
    let cookie = format!(
        "{RETURN_TO_COOKIE}={}; HttpOnly; SameSite=Lax; Path=/",
        urlencoding::encode(path)
    );
    (
        axum::http::StatusCode::FOUND,
        [
            (header::LOCATION, "/account/login".to_owned()),
            (header::SET_COOKIE, cookie),
        ],
    )
        .into_response()
}

// ── GET /backups/{id}/download ────────────────────────────────────────────────

/// `BackupsController#download`. `authenticate_user!` lets in any signed-in
/// user whose account is not a memorial (`active_for_authentication?`),
/// functional or not, so a suspended member can still take their archive
/// away. The archive is looked up among the user's own
/// (`current_user.backups.find`), and the browser is redirected to a link
/// to the file that works for `BACKUP_LINK_TIMEOUT`.
pub async fn backup_download(
    state: AppState,
    client_ip: ClientIpExt,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let session = match extract_session_token(&headers) {
        Some(session_id) => {
            crate::sessions::fetch(&state.db, &session_id, client_addr(client_ip)).await
        }
        None => None,
    };
    let account_id = match session {
        Some(session) => sqlx::query_scalar!(
            r#"SELECT u.account_id FROM users u JOIN accounts a ON a.id = u.account_id
               WHERE u.id = $1 AND a.domain IS NULL AND NOT a.memorial"#,
            session.user_id,
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten(),
        None => None,
    };
    let Some(account_id) = account_id else {
        return redirect_to_sign_in(&format!("/backups/{id}/download"));
    };
    let Ok(backup_id) = id.parse::<i64>() else {
        return crate::error::AppError::NotFound.into_response();
    };
    match crate::portability::backup::download_url(&state, account_id, backup_id).await {
        Ok(url) => (axum::http::StatusCode::FOUND, [(header::LOCATION, url)]).into_response(),
        Err(error) => error.into_response(),
    }
}

// ── POST /account/sso ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SsoForm {
    pub token: String,
}

/// Turn the web client's token into a session on these pages.
pub async fn sso_post(
    state: AppState,
    client_ip: ClientIpExt,
    headers: HeaderMap,
    Form(form): Form<SsoForm>,
) -> Response {
    let token = sqlx::query!(
        r#"SELECT t.id, t.resource_owner_id AS "user_id!"
           FROM oauth_access_tokens t
           JOIN users u ON u.id = t.resource_owner_id
           JOIN accounts a ON a.id = u.account_id
           WHERE t.token = $1
             AND t.revoked_at IS NULL
             AND (t.expires_in IS NULL OR t.created_at + t.expires_in * interval '1 second' > now())
             AND a.domain IS NULL"#,
        form.token,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let Some(token) = token else {
        return Redirect::to("/account/login").into_response();
    };
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    let session_id = match crate::sessions::activate_with_token(
        &state.db,
        token.user_id,
        token.id,
        client_addr(client_ip),
        user_agent,
    )
    .await
    {
        Ok(id) => id,
        Err(_) => return Redirect::to("/account/login").into_response(),
    };

    (
        [(header::SET_COOKIE, set_cookie(&session_id))],
        Redirect::to("/account"),
    )
        .into_response()
}

// ── POST /account/logout ───────────────────────────────────────────────────────

/// `Warden::Manager.before_logout`: the session deactivated.
pub async fn logout_post(state: AppState, headers: HeaderMap) -> Response {
    if let Some(session_id) = extract_session_token(&headers) {
        if let Ok(tokens) = crate::sessions::deactivate(&state.db, &session_id).await {
            crate::sessions::kill_streams(&state, tokens).await;
        }
    }

    if is_htmx(&headers) {
        // Client JS (hx-on::after-request) clears Elk IDB/localStorage and redirects.
        return ([(header::SET_COOKIE, clear_cookie())], "").into_response();
    }

    // Non-HTMX fallback: inline JS page.
    let html = r#"<!doctype html><html><head><meta charset="utf-8"></head><body><script>
Object.keys(localStorage).filter(k=>k.startsWith('elk-')).forEach(k=>localStorage.removeItem(k));
var r=indexedDB.open('keyval-store');
r.onsuccess=function(e){var t=e.target.result.transaction('keyval','readwrite');t.objectStore('keyval').delete('elk-users');t.oncomplete=go;t.onerror=go};
r.onerror=go;
function go(){location.replace('/')}
</script></body></html>"#;

    ([(header::SET_COOKIE, clear_cookie())], Html(html)).into_response()
}

// ── GET /account/password ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PasswordQuery {
    pub ok: Option<String>,
    pub err: Option<String>,
    pub mismatch: Option<String>,
}

pub async fn password_page(
    state: AppState,
    axum::extract::Extension(ResolvedInstance(instance)): axum::extract::Extension<
        ResolvedInstance,
    >,
    client_ip: ClientIpExt,
    headers: HeaderMap,
    Query(query): Query<PasswordQuery>,
) -> Response {
    let locale = Locale::detect(None, accept_language(&headers));

    let Some(_session) = get_session(&headers, &state, client_addr(client_ip)).await else {
        return Redirect::to("/account/login").into_response();
    };

    let domain = instance.domain.clone();
    let ok = query.ok.as_deref() == Some("1");
    let err = query.err.as_deref() == Some("1");
    let mismatch = query.mismatch.as_deref() == Some("1");

    let html = templates::render(
        "account_password.html",
        minijinja::context! {
            lang => locale.as_str(),
            domain,
            ok,
            err,
            mismatch,
            t_account => locale.t("account"),
            t_change_password => locale.t("change_password"),
            t_current_password => locale.t("current_password"),
            t_new_password => locale.t("new_password"),
            t_confirm_password => locale.t("confirm_new_password"),
            t_sign_out => locale.t("sign_out"),
            t_back_to_account => locale.t("back_to_account"),
            t_password_changed => locale.t("password_changed"),
            t_password_error => locale.t("password_error"),
            t_password_mismatch => locale.t("password_mismatch"),
        },
    );
    Html(html).into_response()
}

// ── POST /account/password ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PasswordForm {
    pub current_password: String,
    pub new_password: String,
    pub new_password_confirm: String,
}

pub async fn password_post(
    state: AppState,
    axum::extract::Extension(ResolvedInstance(_instance)): axum::extract::Extension<
        ResolvedInstance,
    >,
    client_ip: ClientIpExt,
    headers: HeaderMap,
    Form(form): Form<PasswordForm>,
) -> Response {
    let locale = Locale::detect(None, accept_language(&headers));
    let htmx = is_htmx(&headers);

    macro_rules! err {
        ($msg:expr, $url:expr) => {{
            if htmx {
                return Html(format!("<div class=\"error\">{}</div>", $msg)).into_response();
            }
            return Redirect::to($url).into_response();
        }};
    }

    let Some(session) = get_session(&headers, &state, client_addr(client_ip)).await else {
        return Redirect::to("/account/login").into_response();
    };

    if form.new_password != form.new_password_confirm {
        err!(
            locale.t("password_mismatch"),
            "/account/password?mismatch=1"
        );
    }

    // Devise's `password_length`, 8 to 72 characters.
    if crate::accounts::password_problem(&form.new_password, None).is_some() {
        err!(locale.t("password_error"), "/account/password?err=1");
    }

    let row = match sqlx::query!(
        "SELECT encrypted_password FROM users WHERE id = $1",
        session.user_id,
    )
    .fetch_one(&state.db)
    .await
    {
        Ok(r) => r,
        Err(_) => err!(locale.t("password_error"), "/account/password?err=1"),
    };

    if verify_password(&form.current_password, &row.encrypted_password)
        .await
        .is_err()
    {
        err!(locale.t("password_error"), "/account/password?err=1");
    }

    let new_hash = match hash_password(&form.new_password).await {
        Ok(h) => h,
        Err(_) => err!(locale.t("password_error"), "/account/password?err=1"),
    };

    match sqlx::query!(
        "UPDATE users SET encrypted_password = $1, updated_at = now() WHERE id = $2",
        new_hash,
        session.user_id,
    )
    .execute(&state.db)
    .await
    {
        Ok(_) => {
            // `Auth::RegistrationsController#update`: every other session
            // ends, and Devise's `password_change` mail goes out.
            match crate::sessions::destroy_others(
                &state.db,
                session.user_id,
                Some(session.activation_id),
            )
            .await
            {
                Ok(tokens) => crate::sessions::kill_streams(&state, tokens).await,
                Err(error) => tracing::warn!(%error, "could not end the other sessions"),
            }
            crate::accounts::notify_password_change(&state, session.user_id).await;
            if htmx {
                return Html(format!(
                    "<div class=\"success\">{}</div>",
                    locale.t("password_changed")
                ))
                .into_response();
            }
            Redirect::to("/account/password?ok=1").into_response()
        }
        Err(_) => err!(locale.t("password_error"), "/account/password?err=1"),
    }
}

// ── GET /account/delete ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DeleteQuery {
    pub err: Option<String>,
}

/// eunha's counterpart to Mastodon's `/settings/delete`.
pub async fn delete_page(
    state: AppState,
    axum::extract::Extension(ResolvedInstance(instance)): axum::extract::Extension<
        ResolvedInstance,
    >,
    client_ip: ClientIpExt,
    headers: HeaderMap,
    Query(query): Query<DeleteQuery>,
) -> Response {
    let locale = Locale::detect(None, accept_language(&headers));

    let Some(session) = get_session(&headers, &state, client_addr(client_ip)).await else {
        return Redirect::to("/account/login").into_response();
    };

    // `require_not_suspended!`
    let account = match load_deletion_subject(&state, session.user_id).await {
        Some(a) if a.suspended => return Redirect::to("/account").into_response(),
        Some(a) => a,
        None => return Redirect::to("/account/login").into_response(),
    };

    let html = templates::render(
        "account_delete.html",
        minijinja::context! {
            lang => locale.as_str(),
            domain => instance.domain.clone(),
            err => query.err.as_deref() == Some("1"),
            has_password => !account.encrypted_password.is_empty(),
            confirmed_and_approved => account.confirmed_and_approved,
            contact_email => crate::settings::Snapshot::load(&state).await.site_contact_email(&instance),
            t_warning_email_change => locale.t("delete_warning_email_change"),
            t_warning_email_reconfirmation => locale.t("delete_warning_email_reconfirmation"),
            t_warning_email_contact => locale.t("delete_warning_email_contact"),
            t_warning_username_available => locale.t("delete_warning_username_available"),
            t_warning_more_details => locale.t("delete_warning_more_details"),
            t_privacy_policy => locale.t("privacy_policy"),
            t_delete_account => locale.t("delete_account"),
            t_warning_before => locale.t("delete_warning_before"),
            t_warning_irreversible => locale.t("delete_warning_irreversible"),
            t_warning_username_unavailable => locale.t("delete_warning_username_unavailable"),
            t_warning_data_removal => locale.t("delete_warning_data_removal"),
            t_warning_caches => locale.t("delete_warning_caches"),
            t_confirm_password => locale.t("delete_confirm_password"),
            t_confirm_username => locale.t("delete_confirm_username"),
            t_challenge_not_passed => locale.t("delete_challenge_not_passed"),
            t_sign_out => locale.t("sign_out"),
            t_back_to_account => locale.t("back_to_account"),
        },
    );
    Html(html).into_response()
}

struct DeletionSubject {
    account_id: i64,
    username: String,
    encrypted_password: String,
    suspended: bool,
    /// `current_user.confirmed? && current_user.approved?`, which decides
    /// which warnings the page shows.
    confirmed_and_approved: bool,
}

async fn load_deletion_subject(state: &AppState, user_id: i64) -> Option<DeletionSubject> {
    let row = sqlx::query!(
        r#"SELECT u.account_id, u.encrypted_password, a.username,
                  a.suspended_at, a.requested_deletion_at,
                  (u.confirmed_at IS NOT NULL AND u.approved) AS "confirmed_and_approved!"
           FROM users u JOIN accounts a ON a.id = u.account_id
           WHERE u.id = $1"#,
        user_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()??;
    Some(DeletionSubject {
        account_id: row.account_id,
        username: row.username,
        encrypted_password: row.encrypted_password,
        suspended: row.suspended_at.is_some() || row.requested_deletion_at.is_some(),
        confirmed_and_approved: row.confirmed_and_approved,
    })
}

// ── POST /account/delete ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DeleteForm {
    pub password: Option<String>,
    pub username: Option<String>,
}

/// Port of `Settings::DeletesController#destroy`: pass the challenge, suspend
/// the account, purge it, and sign out.
pub async fn delete_post(
    state: AppState,
    client_ip: ClientIpExt,
    headers: HeaderMap,
    Form(form): Form<DeleteForm>,
) -> Response {
    let locale = Locale::detect(None, accept_language(&headers));
    let htmx = is_htmx(&headers);

    let Some(session) = get_session(&headers, &state, client_addr(client_ip)).await else {
        return Redirect::to("/account/login").into_response();
    };
    let Some(account) = load_deletion_subject(&state, session.user_id).await else {
        return Redirect::to("/account/login").into_response();
    };
    if account.suspended {
        return Redirect::to("/account").into_response();
    }

    // `challenge_passed?`
    let passed = if account.encrypted_password.is_empty() {
        form.username.as_deref() == Some(account.username.as_str())
    } else {
        verify_password(
            form.password.as_deref().unwrap_or(""),
            &account.encrypted_password,
        )
        .await
        .is_ok()
    };
    if !passed {
        if htmx {
            return Html(format!(
                "<div class=\"error\">{}</div>",
                locale.t("delete_challenge_not_passed")
            ))
            .into_response();
        }
        return Redirect::to("/account/delete?err=1").into_response();
    }

    if let Err(e) = crate::delete_account::mark_deleted(&state, account.account_id).await {
        tracing::error!(account_id = account.account_id, error = %e, "failed to mark account deleted");
        if htmx {
            return Html(format!(
                "<div class=\"error\">{}</div>",
                locale.t("err_server")
            ))
            .into_response();
        }
        return Redirect::to("/account/delete?err=1").into_response();
    }

    crate::delete_account::call_later(
        &state,
        account.account_id,
        crate::delete_account::Options::self_service(),
    )
    .await;

    // `sign_out`
    if let Some(session_id) = extract_session_token(&headers) {
        if let Ok(tokens) = crate::sessions::deactivate(&state.db, &session_id).await {
            crate::sessions::kill_streams(&state, tokens).await;
        }
    }
    let mut h = HeaderMap::new();
    h.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(clear_cookie()).unwrap(),
    );
    if htmx {
        h.insert(
            HeaderName::from_static("hx-redirect"),
            HeaderValue::from_static("/account/login?deleted=1"),
        );
        return (h, Html(String::new())).into_response();
    }
    (h, Redirect::to("/account/login?deleted=1")).into_response()
}

// The instance invite tree now lives in the SPA (`/invite-tree`, backed by
// `GET /api/eunha/v1/invite_tree`); the old server-rendered `/account/invites`
// page was removed to avoid maintaining a second implementation.

// ── Sign-up and its confirmation ───────────────────────────────────────────────

/// Whether the user has confirmed their address.
async fn user_confirmed(state: &AppState, user_id: i64) -> bool {
    sqlx::query_scalar!(
        r#"SELECT confirmed_at IS NOT NULL AS "confirmed!" FROM users WHERE id = $1"#,
        user_id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .unwrap_or(false)
}

/// `POST /auth`: `Auth::RegistrationsController#create`. The user is saved
/// as a sign-up through the API saves it, signed in (Devise's `sign_up`, as
/// `active_for_authentication?` lets an unconfirmed user in), and sent to
/// `auth/setup` (`after_sign_up_path_for`) to wait for the link.
pub async fn registration_post(
    state: AppState,
    axum::extract::Extension(ResolvedInstance(instance)): axum::extract::Extension<
        ResolvedInstance,
    >,
    client_ip: ClientIpExt,
    headers: HeaderMap,
    crate::api::mastodon::extractors::FormOrJson(form): crate::api::mastodon::extractors::FormOrJson<
        crate::api::mastodon::signup::ApiCreateAccountForm,
    >,
) -> Response {
    let ip = client_addr(client_ip);
    let instance = crate::settings::Snapshot::load(&state)
        .await
        .amend(&instance);
    let registered =
        match crate::api::mastodon::signup::register(&state, &instance, &form, ip, None).await {
            Ok(registered) => registered,
            Err(error) => return error.into_response(),
        };
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    let session_id =
        match crate::sessions::activate(&state.db, registered.user_id, ip, user_agent).await {
            Ok(id) => id,
            Err(error) => {
                tracing::error!(%error, "could not activate a session");
                return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
    let mut h = HeaderMap::new();
    h.append(header::SET_COOKIE, set_cookie(&session_id).parse().unwrap());
    if is_htmx(&headers) {
        h.insert(
            HeaderName::from_static("hx-redirect"),
            HeaderValue::from_static("/auth/setup"),
        );
        return (h, "").into_response();
    }
    (h, Redirect::to("/auth/setup")).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct SetupQuery {
    pub sent: Option<String>,
}

/// `GET /auth/setup`: `Auth::SetupController#show`, for a signed-in user
/// still unconfirmed or awaiting approval: the address the link went to, and
/// a form to put it right and have the link sent again.
pub async fn setup_page(
    state: AppState,
    axum::extract::Extension(ResolvedInstance(instance)): axum::extract::Extension<
        ResolvedInstance,
    >,
    client_ip: ClientIpExt,
    headers: HeaderMap,
    Query(query): Query<SetupQuery>,
) -> Response {
    render_setup(
        &state,
        &instance,
        client_ip,
        &headers,
        query.sent.as_deref() == Some("1"),
        None,
    )
    .await
}

async fn render_setup(
    state: &AppState,
    instance: &crate::config::InstanceConfig,
    client_ip: ClientIpExt,
    headers: &HeaderMap,
    sent: bool,
    error: Option<String>,
) -> Response {
    let locale = Locale::detect(None, accept_language(headers));
    // `authenticate_user!`.
    let Some(session) = get_session(headers, state, client_addr(client_ip)).await else {
        return redirect_to_sign_in("/auth/setup");
    };
    let Some(user) = sqlx::query!(
        r#"SELECT email, confirmed_at IS NOT NULL AS "confirmed!", approved
           FROM users WHERE id = $1"#,
        session.user_id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten() else {
        return Redirect::to("/account/login").into_response();
    };
    // `require_unconfirmed_or_pending!`.
    if user.confirmed && user.approved {
        return Redirect::to("/").into_response();
    }
    let html = templates::render(
        "auth_setup.html",
        minijinja::context! {
            lang => locale.as_str(),
            domain => instance.domain.clone(),
            email => user.email.clone(),
            sent,
            error,
            email_hint => locale.t("setup_email_hint").replace("%{email}", &user.email),
            t_setup_title => locale.t("setup_title"),
            t_setup_sent => locale.t("setup_sent"),
            t_link_not_received => locale.t("setup_link_not_received"),
            t_below_hint => locale.t("setup_below_hint"),
            t_email => locale.t("email"),
            t_resend_confirmation => locale.t("resend_confirmation"),
            t_sign_out => locale.t("sign_out"),
        },
    );
    Html(html).into_response()
}

#[derive(Debug, Deserialize)]
pub struct SetupForm {
    pub email: String,
}

/// `PUT /auth/setup`: `Auth::SetupController#update`. The address is updated
/// without the password the settings ask for, as only a user not yet
/// confirmed may: Devise's reconfirmable holds a new one in
/// `unconfirmed_email`. Then the link goes out again
/// (`resend_confirmation_instructions unless @user.confirmed?`).
pub async fn setup_post(
    state: AppState,
    axum::extract::Extension(ResolvedInstance(instance)): axum::extract::Extension<
        ResolvedInstance,
    >,
    client_ip: ClientIpExt,
    headers: HeaderMap,
    Form(form): Form<SetupForm>,
) -> Response {
    let Some(session) = get_session(&headers, &state, client_addr(client_ip)).await else {
        return redirect_to_sign_in("/auth/setup");
    };
    let Some(user) = sqlx::query!(
        r#"SELECT email, confirmed_at IS NOT NULL AS "confirmed!", approved
           FROM users WHERE id = $1"#,
        session.user_id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten() else {
        return Redirect::to("/account/login").into_response();
    };
    if user.confirmed && user.approved {
        return Redirect::to("/").into_response();
    }
    let email = form.email.trim().to_lowercase();
    if email != user.email {
        let problem = if !crate::accounts::valid_email(&email) {
            Some("E-mail address is invalid".to_owned())
        } else if sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM users WHERE lower(email) = $1 AND id <> $2) AS "e!""#,
            email,
            session.user_id,
        )
        .fetch_one(&state.db)
        .await
        .unwrap_or(true)
        {
            Some("E-mail address has already been taken".to_owned())
        } else if let Err(refusal) =
            crate::moderation::signup::check_email(&state, &email, user.confirmed).await
        {
            let (_, label, _, message) = refusal.detail();
            Some(format!("{label} {message}"))
        } else {
            None
        };
        if let Some(problem) = problem {
            return render_setup(&state, &instance, client_ip, &headers, false, Some(problem))
                .await;
        }
        if let Err(error) =
            crate::accounts::set_unconfirmed_email(&state.db, session.user_id, &email).await
        {
            tracing::error!(%error, "could not change the address awaiting confirmation");
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        // `after_commit :send_reconfirmation_instructions`, for the address
        // `update` held back.
        if let Err(error) =
            crate::accounts::send_confirmation_instructions(&state, session.user_id).await
        {
            tracing::error!(error = %format!("{error:#}"), "could not send reconfirmation instructions");
        }
    }
    if !user.confirmed {
        if let Err(error) =
            crate::accounts::send_confirmation_instructions(&state, session.user_id).await
        {
            tracing::error!(error = %format!("{error:#}"), "could not resend confirmation instructions");
        }
    }
    Redirect::to("/auth/setup?sent=1").into_response()
}

/// Devise's `sign_in(user)` from a page outside this module, then on to
/// `target`, with `return_to` kept as the page to come back to after signing
/// in (`store_location_for`).
pub async fn sign_in_and_redirect(
    state: &AppState,
    user_id: i64,
    ip: Option<std::net::IpAddr>,
    user_agent: Option<&str>,
    target: &str,
    return_to: Option<&str>,
) -> Response {
    let session_id = match crate::sessions::activate(&state.db, user_id, ip, user_agent).await {
        Ok(id) => id,
        Err(error) => {
            tracing::error!(%error, "could not activate a session");
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let mut h = HeaderMap::new();
    h.append(header::SET_COOKIE, set_cookie(&session_id).parse().unwrap());
    if let Some(path) = return_to {
        if let Ok(value) = HeaderValue::from_str(&format!(
            "{RETURN_TO_COOKIE}={}; HttpOnly; SameSite=Lax; Path=/",
            urlencoding::encode(path)
        )) {
            h.append(header::SET_COOKIE, value);
        }
    }
    (h, Redirect::to(target)).into_response()
}
