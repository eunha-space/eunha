//! Quotes, as Mastodon's `Quote` model keeps them, and the services around
//! them that more than one surface needs: accepting and rejecting
//! (`Quote#accept!`, `Quote#reject!`, and the counter they move), the
//! authorization stamp's URI (`TagManager#approval_uri_for`), revoking
//! (`RevokeQuoteService`), and telling local timelines that a status changed
//! (`DistributionWorker` with `update`).

use serde_json::Value;
use sqlx::PgPool;

use crate::db::models::quote_state;
use crate::state::AppState;

/// A row of `quotes`.
#[derive(Debug, Clone)]
pub struct Quote {
    pub id: i64,
    pub account_id: i64,
    pub status_id: i64,
    pub quoted_status_id: Option<i64>,
    pub quoted_account_id: Option<i64>,
    pub state: i32,
    pub approval_uri: Option<String>,
    pub activity_uri: Option<String>,
    pub legacy: bool,
}

impl Quote {
    pub fn accepted(&self) -> bool {
        self.state == quote_state::ACCEPTED
    }
}

/// The quote with `id`.
pub async fn find(db: &PgPool, id: i64) -> sqlx::Result<Option<Quote>> {
    sqlx::query_as!(
        Quote,
        r#"SELECT id, account_id, status_id, quoted_status_id, quoted_account_id, state,
                  approval_uri, activity_uri, legacy
           FROM quotes WHERE id = $1"#,
        id,
    )
    .fetch_optional(db)
    .await
}

/// The quote `status_id` makes (`Status#quote`).
pub async fn find_by_status(db: &PgPool, status_id: i64) -> sqlx::Result<Option<Quote>> {
    sqlx::query_as!(
        Quote,
        r#"SELECT id, account_id, status_id, quoted_status_id, quoted_account_id, state,
                  approval_uri, activity_uri, legacy
           FROM quotes WHERE status_id = $1"#,
        status_id,
    )
    .fetch_optional(db)
    .await
}

/// Move the quoted status's `quotes_count` by one, never below nought
/// (`Status#increment_count!`, `#decrement_count!`).
async fn count(db: &PgPool, quoted_status_id: Option<i64>, up: bool) {
    let Some(quoted_status_id) = quoted_status_id else {
        return;
    };
    let result = if up {
        sqlx::query!(
            r#"INSERT INTO status_stats (status_id, quotes_count, created_at, updated_at)
               VALUES ($1, 1, now(), now())
               ON CONFLICT (status_id) DO UPDATE
                 SET quotes_count = GREATEST(status_stats.quotes_count, 0) + 1, updated_at = now()"#,
            quoted_status_id,
        )
        .execute(db)
        .await
    } else {
        sqlx::query!(
            r#"UPDATE status_stats SET quotes_count = GREATEST(quotes_count - 1, 0), updated_at = now()
               WHERE status_id = $1"#,
            quoted_status_id,
        )
        .execute(db)
        .await
    };
    if let Err(error) = result {
        tracing::error!(quoted_status_id, %error, "could not count a quote");
    }
}

/// `after_create_commit :increment_counter_caches!`: a quote created
/// accepted counts on the quoted status.
pub async fn created(db: &PgPool, quoted_status_id: Option<i64>, state: i32) {
    if state == quote_state::ACCEPTED {
        count(db, quoted_status_id, true).await;
    }
}

/// `after_update_commit :update_counter_caches!`, for a quote whose state
/// went from `old` to `new`, `legacy` as the update left it. A legacy quote's
/// count never moves on an update.
///
/// As upstream, one is taken off on every change to a state that is not
/// accepted, whatever the state was before (the code carries a TODO asking
/// whether that is right): a pending quote that is rejected takes one off a
/// count it was never in, which the floor at nought then hides.
pub async fn state_changed(
    db: &PgPool,
    quoted_status_id: Option<i64>,
    legacy: bool,
    old: i32,
    new: i32,
) {
    if legacy || old == new {
        return;
    }
    count(db, quoted_status_id, new == quote_state::ACCEPTED).await;
}

