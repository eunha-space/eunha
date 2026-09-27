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
    crate::federation::delivery::deliver_to_inboxes(
        state,
        activity,
        inboxes,
        format!("{actor_url}#main-key"),
    )
    .await
}

/// [`distribute_profile`] for every local account that is not suspended, or
/// the one named `username`. Returns how many accounts and inboxes.
pub async fn distribute_profiles(
    state: &crate::state::AppState,
    username: Option<&str>,
) -> anyhow::Result<(u64, u64)> {
    let accounts: Vec<crate::db::models::Account> = sqlx::query_as(
        "SELECT * FROM accounts
         WHERE domain IS NULL AND id > 0 AND suspended_at IS NULL
           AND ($1::text IS NULL OR username = $1)
         ORDER BY id",
    )
    .bind(username)
    .fetch_all(&state.db)
    .await?;
    let domain = state.instance.domain.clone();
    let (mut sent, mut inboxes) = (0, 0);
    for account in &accounts {
        let queued = distribute_profile(state, &domain, account).await?;
        if queued > 0 {
            sent += 1;
            inboxes += queued;
        }
    }
    Ok((sent, inboxes))
}
