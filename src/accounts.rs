//! Creating local accounts.
//!
//! Mastodon makes one by saving a `User` together with its `Account`, whether
//! the save comes from a confirmed sign-up or from `tootctl accounts create`.
//! Both go through [`create_local`] here, so an account made on the command line
//! has the same shape as one that signed up.

use anyhow::{anyhow, bail, Context, Result};
use sqlx::PgPool;

use crate::{config::InstanceConfig, rails_encryption::Encryptor};

/// Mastodon's `Account::USERNAME_LENGTH_LIMIT`, which applies to local accounts.
const USERNAME_LENGTH_LIMIT: usize = 30;

/// The `User` half of a new local account.
pub struct NewLocalUser<'a> {
    pub username: &'a str,
    pub email: &'a str,
    pub password_hash: &'a str,
    pub role_id: Option<i64>,
    pub approved: bool,
    pub invite_id: Option<i64>,
    pub locale: Option<&'a str>,
    pub app_id: Option<i64>,
}

/// The rows a new local account was written as.
pub struct LocalUser {
    pub account_id: i64,
    pub user_id: i64,
}

/// Write a confirmed local account and its user, with a fresh signing key.
///
/// The account, its key and its user are written in one transaction, so a
/// failure part way leaves no account without a user holding the username.
pub async fn create_local(
    db: &PgPool,
    encryptor: Option<&Encryptor>,
    domain: &str,
    user: NewLocalUser<'_>,
) -> Result<LocalUser> {
    // A 2048-bit key is on the order of a hundred milliseconds of CPU.
    let (private_key, public_key) =
        crate::tenants::spawn_blocking(crate::crypto::generate_rsa_keypair)
            .await
            .context("generating a signing key did not finish")??;

    let url = format!("https://{}/@{}", domain, user.username);
    let new_account_id = crate::snowflake::next_id();
    // New local accounts use Mastodon's default `numeric_ap_id` scheme: the
    // ActivityPub actor is served at /ap/users/{id}. Build the canonical URI
    // (and its inbox/outbox) from the new account id.
    let uri = crate::federation::tag::account_uri(
        domain,
        new_account_id,
        Some(crate::federation::tag::NUMERIC_AP_ID),
        user.username,
    );

    let mut tx = db.begin().await?;
    let account_id = sqlx::query_scalar!(
        r#"INSERT INTO accounts
             (id, username, url, uri, private_key, public_key,
              inbox_url, outbox_url, shared_inbox_url, id_scheme, created_at, updated_at)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9, 1, now(), now())
           RETURNING id"#,
        new_account_id,
        user.username,
        url,
        uri,
        private_key,
        public_key,
        format!("{}/inbox", uri),
        format!("{}/outbox", uri),
        format!("https://{}/inbox", domain),
    )
    .fetch_one(&mut *tx)
    .await?;

    // Written to `accounts` above so that the account is never keyless; move it
    // to `keypairs` when this instance keeps signing keys there.
    if let Some(encryptor) = encryptor {
        crate::federation::keypair::store_sealed(
            &mut tx,
            encryptor,
            account_id,
            &private_key,
            &public_key,
        )
        .await?;
    }

    let user_id = sqlx::query_scalar!(
        r#"INSERT INTO users
             (account_id, email, encrypted_password, role_id,
              confirmed_at, invite_id, approved,
              locale, created_by_application_id, created_at, updated_at)
           VALUES ($1,$2,$3,$4,
                   now(), $5, $6,
                   $7, $8, now(), now())
           RETURNING id"#,
        account_id,
        user.email,
        user.password_hash,
        user.role_id,
        user.invite_id,
        user.approved,
        user.locale,
        user.app_id,
    )
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(LocalUser {
        account_id,
        user_id,
    })
}

/// What `eunha accounts create` was asked for.
pub struct CreateOptions {
    pub username: String,
    pub email: String,
    pub role: Option<String>,
    pub confirmed: bool,
    pub approve: bool,
}