/// `Quote#accept!`: accepted, with `approval_uri` when one is given. Says
/// whether the state changed.
pub async fn accept(db: &PgPool, quote: &Quote, approval_uri: Option<&str>) -> sqlx::Result<bool> {
    let old = sqlx::query_scalar!(
        r#"UPDATE quotes q
           SET state = 1,
               approval_uri = CASE WHEN $2::text IS NULL THEN q.approval_uri ELSE $2::text END,
               updated_at = now()
           FROM (SELECT id, state FROM quotes WHERE id = $1 FOR UPDATE) old
           WHERE q.id = old.id
           RETURNING old.state"#,
        quote.id,
        approval_uri,
    )
    .fetch_optional(db)
    .await?;
    let Some(old) = old else {
        return Ok(false);
    };
    state_changed(
        db,
        quote.quoted_status_id,
        quote.legacy,
        old,
        quote_state::ACCEPTED,
    )
    .await;
    Ok(old != quote_state::ACCEPTED)
}

/// `Quote#reject!`: an accepted quote is revoked, any other but a revoked one
/// rejected, and its stamp forgotten either way. Says whether the state
/// changed.
pub async fn reject(db: &PgPool, quote: &Quote) -> sqlx::Result<bool> {
    let row = sqlx::query!(
        r#"UPDATE quotes q
           SET state = CASE WHEN old.state = 1 THEN 3 ELSE 2 END,
               approval_uri = NULL,
               updated_at = now()
           FROM (SELECT id, state FROM quotes WHERE id = $1 FOR UPDATE) old
           WHERE q.id = old.id AND old.state <> 3
           RETURNING old.state AS old_state, q.state AS new_state"#,
        quote.id,
    )
    .fetch_optional(db)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    state_changed(
        db,
        quote.quoted_status_id,
        quote.legacy,
        row.old_state,
        row.new_state,
    )
    .await;
    Ok(row.old_state != row.new_state)
}

/// `Quote#destroy`: the row goes, its notification with it (`has_one
/// :notification, dependent: :destroy`), and an accepted quote stops counting
/// (`after_destroy_commit :decrement_counter_caches!`).
pub async fn destroy(db: &PgPool, quote: &Quote) -> sqlx::Result<()> {
    let state = sqlx::query_scalar!("DELETE FROM quotes WHERE id = $1 RETURNING state", quote.id)
        .fetch_optional(db)
        .await?;
    sqlx::query!(
        "DELETE FROM notifications WHERE activity_type = 'Quote' AND activity_id = $1",
        quote.id,
    )
    .execute(db)
    .await?;
    if state == Some(quote_state::ACCEPTED) {
        count(db, quote.quoted_status_id, false).await;
    }
    Ok(())
}

/// `Quote#ensure_quoted_access`: the quoted author is silently mentioned in
/// the quoting status, so that they can still see what quotes them.
pub async fn ensure_quoted_access(db: &PgPool, quote: &Quote) {
    let Some(quoted_account_id) = quote.quoted_account_id else {
        return;
    };
    if let Err(error) = sqlx::query!(
        r#"INSERT INTO mentions (status_id, account_id, silent, created_at, updated_at)
           VALUES ($1, $2, true, now(), now())
           ON CONFLICT DO NOTHING"#,
        quote.status_id,
        quoted_account_id,
    )
    .execute(db)
    .await
    {
        tracing::debug!(quote_id = quote.id, %error, "could not mention the quoted account");
    }
}

/// The URI of a local account's quote authorization `quote_id`:
/// `account_quote_authorization_url`, or `ap_account_quote_authorization_url`
/// for an account that uses numeric ids.
pub fn local_approval_uri(
    domain: &str,
    account_id: i64,
    id_scheme: Option<i32>,
    username: &str,
    quote_id: i64,
) -> String {
    format!(
        "{}/quote_authorizations/{quote_id}",
        crate::federation::tag::account_uri(domain, account_id, id_scheme, username)
    )
}

