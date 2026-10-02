//! Account moves, redirects and aliases, as Mastodon does them.
//!
//! An **alias** (`account_aliases`, upstream's `AccountAlias`) names an
//! account this one used to be. Creating one resolves the handle to the
//! account's actor id and adds it to `accounts.also_known_as`, which is the
//! local actor's `alsoKnownAs`; another server believes a Move to this
//! account only when it finds the old account there.
//!
//! A **migration** (`account_migrations`, upstream's `AccountMigration` and
//! `MoveService`) moves an account to one that lists it as an alias: the
//! account redirects there, its followers are moved over, and a `Move` goes
//! to its remote followers and the accounts that block it. A **redirect**
//! (`Form::Redirect`) is the first half alone.
//!
//! The `MoveWorker` half runs for a migration made here and for a `Move`
//! received from another server (`ActivityPub::Activity::Move`): local
//! followers of the old account follow the new one, and local accounts'
//! notes, blocks and mutes of the old account carry over to it.
//!
//! Mastodon serves all of this from web settings forms. Eunha's client is a
//! single-page app, so it is served over REST (see the `account-moves-rest-api`
//! divergence), answering with the same validation messages.

use crate::api::mastodon::accounts as relationships;
use crate::db::models::Account;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// `AccountMigration::COOLDOWN_PERIOD`, in days.
pub const COOLDOWN_DAYS: i32 = 30;

/// `AccountNote::COMMENT_SIZE_LIMIT`.
const NOTE_LIMIT: usize = 2_000;

/// `ActivityPub::TagManager#uri_for` on an account: a local account's actor
/// id, or a remote one's stored `uri`.
pub fn uri_for(state: &AppState, account: &Account) -> String {
    if account.domain.is_none() {
        crate::federation::tag::account_uri_of(&state.instance.domain, account)
    } else {
        account.stored_uri().unwrap_or_default().to_owned()
    }
}

/// `normalizes :acct, with: ->(acct) { acct.strip.delete_prefix('@') }`.
pub fn normalize_acct(acct: &str) -> String {
    let acct = acct.trim();
    acct.strip_prefix('@').unwrap_or(acct).to_owned()
}

/// `DomainValidator` with `acct: true`: the part after the `@`, if any, has
/// to be a plausible domain name.
fn acct_domain_valid(acct: &str) -> bool {
    let Some(domain) = acct.split('@').nth(1).filter(|d| !d.is_empty()) else {
        return true;
    };
    let Ok(url) = url::Url::parse(&format!("https://{domain}/")) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    host.len() < 256
        && host.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// The validation errors a form collected, as `ActiveRecord::RecordInvalid`
/// words them.
fn invalid(errors: &[String]) -> AppError {
    AppError::Unprocessable(format!("Validation failed: {}", errors.join(", ")))
}

async fn load_account(state: &AppState, id: i64) -> AppResult<Option<Account>> {
    Ok(
        sqlx::query_as!(Account, "SELECT * FROM accounts WHERE id = $1", id)
            .fetch_optional(&state.db)
            .await?,
    )
}

/// `ResolveAccountService`: the account a `username@domain` handle names,
/// local or remote, fetched through WebFinger when unknown or when
/// `skip_cache` asks for its current state. `None` when it cannot be found,
/// its domain is suspended, or fetching it fails.
pub async fn resolve_account(
    state: &AppState,
    acct: &str,
    skip_cache: bool,
) -> AppResult<Option<Account>> {
    let acct = normalize_acct(acct);
    if acct.is_empty() {
        return Ok(None);
    }
    let (username, domain) = match acct.split_once('@') {
        Some((username, domain)) => (username.to_owned(), Some(domain.to_lowercase())),
        None => (acct.clone(), None),
    };
    let local = domain.as_deref().is_none_or(|domain| {
        domain.eq_ignore_ascii_case(&state.instance.domain)
            || state
                .instance
                .aliases
                .iter()
                .any(|alias| domain.eq_ignore_ascii_case(alias))
    });
    if local {
        return Ok(sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE domain IS NULL AND lower(username) = lower($1)",
            username,
        )
        .fetch_optional(&state.db)
        .await?);
    }
    let domain = domain.unwrap_or_default();
    // `return if domain_not_allowed?(@domain)`.
    if crate::federation::moderation::domain_not_allowed(state, &domain).await {
        return Ok(None);
    }
    let known = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE lower(username) = lower($1) AND lower(domain) = $2",
        username,
        domain,
    )
    .fetch_optional(&state.db)
    .await?;
    if let Some(known) = known.as_ref().filter(|_| !skip_cache) {
        return Ok(Some(known.clone()));
    }
    let Ok(actor_uri) =
        crate::federation::webfinger::resolve(&state.fetcher, &username, &domain).await
    else {
        return Ok(None);
    };
    let Ok(id) = crate::api::ap::inbox::fetch_remote_account(state, &actor_uri).await else {
        return Ok(None);
    };
    load_account(state, id).await
}