/// `tootctl accounts create`: make a local account outside the sign-up flow,
/// and return the random password it was given.
///
/// Like upstream it bypasses the registration checks — whether sign-ups are
/// open, and the username and email blocks — but not the account's own
/// validations: the username's format, length and uniqueness, and the email's.
/// Without `approve`, the account is approved exactly when a sign-up would be,
/// which is `User#set_approved` on an open, approval-free instance.
pub async fn create_from_command(
    db: &PgPool,
    encryptor: Option<&Encryptor>,
    instance: &InstanceConfig,
    options: CreateOptions,
) -> Result<String> {
    // Mastodon leaves an account created without `--confirmed` unconfirmed and
    // mails its owner a confirmation link. eunha holds unconfirmed sign-ups in
    // `eunha.pending_signups` rather than `users`, keyed by the link it mails,
    // so there is no such account to make without sending mail.
    if !options.confirmed {
        bail!("eunha can only create confirmed accounts; pass --confirmed");
    }

    let username = options.username.trim();
    if username.is_empty()
        || !username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        bail!("username must contain only letters, numbers and underscores");
    }
    if username.chars().count() > USERNAME_LENGTH_LIMIT {
        bail!("username is too long (maximum is {USERNAME_LENGTH_LIMIT} characters)");
    }

    // Devise strips and downcases the email before validating or saving it.
    let email = options.email.trim().to_lowercase();
    if !valid_email(&email) {
        bail!("email is invalid");
    }

    let role_id = match options.role.as_deref() {
        None => None,
        Some(name) => Some(
            sqlx::query_scalar!("SELECT id FROM user_roles WHERE name = $1 LIMIT 1", name)
                .fetch_optional(db)
                .await?
                .ok_or_else(|| anyhow!("cannot find user role with that name"))?,
        ),
    };

    let username_taken = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM accounts WHERE lower(username) = lower($1) AND domain IS NULL
           ) AS "exists!""#,
        username,
    )
    .fetch_one(db)
    .await?;
    if username_taken {
        bail!("username has already been taken");
    }
    let email_taken = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM users WHERE lower(email) = $1) AS "exists!""#,
        email,
    )
    .fetch_one(db)
    .await?;
    if email_taken {
        bail!("email has already been taken");
    }

    // `SecureRandom.hex`: 16 random bytes, written as 32 hex digits.
    let password = crate::crypto::generate_token(16);
    let password_hash = crate::crypto::hash_password(&password)
        .await
        .map_err(|e| anyhow!("hashing the password: {e}"))?;

    create_local(
        db,
        encryptor,
        &instance.domain,
        NewLocalUser {
            username,
            email: &email,
            password_hash: &password_hash,
            role_id,
            approved: options.approve
                || (instance.registrations_open && !instance.approval_required),
            invite_id: None,
            locale: None,
            app_id: None,
        },
    )
    .await?;

    Ok(password)
}

/// `tootctl accounts modify --reset-password`: give a local account a new
/// random password, sign it out everywhere, and return the password.
///
/// This is `User#change_password!`, as upstream's command runs it: the password
/// and the account's session activations change together, then every
/// authorization it granted is revoked and the push subscriptions made through
/// them go. A streaming connection already open in a running server stays open
/// until it next reconnects, since the server that holds it is another process.
pub async fn reset_password(db: &PgPool, username: &str) -> Result<String> {
    let user_id = sqlx::query_scalar!(
        r#"SELECT u.id FROM users u JOIN accounts a ON a.id = u.account_id
           WHERE lower(a.username) = lower($1) AND a.domain IS NULL"#,
        username,
    )
    .fetch_optional(db)
    .await?
    .ok_or_else(|| anyhow!("no user with such username"))?;

    // `SecureRandom.hex`: 16 random bytes, written as 32 hex digits.
    let password = crate::crypto::generate_token(16);
    let password_hash = crate::crypto::hash_password(&password)
        .await
        .map_err(|e| anyhow!("hashing the password: {e}"))?;

    let mut tx = db.begin().await?;
    sqlx::query!(
        r#"UPDATE users
           SET encrypted_password = $1,
               reset_password_token = NULL, reset_password_sent_at = NULL,
               updated_at = now()
           WHERE id = $2"#,
        password_hash,
        user_id,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM session_activations WHERE user_id = $1",
        user_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE oauth_access_grants SET revoked_at = now() WHERE resource_owner_id = $1 AND revoked_at IS NULL",
        user_id,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"DELETE FROM web_push_subscriptions
           WHERE access_token_id IN (SELECT id FROM oauth_access_tokens WHERE resource_owner_id = $1)"#,
        user_id,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE oauth_access_tokens SET revoked_at = now() WHERE resource_owner_id = $1 AND revoked_at IS NULL",
        user_id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(password)
}

/// One `@` between a non-empty local part and a domain, with none of the
/// characters Mastodon's `EmailAddressValidator` refuses outright (`%`, `,`,
/// `"`) and no whitespace — the shape it accepts, short of parsing the address
/// as the `mail` gem does.
fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && !email
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '%' | ',' | '"'))
}