/// `ActivityPub::TagManager#approval_uri_for`: a remote quoted author's stamp
/// is what it said it was; a local one's is ours to name, and is named only
/// once the quote is accepted unless `check_approval` is off.
pub async fn approval_uri_for(
    state: &AppState,
    quote: &Quote,
    check_approval: bool,
) -> sqlx::Result<Option<String>> {
    let quoted = match quote.quoted_account_id {
        Some(id) => {
            sqlx::query!(
                "SELECT id, domain, username, id_scheme FROM accounts WHERE id = $1",
                id
            )
            .fetch_optional(&state.db)
            .await?
        }
        None => None,
    };
    let Some(quoted) = quoted.filter(|a| a.domain.is_none()) else {
        return Ok(quote.approval_uri.clone());
    };
    if check_approval && !quote.accepted() {
        return Ok(None);
    }
    Ok(Some(local_approval_uri(
        &state.instance.domain,
        quoted.id,
        quoted.id_scheme,
        &quoted.username,
        quote.id,
    )))
}

/// `ActivityPub::TagManager#uri_for` a status: a remote one's `uri`, a local
/// one's as its author's scheme names it.
pub async fn status_uri(state: &AppState, status_id: i64) -> sqlx::Result<Option<String>> {
    let row = sqlx::query!(
        r#"SELECT s.id, s.uri, a.id AS account_id, a.domain, a.username, a.id_scheme
           FROM statuses s JOIN accounts a ON a.id = s.account_id
           WHERE s.id = $1"#,
        status_id,
    )
    .fetch_optional(&state.db)
    .await?;
    Ok(row.map(|r| match r.uri.filter(|u| !u.is_empty()) {
        Some(uri) => uri,
        None => crate::federation::tag::status_uri(
            &state.instance.domain,
            r.account_id,
            r.id_scheme,
            &r.username,
            r.id,
        ),
    }))
}

/// `ActivityPub::TagManager#uri_for` an account.
pub async fn account_uri(state: &AppState, account_id: i64) -> sqlx::Result<Option<String>> {
    let row = sqlx::query!(
        "SELECT id, uri, domain, username, id_scheme FROM accounts WHERE id = $1",
        account_id,
    )
    .fetch_optional(&state.db)
    .await?;
    Ok(row.map(|r| {
        if r.domain.is_none() {
            crate::federation::tag::account_uri(
                &state.instance.domain,
                r.id,
                r.id_scheme,
                &r.username,
            )
        } else {
            r.uri.unwrap_or_default()
        }
    }))
}

/// `ActivityPub::QuoteAuthorizationSerializer`, without an `@context`: the
/// stamp of a local account's quote. `None` when a status it names is gone.
pub async fn authorization_object(
    state: &AppState,
    quote: &Quote,
    check_approval: bool,
) -> anyhow::Result<Option<Value>> {
    let Some(id) = approval_uri_for(state, quote, check_approval).await? else {
        return Ok(None);
    };
    let (Some(quoted_account_id), Some(quoted_status_id)) =
        (quote.quoted_account_id, quote.quoted_status_id)
    else {
        return Ok(None);
    };
    let (Some(attributed_to), Some(target), Some(interacting)) = (
        account_uri(state, quoted_account_id).await?,
        status_uri(state, quoted_status_id).await?,
        status_uri(state, quote.status_id).await?,
    ) else {
        return Ok(None);
    };
    let mut object = crate::federation::consent::quote_authorization(
        &id,
        &attributed_to,
        &interacting,
        &target,
    )?;
    if let Some(map) = object.as_object_mut() {
        map.remove("@context");
    }
    Ok(Some(object))
}