/// `save_with_challenge` and `valid_with_challenge?`: an account with a
/// password confirms with it; one without (signed up through SSO) types its
/// username instead.
async fn challenge(
    state: &AppState,
    account: &Account,
    current_password: Option<&str>,
    current_username: Option<&str>,
) -> AppResult<()> {
    let encrypted_password: String = sqlx::query_scalar!(
        "SELECT encrypted_password FROM users WHERE account_id = $1",
        account.id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::Forbidden)?;
    if !encrypted_password.is_empty() {
        let password = current_password.unwrap_or_default();
        let valid = !password.is_empty()
            && crate::crypto::verify_password(password, &encrypted_password)
                .await
                .is_ok();
        if !valid {
            return Err(invalid(&["Current password is invalid".into()]));
        }
    } else if current_username.map(normalize_acct).as_deref() != Some(account.username.as_str()) {
        return Err(invalid(&["Current username is invalid".into()]));
    }
    Ok(())
}

/// What a migration, redirect or alias form submits.
#[derive(Debug, Default, serde::Deserialize)]
pub struct MoveForm {
    #[serde(default)]
    pub acct: String,
    pub current_password: Option<String>,
    pub current_username: Option<String>,
}

/// An `account_migrations` row.
#[derive(Debug, serde::Serialize)]
pub struct Migration {
    pub id: String,
    pub acct: String,
    pub followers_count: i64,
    pub target_account_id: Option<String>,
    pub created_at: String,
}

/// `Settings::MigrationsController#create`: move `account_id` to the
/// account `form.acct` names, which has to list it as an alias, then
/// [`move_service`].
pub async fn create_migration(
    state: &AppState,
    account_id: i64,
    form: &MoveForm,
) -> AppResult<Migration> {
    let account = load_account(state, account_id)
        .await?
        .ok_or(AppError::NotFound)?;
    challenge(
        state,
        &account,
        form.current_password.as_deref(),
        form.current_username.as_deref(),
    )
    .await?;

    let _lock = crate::redis_lock::try_acquire(
        state,
        &format!("lock:account_migration:{account_id}"),
        crate::redis_lock::DEFAULT_TTL_MS,
    )
    .await
    .ok_or_else(|| {
        AppError::ServiceUnavailable(
            "There was a temporary problem serving your request, please try again".into(),
        )
    })?;

    let acct = normalize_acct(&form.acct);
    // `before_validation :set_target_account`.
    let target = resolve_account(state, &acct, true).await?;
    let followers_count: i64 = sqlx::query_scalar!(
        "SELECT followers_count FROM account_stats WHERE account_id = $1",
        account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(0);

    let mut errors = Vec::new();
    if acct.is_empty() {
        errors.push("Acct can't be blank".to_owned());
    } else if !acct_domain_valid(&acct) {
        errors.push("Acct is not a valid domain name".to_owned());
    }
    let on_cooldown = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM account_migrations
             WHERE account_id = $1 AND created_at >= now() - make_interval(days => $2)
           ) AS "exists!""#,
        account_id,
        COOLDOWN_DAYS,
    )
    .fetch_one(&state.db)
    .await?;
    if on_cooldown {
        errors.push("You are on cooldown".to_owned());
    }
    match &target {
        None => errors.push("Acct could not be found".to_owned()),
        Some(target) => {
            let own_uri = uri_for(state, &account);
            if !target
                .also_known_as
                .as_deref()
                .unwrap_or_default()
                .contains(&own_uri)
            {
                errors.push("Acct is not an alias of this account".to_owned());
            }
            if account.moved_to_account_id == Some(target.id) {
                errors.push("Acct is the same account you have already moved to".to_owned());
            }
            if account.id == target.id {
                errors.push("Acct cannot be current account".to_owned());
            }
        }
    }
    if !errors.is_empty() {
        return Err(invalid(&errors));
    }
    let target = target.expect("validated above");

    let row = sqlx::query!(
        r#"INSERT INTO account_migrations
             (account_id, acct, followers_count, target_account_id, created_at, updated_at)
           VALUES ($1, $2, $3, $4, now(), now())
           RETURNING id, created_at"#,
        account_id,
        acct,
        followers_count,
        target.id,
    )
    .fetch_one(&state.db)
    .await?;
    drop(_lock);

    move_service(state, row.id, &account, &target).await?;

    Ok(Migration {
        id: row.id.to_string(),
        acct,
        followers_count,
        target_account_id: Some(target.id.to_string()),
        created_at: crate::api::mastodon::convert::mastodon_date(row.created_at),
    })
}

