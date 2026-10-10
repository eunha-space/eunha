//! Mastodon's self-destruct mode: `SelfDestructHelper`,
//! `ApplicationController#check_self_destruct!`, and
//! `Scheduler::SelfDestructScheduler`.
//!
//! An instance that is closing for good is given `self_destruct`, the value
//! `eunha self-destruct` prints (`tootctl self-destruct`). While it verifies,
//! the instance answers nearly every request with a 410, runs none of its
//! schedules but this one, and every minute tells every server it knows that
//! another batch of its local accounts is gone, with a `Delete` of each actor.
//! Nothing local is erased: the database is meant to be dropped once every
//! notice is out (docs/operating/self-destruct.md).

use anyhow::Result;
use axum::{
    extract::Request,
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Response},
    Json,
};

use crate::config::InstanceConfig;
use crate::state::AppState;

/// `SelfDestructHelper::VERIFY_PURPOSE`, the name of the message verifier.
pub const VERIFY_PURPOSE: &str = "self-destruct";

/// `SelfDestructScheduler::MAX_ENQUEUED`.
const MAX_ENQUEUED: i64 = 10_000;

/// `SelfDestructScheduler::MAX_ACCOUNT_DELETIONS_PER_JOB`.
const MAX_ACCOUNT_DELETIONS_PER_PASS: i64 = 50;

/// What `tootctl self-destruct` tells the administrator to set:
/// `message_verifier('self-destruct').generate(local_domain)`. Without
/// `secret_key_base` it is signed under a key derived from the VAPID private
/// key instead, as async refresh ids are.
pub fn value(instance: &InstanceConfig) -> String {
    match &instance.secret_key_base {
        Some(secret) => secret.self_destruct_value(&instance.domain),
        None => crate::crypto::sign_message(
            &instance.vapid_private_key,
            VERIFY_PURPOSE.as_bytes(),
            &instance.domain,
        ),
    }
}

/// `SelfDestructHelper.self_destruct?`: `self_destruct` is set, and verifies
/// as signing this instance's domain. A value signed under the VAPID key is
/// read either way, so configuring `secret_key_base` later keeps it.
pub fn enabled(instance: &InstanceConfig) -> bool {
    let Some(value) = instance
        .self_destruct
        .as_deref()
        .filter(|v| !v.trim().is_empty())
    else {
        return false;
    };
    let signed = instance
        .secret_key_base
        .as_ref()
        .and_then(|secret| secret.verify_self_destruct(value))
        .or_else(|| {
            crate::crypto::verify_message(
                &instance.vapid_private_key,
                VERIFY_PURPOSE.as_bytes(),
                value,
            )
        });
    signed.as_deref() == Some(instance.domain.as_str())
}

// ── check_self_destruct! ─────────────────────────────────────────────────

/// `ApplicationController#check_self_destruct!`, before every request of a
/// self-destructing instance but those [`exempt`] lets through.
pub async fn gate(req: Request, next: Next) -> Response {
    let closing = req
        .extensions()
        .get::<AppState>()
        .is_some_and(|state| enabled(&state.instance));
    if !closing || exempt(req.uri().path()) {
        return next.run(req).await;
    }
    let domain = req
        .extensions()
        .get::<AppState>()
        .map(|state| state.instance.domain.clone())
        .unwrap_or_default();
    gone(req.uri().path(), req.headers(), &domain)
}

/// What Mastodon still serves while it self-destructs, in eunha's terms.
///
///  -  What is not an `ApplicationController`: the health check, WebFinger,
///     host-meta, NodeInfo, the OAuth metadata, the manifest and the custom
///     CSS; Doorkeeper's token and revocation endpoints, which are
///     `ActionController::API`; and the streaming API, which Mastodon serves
///     from its Node process. The web client's static files, which nginx
///     serves for Mastodon, go too.
///  -  What skips `check_self_destruct!`, so that members can still sign in
///     and take their data with them: signing in and out, password resets,
///     email confirmation, the security key step, the account page and its
///     password change (`registrations#edit`/`#update`), the exports and the
///     archive takeout, the login history, and the two-factor methods with
///     their security keys. Eunha's web client serves the export page at
///     `/settings/export`, from the eunha API under `/api/eunha/v1`.
pub fn exempt(path: &str) -> bool {
    const PATHS: &[&str] = &[
        "/health",
        "/manifest",
        "/manifest.json",
        "/custom.css",
        "/oauth/token",
        "/oauth/revoke",
        "/account",
        "/account/login",
        "/account/logout",
        "/account/sso",
        "/account/password",
        "/auth/challenge",
        "/settings/export",
        "/api/eunha/v1/health",
        "/api/eunha/v1/exports",
        "/api/eunha/v1/backups",
        "/api/eunha/v1/login_activities",
        "/api/eunha/v1/two_factor_authentication",
    ];
    const PREFIXES: &[&str] = &[
        "/.well-known/",
        "/nodeinfo/",
        "/css/",
        "/assets/",
        "/api/v1/streaming",
        "/auth/password",
        "/auth/confirm",
        "/auth/sessions/",
        "/backups/",
        "/api/eunha/v1/exports/",
        "/api/eunha/v1/backups/",
        "/api/eunha/v1/two_factor_authentication/webauthn_credentials",
    ];
    if PATHS.contains(&path) || PREFIXES.iter().any(|p| path.starts_with(p)) {
        return true;
    }
    // A file at the top of the web client's build: its stylesheet and icons.
    let rest = path.trim_start_matches('/');
    !rest.is_empty() && !rest.contains('/') && !rest.starts_with('@') && rest.contains('.')
}