/// `RevokeQuoteService`: the quote is rejected (`Quote#reject!`), local
/// timelines are told the quoting status changed, and every server that saw
/// either status is sent the `Delete` of the stamp, signed by the quoted
/// author whatever the mode (`always_sign`).
///
/// `quoting_status_known` is whether the quoting status is still at hand when
/// it has been deleted: `RemoveStatusService` revokes the quote of a status it
/// is removing, and reaches that status's audience, where a quote revoked from
/// the API after its quoting status went reaches only the quoted one's.
pub async fn revoke(
    state: &AppState,
    quote: &Quote,
    quoting_status_known: bool,
) -> anyhow::Result<()> {
    reject(&state.db, quote).await?;

    // `distribute_update!`: `DistributionWorker` finds no deleted status.
    let quoting_status_live = sqlx::query_scalar!(
        r#"SELECT (deleted_at IS NULL) AS "live!" FROM statuses WHERE id = $1"#,
        quote.status_id,
    )
    .fetch_optional(&state.db)
    .await?;
    if quoting_status_live == Some(true) {
        distribute_update(state, quote.status_id, false).await;
    }

    // `distribute_stamp_deletion!`: nothing to sign for a quoted status that
    // is already gone, whose deletion has been federated.
    let Some(quoted_status_id) = quote.quoted_status_id else {
        return Ok(());
    };
    let quoted_live = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        quoted_status_id,
    )
    .fetch_optional(&state.db)
    .await?
    .is_some();
    if !quoted_live {
        return Ok(());
    }
    let Some(quoted_account_id) = quote.quoted_account_id else {
        return Ok(());
    };
    let Some(author) = sqlx::query!(
        "SELECT id, username, id_scheme, domain FROM accounts WHERE id = $1",
        quoted_account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .filter(|a| a.domain.is_none()) else {
        return Ok(());
    };
    if !crate::federation::keypair::has_signing_key(state, author.id)
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }

    // `force_approval_id: true`: the stamp is named though it no longer
    // stands.
    let Some(object) = authorization_object(state, quote, false).await? else {
        return Ok(());
    };
    let Some(stamp) = approval_uri_for(state, quote, false).await? else {
        return Ok(());
    };
    let actor = crate::federation::tag::account_uri(
        &state.instance.domain,
        author.id,
        author.id_scheme,
        &author.username,
    );
    let activity = crate::federation::consent::delete_quote_authorization(
        &format!("{stamp}#delete"),
        &actor,
        object,
    );

    let mut inboxes = Vec::new();
    if quoting_status_known || quoting_status_live == Some(true) {
        inboxes.extend(
            crate::federation::delivery::status_reach_of(state, quote.status_id, true).await?,
        );
    }
    inboxes
        .extend(crate::federation::delivery::status_reach_of(state, quoted_status_id, true).await?);
    inboxes.sort();
    inboxes.dedup();
    if inboxes.is_empty() {
        return Ok(());
    }
    // `Quote#sign?` is true, and `always_sign: true`.
    crate::federation::delivery::deliver_to_inboxes_signed(
        state,
        activity,
        inboxes,
        format!("{actor}#main-key"),
        crate::federation::delivery::LinkedData::Always,
    )
    .await?;
    Ok(())
}

/// `LocalNotificationWorker` with a `quote`: the quoted author hears of it,
/// once.
pub async fn notify(state: &AppState, quote: &Quote) {
    let Some(quoted_account_id) = quote.quoted_account_id else {
        return;
    };
    let Ok(Some(quoter)) = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        quote.account_id,
    )
    .fetch_optional(&state.db)
    .await
    else {
        return;
    };
    crate::push::create_and_push(
        state,
        quoted_account_id,
        quoter.id,
        "quote",
        Some(quote.status_id),
        format!("{} quoted your post", quoter.display_name),
        quoter.acct(),
        crate::api::mastodon::convert::account_avatar_url_for(&state.urls, &quoter),
    )
    .await;
}

/// `DistributionWorker` with `update: true`: what `FanOutOnWriteService` does
/// for a status that changed. Its readers' timelines are given the new
/// version, and, unless `skip_notifications`, the quoted author hears of an
/// accepted quote, the mentioned of the mention, and the boosters and the
/// quoters that it was edited (the last two replacing any earlier word of
/// it).
pub async fn distribute_update(state: &AppState, status_id: i64, skip_notifications: bool) {
    if let Err(error) = try_distribute_update(state, status_id, skip_notifications).await {
        tracing::warn!(status_id, %error, "could not distribute a status update");
    }
}