/// `MoveService`: redirect, move the local relationships, and tell the
/// fediverse — an `Update` of the profile, which now names `movedTo`, and
/// the `Move` itself.
async fn move_service(
    state: &AppState,
    migration_id: i64,
    source: &Account,
    target: &Account,
) -> AppResult<()> {
    sqlx::query!(
        "UPDATE accounts SET moved_to_account_id = $1, updated_at = now() WHERE id = $2",
        target.id,
        source.id,
    )
    .execute(&state.db)
    .await?;
    queue_move_worker(state, source.id, target.id).await;
    distribute_update(state, source.id).await;
    distribute_move(state, migration_id, source.id, target).await;
    Ok(())
}

/// `ActivityPub::UpdateDistributionWorker`.
async fn distribute_update(state: &AppState, account_id: i64) {
    let Ok(Some(account)) = load_account(state, account_id).await else {
        return;
    };
    if let Err(error) =
        crate::accounts::distribute_profile(state, &state.instance.domain, &account, None).await
    {
        tracing::warn!(account_id, %error, "could not distribute the profile Update");
    }
}

/// `ActivityPub::MoveDistributionWorker`: the `Move` goes to the inboxes of
/// the account's followers and of the accounts that block it, and to the
/// enabled relays.
async fn distribute_move(state: &AppState, migration_id: i64, source_id: i64, target: &Account) {
    let Ok(Some(source)) = load_account(state, source_id).await else {
        return;
    };
    if !crate::federation::keypair::has_signing_key(state, source.id)
        .await
        .unwrap_or(false)
    {
        return;
    }
    let inboxes: Vec<String> = match sqlx::query_scalar!(
        r#"SELECT DISTINCT inbox AS "inbox!" FROM (
             SELECT CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url ELSE a.inbox_url END AS inbox
             FROM accounts a
             WHERE a.domain IS NOT NULL AND a.inbox_url <> ''
               AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
               AND (EXISTS (SELECT 1 FROM follows f WHERE f.account_id = a.id AND f.target_account_id = $1)
                    OR EXISTS (SELECT 1 FROM blocks b WHERE b.account_id = a.id AND b.target_account_id = $1))
             UNION
             SELECT inbox_url FROM relays WHERE state = 2 AND inbox_url <> ''
           ) reach
           WHERE inbox <> ''"#,
        source_id,
    )
    .fetch_all(&state.db)
    .await
    {
        Ok(inboxes) => inboxes,
        Err(error) => {
            tracing::warn!(source_id, %error, "could not list the Move's inboxes");
            return;
        }
    };
    let actor = uri_for(state, &source);
    // `ActivityPub::MoveSerializer`.
    let activity = crate::federation::activity::move_actor(
        &format!("{actor}#moves/{migration_id}"),
        &actor,
        &actor,
        &uri_for(state, target),
    );
    let key_id = format!("{actor}#main-key");
    if let Err(error) =
        crate::federation::delivery::deliver_to_inboxes(state, activity, inboxes, key_id).await
    {
        tracing::warn!(source_id, %error, "could not queue the Move");
    }
}