/// Send `account`'s profile, as an `Update` of its actor, to every server
/// that knows it: its followers', and the others Mastodon's reach finder
/// counts. What an edit to the profile does, and what tells other servers
/// where its avatar and header now are after the media has moved. Returns
/// how many inboxes it was queued for; nothing for an account that cannot
/// sign or is not local.
pub async fn distribute_profile(
    state: &crate::state::AppState,
    domain: &str,
    account: &crate::db::models::Account,
    batch: Option<&feder::deliverer::Batch>,
) -> anyhow::Result<u64> {
    if account.domain.is_some()
        || !crate::federation::keypair::has_signing_key(state, account.id)
            .await
            .unwrap_or(false)
    {
        return Ok(0);
    }
    let actor_url = crate::federation::tag::account_uri_of(domain, account);
    let actor = crate::api::ap::objects::actor_json(state, domain, account)
        .await
        .map_err(|e| anyhow::anyhow!("building the actor: {e:?}"))?;
    // A new id each time: a server that has seen an activity's id drops it
    // again, and this may be sent twice for one profile.
    let update_id = format!(
        "{actor_url}#updates/{}",
        chrono::Utc::now().timestamp_millis()
    );
    let activity = crate::federation::activity::update_actor(&update_id, &actor_url, actor)?;
    let inboxes = crate::federation::delivery::account_reach_inboxes(state, account.id).await?;
    let key_id = format!("{actor_url}#main-key");
    match batch {
        Some(batch) => {
            crate::federation::delivery::deliver_to_inboxes_in_batch(
                state, activity, inboxes, key_id, batch,
            )
            .await
        }
        None => {
            crate::federation::delivery::deliver_to_inboxes(state, activity, inboxes, key_id).await
        }
    }
}

/// Which local accounts a batch acts on.
#[derive(Clone, Debug)]
pub enum Selection {
    /// Every local account that is not suspended or being deleted.
    All,
    /// These, by username.
    Usernames(Vec<String>),
}