/// The 410: `{"error":"Gone"}` to what asks for JSON, the API and OAuth
/// included, and otherwise a page saying the server is closing.
fn gone(path: &str, headers: &HeaderMap, domain: &str) -> Response {
    let wants_json = path.starts_with("/api/")
        || path.starts_with("/oauth/")
        || headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|accept| accept.contains("json"));
    if wants_json {
        return (StatusCode::GONE, Json(serde_json::json!({"error": "Gone"}))).into_response();
    }
    let domain = domain
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let page = format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>This server is closing</title></head>\
         <body><h1>This server is closing</h1>\
         <p>{domain} is closing for good. Its accounts can no longer be used, \
         but you can still <a href=\"/account/login\">sign in</a> to download \
         your data.</p></body></html>\n"
    );
    (StatusCode::GONE, Html(page)).into_response()
}

// ── SelfDestructScheduler ────────────────────────────────────────────────

/// `SelfDestructScheduler#perform`.
pub async fn perform(state: &AppState) -> Result<()> {
    if !enabled(&state.instance) || overwhelmed(state).await? {
        return Ok(());
    }
    // Local accounts neither deleted nor marked for deletion.
    let accounts: Vec<i64> = sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE domain IS NULL AND requested_deletion_at IS NULL
         ORDER BY id ASC LIMIT $1",
        MAX_ACCOUNT_DELETIONS_PER_PASS,
    )
    .fetch_all(&state.db)
    .await?;
    for account_id in accounts {
        delete_account(state, account_id).await?;
    }
    if overwhelmed(state).await? {
        return Ok(());
    }
    // Local accounts marked for deletion but not deleted yet.
    let accounts: Vec<i64> = sqlx::query_scalar!(
        "SELECT a.id FROM accounts a
         JOIN account_deletion_requests r ON r.account_id = a.id
         WHERE a.domain IS NULL ORDER BY a.id LIMIT $1",
        MAX_ACCOUNT_DELETIONS_PER_PASS,
    )
    .fetch_all(&state.db)
    .await?;
    for account_id in accounts {
        delete_account(state, account_id).await?;
    }
    Ok(())
}

/// `sidekiq_overwhelmed?`, for eunha's queues: more than `MAX_ENQUEUED`
/// deliveries and jobs waiting. Mastodon also holds off while Redis is past
/// half its memory; eunha's queues are in PostgreSQL, so there is no such
/// limit to watch.
async fn overwhelmed(state: &AppState) -> Result<bool> {
    let enqueued = sqlx::query_scalar!(
        r#"SELECT (SELECT count(*) FROM eunha.ojak_queue WHERE failed_at IS NULL)
                + (SELECT count(*) FROM eunha.jobs WHERE dead_at IS NULL) AS "enqueued!""#,
    )
    .fetch_one(&state.db)
    .await?;
    Ok(enqueued > MAX_ENQUEUED)
}