/// `Settings::Migration::RedirectsController#create`: redirect to the
/// account `form.acct` names without moving anyone.
pub async fn create_redirect(state: &AppState, account_id: i64, form: &MoveForm) -> AppResult<()> {
    let account = load_account(state, account_id)
        .await?
        .ok_or(AppError::NotFound)?;
    challenge(
        state,
        &account,
        form.current_password.as_deref(),
        form.current_username.as_deref(),
    )
    .await?;
    let acct = normalize_acct(&form.acct);
    let target = resolve_account(state, &acct, true).await?;

    let mut errors = Vec::new();
    if acct.is_empty() {
        errors.push("Acct can't be blank".to_owned());
    } else if !acct_domain_valid(&acct) {
        errors.push("Acct is not a valid domain name".to_owned());
    }
    match &target {
        None => errors.push("Acct could not be found".to_owned()),
        Some(target) => {
            if account.moved_to_account_id == Some(target.id) {
                errors.push("Acct is the same account you have already moved to".to_owned());
            }
            if account.id == target.id {
                errors.push("Acct cannot be current account".to_owned());
            }
        }
    }
    if !errors.is_empty() {
        return Err(invalid(&errors));
    }
    let target = target.expect("validated above");
    sqlx::query!(
        "UPDATE accounts SET moved_to_account_id = $1, updated_at = now() WHERE id = $2",
        target.id,
        account_id,
    )
    .execute(&state.db)
    .await?;
    distribute_update(state, account_id).await;
    Ok(())
}

/// `Settings::Migration::RedirectsController#destroy`: stop redirecting.
/// The followers already moved stay where they are.
pub async fn cancel_redirect(state: &AppState, account_id: i64) -> AppResult<()> {
    let cleared = sqlx::query!(
        "UPDATE accounts SET moved_to_account_id = NULL, updated_at = now()
         WHERE id = $1 AND moved_to_account_id IS NOT NULL",
        account_id,
    )
    .execute(&state.db)
    .await?;
    if cleared.rows_affected() > 0 {
        distribute_update(state, account_id).await;
    }
    Ok(())
}

/// An `account_aliases` row.
#[derive(Debug, serde::Serialize)]
pub struct Alias {
    pub id: String,
    pub account_id: String,
    pub acct: String,
    pub uri: String,
    pub created_at: String,
}

/// The account's aliases, newest first, as the settings page lists them.
pub async fn list_aliases(state: &AppState, account_id: i64) -> AppResult<Vec<Alias>> {
    let rows = sqlx::query!(
        "SELECT id, account_id, acct, uri, created_at FROM account_aliases
         WHERE account_id = $1 ORDER BY id DESC",
        account_id,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Alias {
            id: r.id.to_string(),
            account_id: r.account_id.to_string(),
            acct: r.acct,
            uri: r.uri,
            created_at: crate::api::mastodon::convert::mastodon_date(r.created_at),
        })
        .collect())
}

/// `Settings::AliasesController#create`: `AccountAlias` resolves the handle
/// to the account's actor id, refuses one that cannot be found, is this
/// account, or is already an alias, and adds the id to `also_known_as`. The
/// profile `Update` tells other servers.
pub async fn create_alias(state: &AppState, account_id: i64, acct: &str) -> AppResult<Alias> {
    let account = load_account(state, account_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let acct = normalize_acct(acct);
    // `before_validation :set_uri`.
    let uri = match resolve_account(state, &acct, false).await? {
        Some(target) => uri_for(state, &target),
        None => String::new(),
    };

    let mut errors = Vec::new();
    if acct.is_empty() {
        errors.push("Acct can't be blank".to_owned());
    } else if !acct_domain_valid(&acct) {
        errors.push("Acct is not a valid domain name".to_owned());
    }
    let taken = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM account_aliases WHERE account_id = $1 AND uri = $2) AS "exists!""#,
        account_id,
        uri,
    )
    .fetch_one(&state.db)
    .await?;
    if taken {
        errors.push("Uri has already been taken".to_owned());
    }
    if uri.is_empty() {
        errors.push("Acct could not be found".to_owned());
    } else if uri == uri_for(state, &account) {
        errors.push("Acct cannot be current account".to_owned());
    }
    if !errors.is_empty() {
        return Err(invalid(&errors));
    }

    let row = sqlx::query!(
        r#"INSERT INTO account_aliases (account_id, acct, uri, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())
           RETURNING id, created_at"#,
        account_id,
        acct,
        uri,
    )
    .fetch_one(&state.db)
    .await?;
    // `after_create :add_to_account`.
    sqlx::query!(
        "UPDATE accounts SET also_known_as = COALESCE(also_known_as, '{}') || $2::varchar,
                             updated_at = now()
         WHERE id = $1",
        account_id,
        uri,
    )
    .execute(&state.db)
    .await?;
    distribute_update(state, account_id).await;

    Ok(Alias {
        id: row.id.to_string(),
        account_id: account_id.to_string(),
        acct,
        uri,
        created_at: crate::api::mastodon::convert::mastodon_date(row.created_at),
    })
}