/// What a batch did, or with `dry_run`, would do: for each account, how many
/// inboxes it was queued for, or why it was passed over.
#[derive(Debug, Default)]
pub struct BatchReport {
    pub sent: Vec<(String, u64)>,
    pub skipped: Vec<(String, &'static str)>,
    pub unknown: Vec<String>,
}

/// The local accounts `selection` names, and the usernames it names that are
/// not local accounts.
pub async fn select(
    db: &sqlx::PgPool,
    selection: &Selection,
) -> anyhow::Result<(Vec<crate::db::models::Account>, Vec<String>)> {
    let names: Option<Vec<String>> = match selection {
        Selection::All => None,
        Selection::Usernames(names) => Some(names.clone()),
    };
    let accounts: Vec<crate::db::models::Account> = sqlx::query_as(
        "SELECT * FROM accounts
         WHERE domain IS NULL AND id > 0
           AND suspended_at IS NULL AND requested_deletion_at IS NULL
           AND ($1::text[] IS NULL OR username = ANY($1))
         ORDER BY id",
    )
    .bind(&names)
    .fetch_all(db)
    .await?;
    let unknown = names
        .unwrap_or_default()
        .into_iter()
        .filter(|name| !accounts.iter().any(|account| &account.username == name))
        .collect();
    Ok((accounts, unknown))
}

/// Send the profiles of the accounts `selection` names to the servers that
/// know them, as [`distribute_profile`] does for one. With `dry_run`, only
/// report what would be sent.
pub async fn update_profiles(
    state: &crate::state::AppState,
    selection: &Selection,
    batch: &feder::deliverer::Batch,
    dry_run: bool,
) -> anyhow::Result<BatchReport> {
    let (accounts, unknown) = select(&state.db, selection).await?;
    let domain = state.instance.domain.clone();
    let mut report = BatchReport {
        unknown,
        ..BatchReport::default()
    };
    for account in &accounts {
        if !crate::federation::keypair::has_signing_key(state, account.id)
            .await
            .unwrap_or(false)
        {
            report
                .skipped
                .push((account.username.clone(), "no signing key"));
            continue;
        }
        let queued = if dry_run {
            crate::federation::delivery::account_reach_inboxes(state, account.id)
                .await?
                .len() as u64
        } else {
            distribute_profile(state, &domain, account, Some(batch)).await?
        };
        report.sent.push((account.username.clone(), queued));
    }
    Ok(report)
}

/// Move the followers of the accounts `selection` names from their actors
/// under `from`, a domain the instance had before, to their actors now: a
/// `Move` from each old actor to its new one, signed with the old actor's key
/// id, to its followers' servers. Each account also follows again, from its
/// new actor, the remote accounts it followed, whose servers would otherwise
/// go on delivering to the old one. They hold that key from when they followed,
/// so nothing has to be served under `from`; each fetches the new actor,
/// finds the old one in its `alsoKnownAs`, and follows it there. With
/// `dry_run`, only report what would be sent.
///
/// # Errors
///
/// When `from` is not among the instance's `previous_domains`: the new actors
/// would not list the old ones, and every server would refuse the Move.
pub async fn move_followers(
    state: &crate::state::AppState,
    selection: &Selection,
    from: &str,
    batch: &feder::deliverer::Batch,
    dry_run: bool,
) -> anyhow::Result<BatchReport> {
    anyhow::ensure!(
        state.instance.previous_domains.iter().any(|d| d == from),
        "{from} is not in instance.previous_domains, so no actor lists its old id there \
         and every server would refuse the Move; add it and restart first"
    );
    let (accounts, unknown) = select(&state.db, selection).await?;
    let domain = state.instance.domain.clone();
    let mut report = BatchReport {
        unknown,
        ..BatchReport::default()
    };
    for account in &accounts {
        if !crate::federation::keypair::has_signing_key(state, account.id)
            .await
            .unwrap_or(false)
        {
            report
                .skipped
                .push((account.username.clone(), "no signing key"));
            continue;
        }
        let old = crate::federation::tag::account_uri(
            from,
            account.id,
            account.id_scheme,
            &account.username,
        );
        let new = crate::federation::tag::account_uri_of(&domain, account);
        let following = remote_follows(state, account.id).await?;
        let queued = if dry_run {
            crate::federation::delivery::follower_inboxes(state, account.id)
                .await?
                .len() as u64
                + following.len() as u64
        } else {
            // The servers of the accounts this one follows still send their
            // posts to the old actor's inbox; following again from the new
            // actor is what brings them here. A Move carries only followers.
            let key_id = format!("{new}#main-key");
            let mut refollowed = 0;
            for (table, id, target, inbox) in &following {
                let follow_id = format!("{new}#follows/{}", crate::snowflake::next_id());
                let activity = crate::federation::activity::follow(&follow_id, &new, target)?;
                refollowed += crate::federation::delivery::deliver_to_inboxes_in_batch(
                    state,
                    activity,
                    vec![inbox.clone()],
                    key_id.clone(),
                    batch,
                )
                .await?;
                sqlx::query(&format!("UPDATE {table} SET uri = $2 WHERE id = $1"))
                    .bind(id)
                    .bind(&follow_id)
                    .execute(&state.db)
                    .await?;
            }
            let activity = crate::federation::activity::move_actor(
                &format!("{old}#moves/{}", crate::snowflake::next_id()),
                &old,
                &old,
                &new,
            );
            let moved = crate::federation::delivery::fanout_to_followers_unproven(
                state,
                activity,
                account.id,
                format!("{old}#main-key"),
                Some(batch),
            )
            .await?;
            moved + refollowed
        };
        report.sent.push((account.username.clone(), queued));
    }
    Ok(report)
}

/// The remote accounts `account_id` follows or has asked to follow: which
/// table, the row, the account's actor id, and the inbox to send to.
async fn remote_follows(
    state: &crate::state::AppState,
    account_id: i64,
) -> anyhow::Result<Vec<(&'static str, i64, String, String)>> {
    let mut found = Vec::new();
    for table in ["follows", "follow_requests"] {
        let rows: Vec<(i64, String, String)> = sqlx::query_as(&format!(
            "SELECT f.id, a.uri,
                    CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url ELSE a.inbox_url END
             FROM {table} f JOIN accounts a ON a.id = f.target_account_id
             WHERE f.account_id = $1 AND a.domain IS NOT NULL
               AND a.inbox_url <> '' AND a.uri <> '' AND a.suspended_at IS NULL"
        ))
        .bind(account_id)
        .fetch_all(&state.db)
        .await?;
        found.extend(
            rows.into_iter()
                .map(|(id, uri, inbox)| (table, id, uri, inbox)),
        );
    }
    Ok(found)
}

/// Where a batch's deliveries stand: how many are still to be tried, and
/// which were given up on and why. A delivery that went through leaves the
/// queue, so what a batch queued less these is what was delivered.
#[derive(Debug, Default)]
pub struct BatchStatus {
    pub pending: u64,
    pub failed: Vec<(String, String)>,
}

/// Where the deliveries tagged `tag` stand.
pub async fn batch_status(db: &sqlx::PgPool, tag: &str) -> anyhow::Result<BatchStatus> {
    let rows: Vec<(String, bool, Option<String>)> = sqlx::query_as(
        "SELECT payload->>'inbox', failed_at IS NOT NULL, last_error
         FROM eunha.feder_queue
         WHERE queue = 'delivery' AND payload->>'tag' = $1
         ORDER BY id",
    )
    .bind(tag)
    .fetch_all(db)
    .await?;
    let mut status = BatchStatus::default();
    for (inbox, failed, error) in rows {
        if failed {
            status.failed.push((inbox, error.unwrap_or_default()));
        } else {
            status.pending += 1;
        }
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::valid_email;

    #[test]
    fn test_valid_email_requires_one_at_between_two_parts() {
        assert!(valid_email("owner@example.com"));
        assert!(!valid_email("owner"));
        assert!(!valid_email("@example.com"));
        assert!(!valid_email("owner@"));
        assert!(!valid_email("owner@example@com"));
        assert!(!valid_email("own er@example.com"));
        assert!(!valid_email("owner%relay@example.com"));
    }
}