/// `SelfDestructScheduler#delete_account!`: the actor's `Delete`, signed
/// with its Linked Data Signature, to every inbox known
/// (`Account.inboxes`), then the account marked deleted
/// (`requested_deletion_at`) without a deletion request, and any it had
/// removed. Nothing else about the account changes.
async fn delete_account(state: &AppState, account_id: i64) -> Result<()> {
    let Some(account) = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        account_id
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    if crate::federation::keypair::has_signing_key(state, account.id).await? {
        let domain = &state.instance.domain;
        let actor = crate::federation::tag::account_uri_of(domain, &account);
        let key_id = crate::federation::tag::key_id_of(domain, &account);
        let inboxes = inboxes(state).await?;
        crate::federation::delivery::deliver_to_inboxes_signed(
            state,
            crate::federation::activity::delete_actor(&actor),
            inboxes,
            key_id,
            crate::federation::delivery::LinkedData::Always,
        )
        .await?;
    }
    // Not `Account#mark_deleted!`, which would make a deletion request.
    sqlx::query!(
        "UPDATE accounts SET requested_deletion_at = now(), updated_at = now() WHERE id = $1",
        account.id
    )
    .execute(&state.db)
    .await?;
    sqlx::query!(
        "DELETE FROM account_deletion_requests WHERE account_id = $1",
        account.id
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `Account.inboxes`: the preferred inbox — shared if there is one — of every
/// ActivityPub account, less those of servers no longer delivered to
/// (`DeliveryFailureTracker.without_unavailable`).
async fn inboxes(state: &AppState) -> Result<Vec<String>> {
    let inboxes = sqlx::query_scalar!(
        r#"SELECT DISTINCT coalesce(nullif(shared_inbox_url, ''), inbox_url) AS "inbox!"
           FROM accounts
           WHERE protocol = 1 AND domain IS NOT NULL
             AND coalesce(nullif(shared_inbox_url, ''), inbox_url) <> ''"#,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(inboxes
        .into_iter()
        .filter(|inbox| {
            url::Url::parse(inbox)
                .is_ok_and(|url| !state.delivery_failures.is_unavailable_inbox(&url))
        })
        .collect())
}

/// Where a self-destruct stands, as `tootctl self-destruct` reports it.
pub enum Progress {
    /// `pending_accounts`: local accounts not yet told about.
    AccountsPending(i64),
    /// Deletion notices waiting for their first attempt.
    Delivering,
    /// Every notice was tried at least once; some wait to be tried again.
    Retrying,
    /// Every notice is out.
    Done,
}

/// `tootctl self-destruct` on an instance already self-destructing. As
/// Mastodon counts them, the accounts pending are the local ones not
/// suspended nor marked for deletion, and the suspended ones that still have a
/// deletion request.
pub async fn progress(db: &sqlx::PgPool) -> Result<Progress> {
    let pending = sqlx::query_scalar!(
        r#"SELECT (SELECT count(*) FROM accounts
                   WHERE domain IS NULL AND suspended_at IS NULL
                     AND requested_deletion_at IS NULL)
                + (SELECT count(*) FROM accounts a
                   JOIN account_deletion_requests r ON r.account_id = a.id
                   WHERE a.domain IS NULL AND a.suspended_at IS NOT NULL) AS "pending!""#,
    )
    .fetch_one(db)
    .await?;
    if pending > 0 {
        return Ok(Progress::AccountsPending(pending));
    }
    let (fresh, retrying) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT count(*) FILTER (WHERE attempts = 0), count(*) FILTER (WHERE attempts > 0)
         FROM eunha.ojak_queue WHERE failed_at IS NULL",
    )
    .fetch_one(db)
    .await?;
    let jobs =
        sqlx::query_scalar!(r#"SELECT count(*) AS "jobs!" FROM eunha.jobs WHERE dead_at IS NULL"#)
            .fetch_one(db)
            .await?;
    Ok(if fresh > 0 || jobs > 0 {
        Progress::Delivering
    } else if retrying > 0 {
        Progress::Retrying
    } else {
        Progress::Done
    })
}

#[cfg(test)]
mod tests {
    use super::exempt;

    #[test]
    fn signing_in_and_taking_data_out_stay_open() {
        for path in [
            "/.well-known/webfinger",
            "/nodeinfo/2.0",
            "/oauth/token",
            "/account/login",
            "/auth/password/edit",
            "/backups/1/download",
            "/settings/export",
            "/api/eunha/v1/exports/following_accounts.csv",
            "/api/eunha/v1/backups",
            "/api/eunha/v1/two_factor_authentication/webauthn_credentials/1",
            "/assets/index-abc123.js",
            "/auth.css",
        ] {
            assert!(exempt(path), "{path}");
        }
    }

    #[test]
    fn everything_else_is_gone() {
        for path in [
            "/",
            "/api/v1/instance",
            "/api/v1/statuses",
            "/oauth/authorize",
            "/users/alice",
            "/inbox",
            "/@alice",
            "/@alice.example",
            "/account/delete",
            "/api/eunha/v1/imports",
            "/api/eunha/v1/two_factor_authentication/otp",
            "/media_proxy/1",
        ] {
            assert!(!exempt(path), "{path}");
        }
    }
}