/// `Settings::AliasesController#destroy`: the alias goes, and so does its
/// id from `also_known_as` (`after_destroy :remove_from_account`). Upstream
/// sends no `Update` for this; the next profile change carries it.
pub async fn destroy_alias(state: &AppState, account_id: i64, alias_id: i64) -> AppResult<()> {
    let uri = sqlx::query_scalar!(
        "DELETE FROM account_aliases WHERE id = $1 AND account_id = $2 RETURNING uri",
        alias_id,
        account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    sqlx::query!(
        "UPDATE accounts SET also_known_as = array_remove(also_known_as, $2::varchar),
                             updated_at = now()
         WHERE id = $1",
        account_id,
        uri,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

// ── MoveWorker ───────────────────────────────────────────────────────────

/// Run `MoveWorker` for `source` having moved to `target`: in the
/// background, or at once when the tests ask for background work inline.
pub async fn queue_move_worker(state: &AppState, source_id: i64, target_id: i64) {
    if crate::feed::sync_fanout() {
        run_move_worker(state, source_id, target_id).await;
    } else {
        let state = state.clone();
        crate::tenants::spawn(async move {
            run_move_worker(&state, source_id, target_id).await;
        });
    }
}

async fn run_move_worker(state: &AppState, source_id: i64, target_id: i64) {
    if let Err(error) = move_worker(state, source_id, target_id).await {
        tracing::warn!(source_id, target_id, %error, "moving relationships failed");
    }
}

/// `MoveWorker#perform`. A step that fails for one account does not stop
/// the others; the first failure is returned at the end.
pub async fn move_worker(state: &AppState, source_id: i64, target_id: i64) -> AppResult<()> {
    let (Some(source), Some(target)) = (
        load_account(state, source_id).await?,
        load_account(state, target_id).await?,
    ) else {
        // `rescue ActiveRecord::RecordNotFound`.
        return Ok(());
    };

    let mut deferred: Option<AppError> = None;
    if source.domain.is_none() && target.domain.is_none() {
        let moved = rewrite_follows(state, &source, &target).await?;
        update_followers_count(state, source.id, -moved).await?;
        update_followers_count(state, target.id, moved).await?;
    } else {
        queue_follow_unfollows(state, &source, &target, &mut deferred).await?;
    }

    copy_account_notes(state, &source, &target, &mut deferred).await?;
    carry_blocks_over(state, &source, &target, &mut deferred).await?;
    carry_mutes_over(state, &source, &target, &mut deferred).await?;

    match deferred {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// `Account::Counters#update_count!`.
async fn update_followers_count(state: &AppState, account_id: i64, by: i64) -> AppResult<()> {
    if by == 0 {
        return Ok(());
    }
    sqlx::query!(
        "INSERT INTO account_stats (account_id, followers_count, created_at, updated_at)
         VALUES ($1, GREATEST($2::bigint, 0), now(), now())
         ON CONFLICT (account_id) DO UPDATE
           SET followers_count = GREATEST(account_stats.followers_count + $2::bigint, 0), updated_at = now()",
        account_id,
        by,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `MoveWorker#rewrite_follows!`, for a move between two local accounts:
/// the follows themselves are pointed at the new account. Returns how many
/// were.
async fn rewrite_follows(state: &AppState, source: &Account, target: &Account) -> AppResult<i64> {
    // First, approve the source's followers' pending requests to the target.
    let requesters: Vec<i64> = sqlx::query_scalar!(
        "SELECT fr.account_id FROM follow_requests fr
         WHERE fr.target_account_id = $2
           AND fr.account_id IN (SELECT account_id FROM follows WHERE target_account_id = $1)",
        source.id,
        target.id,
    )
    .fetch_all(&state.db)
    .await?;
    for requester in requesters {
        relationships::authorize(state, requester, target.id).await?;
    }

    // Then local accounts following both: lists holding the old account
    // gain the new one.
    sqlx::query!(
        "INSERT INTO list_accounts (list_id, account_id, follow_id)
         SELECT la.list_id, $2, tf.id
         FROM list_accounts la
         JOIN lists l ON l.id = la.list_id
         JOIN accounts owner ON owner.id = l.account_id AND owner.domain IS NULL
         JOIN follows sf ON sf.account_id = l.account_id AND sf.target_account_id = $1
         JOIN follows tf ON tf.account_id = l.account_id AND tf.target_account_id = $2
         WHERE la.account_id = $1
         ON CONFLICT DO NOTHING",
        source.id,
        target.id,
    )
    .execute(&state.db)
    .await?;

    // Finally the common case, local accounts not following the new
    // account: their follow, and the list memberships riding on it, move.
    sqlx::query!(
        "UPDATE list_accounts la SET account_id = $2
         FROM lists l, accounts owner
         WHERE la.list_id = l.id AND la.account_id = $1
           AND owner.id = l.account_id AND owner.domain IS NULL AND l.account_id <> $2
           AND EXISTS (SELECT 1 FROM follows f WHERE f.account_id = l.account_id AND f.target_account_id = $1)
           AND NOT EXISTS (SELECT 1 FROM follows f WHERE f.account_id = l.account_id AND f.target_account_id = $2)
           AND NOT EXISTS (SELECT 1 FROM list_accounts x WHERE x.list_id = la.list_id AND x.account_id = $2)",
        source.id,
        target.id,
    )
    .execute(&state.db)
    .await?;
    let moved = sqlx::query!(
        "UPDATE follows f SET target_account_id = $2
         FROM accounts follower
         WHERE follower.id = f.account_id AND follower.domain IS NULL
           AND f.target_account_id = $1 AND f.account_id <> $2
           AND NOT EXISTS (SELECT 1 FROM follows x WHERE x.account_id = f.account_id AND x.target_account_id = $2)",
        source.id,
        target.id,
    )
    .execute(&state.db)
    .await?
    .rows_affected();
    Ok(moved as i64)
}

/// `MoveWorker#queue_follow_unfollows!`: every local follower of the old
/// account follows the new one (`UnfollowFollowWorker`), straight away when
/// the new account is local.
async fn queue_follow_unfollows(
    state: &AppState,
    source: &Account,
    target: &Account,
    deferred: &mut Option<AppError>,
) -> AppResult<()> {
    let bypass_locked = target.domain.is_none();
    let followers: Vec<i64> = sqlx::query_scalar!(
        "SELECT f.account_id FROM follows f JOIN accounts a ON a.id = f.account_id
         WHERE f.target_account_id = $1 AND a.domain IS NULL
         ORDER BY f.account_id",
        source.id,
    )
    .fetch_all(&state.db)
    .await?;
    for follower_id in followers {
        let Some(follower) = load_account(state, follower_id).await? else {
            continue;
        };
        match follow_migration(state, &follower, target, source, bypass_locked).await {
            // `rescue ActiveRecord::RecordNotFound, Mastodon::NotPermittedError`.
            Ok(()) | Err(AppError::NotFound | AppError::Forbidden) => {}
            Err(error) => {
                tracing::warn!(follower_id, %error, "could not move a follower");
                deferred.get_or_insert(error);
            }
        }
    }
    Ok(())
}

/// `FollowMigrationService`: `follower` follows `target` with the options it
/// followed `old_target` with, the lists that held the old account gain the
/// new one, and once the new follow is made or asked for the old one ends.
async fn follow_migration(
    state: &AppState,
    follower: &Account,
    target: &Account,
    old_target: &Account,
    bypass_locked: bool,
) -> AppResult<()> {
    let original = sqlx::query!(
        "SELECT show_reblogs, notify, languages FROM follows
         WHERE account_id = $1 AND target_account_id = $2",
        follower.id,
        old_target.id,
    )
    .fetch_optional(&state.db)
    .await?;
    let outcome = relationships::follow(
        state,
        follower,
        target,
        relationships::FollowOptions {
            reblogs: original.as_ref().map(|f| f.show_reblogs),
            notify: original.as_ref().map(|f| f.notify),
            languages: original.as_ref().and_then(|f| f.languages.clone()),
            bypass_locked,
            bypass_limit: true,
        },
    )
    .await?;
    if original.is_some() {
        migrate_list_accounts(state, follower.id, old_target.id, target.id).await?;
    }
    if matches!(
        outcome,
        relationships::FollowOutcome::Requested | relationships::FollowOutcome::Followed
    ) {
        relationships::unfollow(state, follower.id, old_target.id, true).await?;
    }
    Ok(())
}

/// `FollowMigrationService#migrate_list_accounts!`: `list.accounts <<
/// target` for each of the owner's lists holding the old account, tied to
/// the owner's follow of the new account or request for it.
async fn migrate_list_accounts(
    state: &AppState,
    owner_id: i64,
    old_target_id: i64,
    target_id: i64,
) -> AppResult<()> {
    sqlx::query!(
        "INSERT INTO list_accounts (list_id, account_id, follow_id, follow_request_id)
         SELECT la.list_id, $3, f.id, CASE WHEN f.id IS NULL THEN fr.id END
         FROM list_accounts la
         JOIN lists l ON l.id = la.list_id AND l.account_id = $1
         LEFT JOIN follows f ON f.account_id = $1 AND f.target_account_id = $3
         LEFT JOIN follow_requests fr ON fr.account_id = $1 AND fr.target_account_id = $3
         WHERE la.account_id = $2 AND (f.id IS NOT NULL OR fr.id IS NOT NULL)
         ON CONFLICT DO NOTHING",
        owner_id,
        old_target_id,
        target_id,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// The `move_handler.*` texts, in the note author's locale.
fn move_handler_text(locale: Option<&str>, key: &str, acct: &str) -> String {
    let korean = locale.is_some_and(|l| l.starts_with("ko"));
    let template = match (key, korean) {
        ("copy_account_note_text", false) => {
            "This user moved from %{acct}, here were your previous notes about them:"
        }
        ("copy_account_note_text", true) => {
            "이 사용자는 %{acct}로부터 이동하였습니다. 당신의 이전 노트는 이렇습니다:"
        }
        ("carry_blocks_over_text", false) => "This user moved from %{acct}, which you had blocked.",
        ("carry_blocks_over_text", true) => "이 사용자는 예전에 차단한 %{acct}에서 이주 했습니다.",
        ("carry_mutes_over_text", false) => "This user moved from %{acct}, which you had muted.",
        ("carry_mutes_over_text", true) => "이 사용자는 예전에 뮤트한 %{acct}에서 이주 했습니다.",
        _ => "%{acct}",
    };
    template.replace("%{acct}", acct)
}

/// `MoveWorker#copy_account_notes!`: each note about the old account is
/// copied onto the new one, under a line saying where it came from, and
/// joined to a note the author already had there.
async fn copy_account_notes(
    state: &AppState,
    source: &Account,
    target: &Account,
    deferred: &mut Option<AppError>,
) -> AppResult<()> {
    let notes = sqlx::query!(
        "SELECT n.account_id, n.comment, u.locale AS \"locale?\"
         FROM account_notes n LEFT JOIN users u ON u.account_id = n.account_id
         WHERE n.target_account_id = $1",
        source.id,
    )
    .fetch_all(&state.db)
    .await?;
    for note in notes {
        let text = move_handler_text(
            note.locale.as_deref(),
            "copy_account_note_text",
            &source.acct(),
        );
        let existing: Option<String> = sqlx::query_scalar!(
            "SELECT comment FROM account_notes WHERE account_id = $1 AND target_account_id = $2",
            note.account_id,
            target.id,
        )
        .fetch_optional(&state.db)
        .await?;
        let comment = match existing {
            None => {
                let joined = [text.as_str(), note.comment.as_str()].join("\n");
                if joined.chars().count() <= NOTE_LIMIT {
                    joined
                } else {
                    note.comment.clone()
                }
            }
            Some(existing) => [
                text.as_str(),
                note.comment.as_str(),
                "\n",
                existing.as_str(),
            ]
            .join("\n"),
        };
        // `rescue ActiveRecord::RecordInvalid; nil`.
        if comment.chars().count() > NOTE_LIMIT {
            continue;
        }
        if let Err(error) = sqlx::query!(
            r#"INSERT INTO account_notes (account_id, target_account_id, comment, created_at, updated_at)
               VALUES ($1, $2, $3, now(), now())
               ON CONFLICT (account_id, target_account_id)
               DO UPDATE SET comment = EXCLUDED.comment, updated_at = now()"#,
            note.account_id,
            target.id,
            comment,
        )
        .execute(&state.db)
        .await
        {
            deferred.get_or_insert(error.into());
        }
    }
    Ok(())
}

/// `MoveWorker#carry_blocks_over!`: a local account that blocked the old
/// account blocks the new one, unless it already blocks or follows it.
async fn carry_blocks_over(
    state: &AppState,
    source: &Account,
    target: &Account,
    deferred: &mut Option<AppError>,
) -> AppResult<()> {
    let blockers = sqlx::query!(
        r#"SELECT b.account_id, u.locale AS "locale?"
           FROM blocks b
           JOIN accounts a ON a.id = b.account_id AND a.domain IS NULL
           LEFT JOIN users u ON u.account_id = b.account_id
           WHERE b.target_account_id = $1"#,
        source.id,
    )
    .fetch_all(&state.db)
    .await?;
    for blocker in blockers {
        // `skip_block_move?`.
        let skip = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = $2)
                   OR EXISTS (SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)
               AS "skip!""#,
            blocker.account_id,
            target.id,
        )
        .fetch_one(&state.db)
        .await?;
        if skip {
            continue;
        }
        let result = async {
            relationships::block(state, blocker.account_id, target.id).await?;
            add_account_note_if_needed(
                state,
                blocker.account_id,
                blocker.locale.as_deref(),
                source,
                target,
                "carry_blocks_over_text",
            )
            .await
        }
        .await;
        if let Err(error) = result {
            deferred.get_or_insert(error);
        }
    }
    Ok(())
}

/// `MoveWorker#carry_mutes_over!`: likewise for mutes, keeping whether the
/// mute hid notifications.
async fn carry_mutes_over(
    state: &AppState,
    source: &Account,
    target: &Account,
    deferred: &mut Option<AppError>,
) -> AppResult<()> {
    let muters = sqlx::query!(
        r#"SELECT m.account_id, m.hide_notifications, u.locale AS "locale?"
           FROM mutes m
           JOIN accounts a ON a.id = m.account_id AND a.domain IS NULL
           LEFT JOIN users u ON u.account_id = m.account_id
           WHERE m.target_account_id = $1"#,
        source.id,
    )
    .fetch_all(&state.db)
    .await?;
    for muter in muters {
        // `skip_mute_move?`.
        let skip = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM mutes WHERE account_id = $1 AND target_account_id = $2)
                   OR EXISTS (SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)
               AS "skip!""#,
            muter.account_id,
            target.id,
        )
        .fetch_one(&state.db)
        .await?;
        if skip {
            continue;
        }
        let result = async {
            relationships::mute(
                state,
                muter.account_id,
                target.id,
                muter.hide_notifications,
                0,
            )
            .await?;
            add_account_note_if_needed(
                state,
                muter.account_id,
                muter.locale.as_deref(),
                source,
                target,
                "carry_mutes_over_text",
            )
            .await
        }
        .await;
        if let Err(error) = result {
            deferred.get_or_insert(error);
        }
    }
    Ok(())
}

/// `MoveWorker#add_account_note_if_needed!`: say why the new account is
/// blocked or muted, unless the author already has a note on it.
async fn add_account_note_if_needed(
    state: &AppState,
    account_id: i64,
    locale: Option<&str>,
    source: &Account,
    target: &Account,
    key: &str,
) -> AppResult<()> {
    let text = move_handler_text(locale, key, &source.acct());
    sqlx::query!(
        r#"INSERT INTO account_notes (account_id, target_account_id, comment, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())
           ON CONFLICT (account_id, target_account_id) DO NOTHING"#,
        account_id,
        target.id,
        text,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acct_is_normalized_as_upstream() {
        assert_eq!(normalize_acct("  @alice@example.com "), "alice@example.com");
        assert_eq!(normalize_acct("alice"), "alice");
    }

    #[test]
    fn acct_domains_are_validated() {
        assert!(acct_domain_valid("alice"));
        assert!(acct_domain_valid("alice@example.com"));
        assert!(acct_domain_valid("alice@"));
        assert!(!acct_domain_valid("alice@exa_mple.com"));
        assert!(!acct_domain_valid("alice@bad..com"));
    }

    #[test]
    fn notes_are_written_in_the_authors_language() {
        assert_eq!(
            move_handler_text(Some("en"), "carry_blocks_over_text", "old@example.com"),
            "This user moved from old@example.com, which you had blocked."
        );
        assert_eq!(
            move_handler_text(Some("ko"), "carry_mutes_over_text", "old"),
            "이 사용자는 예전에 뮤트한 old에서 이주 했습니다."
        );
        assert_eq!(
            move_handler_text(None, "copy_account_note_text", "old"),
            "This user moved from old, here were your previous notes about them:"
        );
    }
}