async fn try_distribute_update(
    state: &AppState,
    status_id: i64,
    skip_notifications: bool,
) -> anyhow::Result<()> {
    let Some(status) = sqlx::query_as!(
        crate::db::models::Status,
        "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        status_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let Some(author) = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        status.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    // `return if @status.proper.account.suspended?`
    let proper_author_suspended = sqlx::query_scalar!(
        r#"SELECT (a.suspended_at IS NOT NULL) AS "suspended!"
           FROM statuses s JOIN accounts a ON a.id = s.account_id
           WHERE s.id = COALESCE($1::bigint, $2::bigint)"#,
        status.reblog_of_id,
        status.id,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    if proper_author_suspended {
        return Ok(());
    }

    if !skip_notifications {
        // `notify_quoted_account!`
        if let Some(quote) = find_by_status(&state.db, status.id).await? {
            if quote.accepted() {
                let quoted_local = match quote.quoted_account_id {
                    Some(id) => sqlx::query_scalar!(
                        r#"SELECT (domain IS NULL) AS "local!" FROM accounts WHERE id = $1"#,
                        id
                    )
                    .fetch_optional(&state.db)
                    .await?
                    .unwrap_or(false),
                    None => false,
                };
                if quoted_local {
                    notify(state, &quote).await;
                }
            }
        }

        // `notify_mentioned_accounts!`: the notification already made stays.
        let mentioned: Vec<i64> = sqlx::query_scalar!(
            r#"SELECT m.account_id FROM mentions m JOIN accounts a ON a.id = m.account_id
               WHERE m.status_id = $1 AND NOT m.silent AND a.domain IS NULL"#,
            status.id,
        )
        .fetch_all(&state.db)
        .await?;
        let icon = crate::api::mastodon::convert::account_avatar_url_for(&state.urls, &author);
        for account_id in mentioned {
            crate::push::create_and_push(
                state,
                account_id,
                author.id,
                "mention",
                Some(status.id),
                format!("{} mentioned you", author.display_name),
                author.acct(),
                icon.clone(),
            )
            .await;
        }

        // `notify_about_update!`
        let boosters: Vec<i64> = sqlx::query_scalar!(
            r#"SELECT s.account_id FROM statuses s JOIN accounts a ON a.id = s.account_id
               WHERE s.reblog_of_id = $1 AND s.deleted_at IS NULL AND a.domain IS NULL"#,
            status.id,
        )
        .fetch_all(&state.db)
        .await?;
        for account_id in boosters {
            crate::push::create_and_push(
                state,
                account_id,
                author.id,
                "update",
                Some(status.id),
                format!("{} edited a status", author.display_name),
                String::new(),
                icon.clone(),
            )
            .await;
        }
        let quoters = sqlx::query!(
            "SELECT account_id, status_id FROM quotes WHERE quoted_status_id = $1 AND state = 1",
            status.id,
        )
        .fetch_all(&state.db)
        .await?;
        for quote in quoters {
            crate::push::create_and_push(
                state,
                quote.account_id,
                author.id,
                "quoted_update",
                Some(quote.status_id),
                format!("{} edited a quoted post", author.display_name),
                String::new(),
                icon.clone(),
            )
            .await;
        }
    }

    // `DistributionWorker` with `update`: `FanOutOnWriteService` pushes the
    // status to the feeds again, and streams the new version where it went in.
    crate::feed::distribute(state, status.id, true).await;
    Ok(())
}

/// What `RemoveStatusService` does with the quote a removed status made: an
/// accepted quote of a local post is revoked while there is a chance
/// (`RevokeQuoteService`), and the status's destruction takes any other
/// accepted quote off the quoted post's count (`Quote#destroy`). Eunha keeps
/// the deleted status, and the quote row with it.
pub async fn status_removed(state: &AppState, status_id: i64) {
    let quote = match find_by_status(&state.db, status_id).await {
        Ok(Some(quote)) => quote,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(status_id, %error, "could not read a removed status's quote");
            return;
        }
    };
    if !quote.accepted() {
        return;
    }
    let quoted_local = match quote.quoted_account_id {
        Some(id) => sqlx::query_scalar!(
            r#"SELECT (domain IS NULL) AS "local!" FROM accounts WHERE id = $1"#,
            id
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or(false),
        None => false,
    };
    if quoted_local {
        if let Err(error) = revoke(state, &quote, true).await {
            tracing::warn!(status_id, %error, "could not revoke a removed status's quote");
        }
    } else {
        count(&state.db, quote.quoted_status_id, false).await;
    }
}
