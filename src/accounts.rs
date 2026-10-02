//! Creating local accounts.
//!
//! Mastodon makes one by saving a `User` together with its `Account`, whether
//! the save comes from a confirmed sign-up or from `tootctl accounts create`.
//! Both go through [`create_local`] here, so an account made on the command line
//! has the same shape as one that signed up.

use anyhow::{anyhow, bail, Context, Result};
use sqlx::PgPool;

use crate::{
    api::ap::serving::{AccountUris, Own},
    config::InstanceConfig,
    rails_encryption::Encryptor,
};

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
    /// `users.sign_up_ip`.
    pub sign_up_ip: Option<std::net::IpAddr>,
    /// `invite_request`: the reason given for joining, as a
    /// `user_invite_requests` row.
    pub invite_request: Option<&'a str>,
    /// `users.time_zone`, already normalized.
    pub time_zone: Option<&'a str>,
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
    let uris = crate::api::ap::serving::uris(domain)?;
    let own = AccountUris::new(
        &uris,
        new_account_id,
        Some(crate::federation::tag::NUMERIC_AP_ID),
        user.username,
    );
    let uri = own.actor()?.to_string();
    let inbox_url = own.uri(Own::Inbox)?;
    let outbox_url = own.uri(Own::Outbox)?;
    let shared_inbox_url = uris.shared_inbox_uri()?;

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
        inbox_url.as_str(),
        outbox_url.as_str(),
        shared_inbox_url.as_str(),
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

    // `User#set_age_verified_at`: when the instance asks for an age, every
    // user it creates has had theirs checked.
    let age_verified = crate::settings::min_age(db).await.is_some();
    let user_id = sqlx::query_scalar!(
        r#"INSERT INTO users
             (account_id, email, encrypted_password, role_id,
              confirmed_at, invite_id, approved,
              locale, created_by_application_id, sign_up_ip, age_verified_at,
              time_zone, created_at, updated_at)
           VALUES ($1,$2,$3,$4,
                   now(), $5, $6,
                   $7, $8, $9::text::inet, CASE WHEN $10 THEN now() END,
                   $11, now(), now())
           RETURNING id"#,
        account_id,
        user.email,
        user.password_hash,
        user.role_id,
        user.invite_id,
        user.approved,
        user.locale,
        user.app_id,
        user.sign_up_ip.map(|ip| ip.to_string()),
        age_verified,
        user.time_zone,
    )
    .fetch_one(&mut *tx)
    .await?;

    if let Some(text) = user.invite_request.filter(|t| !t.is_empty()) {
        sqlx::query!(
            r#"INSERT INTO user_invite_requests (user_id, text, created_at, updated_at)
               VALUES ($1, $2, now(), now())"#,
            user_id,
            text,
        )
        .execute(&mut *tx)
        .await?;
    }

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
                || crate::settings::registrations_mode_in(db, instance)
                    .await
                    .open(),
            invite_id: None,
            locale: None,
            app_id: None,
            sign_up_ip: None,
            invite_request: None,
            time_zone: None,
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
    change_password(db, user_id).await
}

/// `User#change_password!(SecureRandom.hex)`: a new random password, every
/// session and authorization of the user gone with it. Returns the password.
pub async fn change_password(db: &PgPool, user_id: i64) -> Result<String> {
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

/// Devise's `reset_password_within`.
const RESET_PASSWORD_WITHIN_HOURS: i32 = 6;

/// What `users.reset_password_token` holds for a token mailed out, so the
/// column alone cannot reset anyone's password: Devise's HMAC
/// (`Devise.token_generator.digest`) when `secret_key_base` is configured,
/// and the token's SHA-256 when it is not.
async fn reset_password_digest(state: &crate::state::AppState, token: &str) -> String {
    match &state.instance.secret_key_base {
        Some(secret) => secret.reset_password_token_digest(token).await,
        None => reset_password_sha256(token),
    }
}

fn reset_password_sha256(token: &str) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(token.as_bytes()))
}

/// `User#send_reset_password_instructions`: a reset token, and a mail with the
/// link that uses it. Nothing for a user without a password
/// (`encrypted_password.blank?`), nor, as `UserMailer` holds back, for a
/// memorial account.
pub async fn send_reset_password_instructions(
    state: &crate::state::AppState,
    user_id: i64,
) -> Result<()> {
    let Some(user) = sqlx::query!(
        r#"SELECT u.email, u.encrypted_password, u.locale, a.username, a.memorial
           FROM users u JOIN accounts a ON a.id = u.account_id WHERE u.id = $1"#,
        user_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    if user.encrypted_password.is_empty() {
        return Ok(());
    }
    // `set_reset_password_token`: `Devise.friendly_token`, stored as its digest.
    let token = crate::email_subscriptions::friendly_token();
    sqlx::query!(
        "UPDATE users SET reset_password_token = $1, reset_password_sent_at = now() WHERE id = $2",
        reset_password_digest(state, &token).await,
        user_id,
    )
    .execute(&state.db)
    .await?;
    if user.memorial {
        return Ok(());
    }
    // `edit_password_url(reset_password_token:)`.
    let url = format!(
        "https://{}/auth/password/edit?reset_password_token={token}",
        state.instance.domain
    );
    let email = state.email.clone();
    let locale = user.locale.unwrap_or_else(|| "en".into());
    crate::tenants::spawn(async move {
        if let Err(error) = email
            .send_password_reset(&user.email, &user.username, &url, &locale)
            .await
        {
            tracing::error!(%error, "failed to send password reset email");
        }
    });
    Ok(())
}

/// The user a mailed reset token belongs to, if it is still good:
/// `with_reset_password_token` and `reset_password_period_valid?`.
pub async fn reset_password_user(
    state: &crate::state::AppState,
    token: &str,
) -> Result<i64, &'static str> {
    if token.is_empty() {
        return Err("Reset password token can't be blank");
    }
    // A SHA-256 is still read once `secret_key_base` is configured, so a link
    // eunha mailed before then keeps its six hours.
    let digests = vec![
        reset_password_digest(state, token).await,
        reset_password_sha256(token),
    ];
    let row = sqlx::query!(
        r#"SELECT id, reset_password_sent_at > now() - make_interval(hours => $2) AS "fresh!"
           FROM users
           WHERE reset_password_token = ANY($1) AND encrypted_password <> ''"#,
        &digests,
        RESET_PASSWORD_WITHIN_HOURS,
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|_| "Reset password token is invalid")?;
    match row {
        None => Err("Reset password token is invalid"),
        Some(r) if !r.fresh => Err("Reset password token has expired, please request a new one"),
        Some(r) => Ok(r.id),
    }
}

/// Devise's `password_length` and `validates_confirmation_of :password`, as
/// the full messages a form shows.
pub fn password_problem(password: &str, confirmation: Option<&str>) -> Option<&'static str> {
    let length = password.chars().count();
    if password.is_empty() {
        Some("Password can't be blank")
    } else if length < 8 {
        Some("Password is too short (minimum is 8 characters)")
    } else if length > 72 {
        Some("Password is too long (maximum is 72 characters)")
    } else if confirmation.is_some_and(|c| c != password) {
        Some("Password confirmation doesn't match Password")
    } else {
        None
    }
}

/// `Auth::PasswordsController#update`: Devise's `reset_password_by_token`,
/// then every session and authorization of the user ended, and the
/// `password_change` mail.
pub async fn reset_password_by_token(
    state: &crate::state::AppState,
    token: &str,
    password: &str,
    confirmation: Option<&str>,
) -> Result<i64, &'static str> {
    let user_id = reset_password_user(state, token).await?;
    if let Some(problem) = password_problem(password, confirmation) {
        return Err(problem);
    }
    let hash = crate::crypto::hash_password(password)
        .await
        .map_err(|_| "Could not set the password")?;
    sqlx::query!(
        r#"UPDATE users SET encrypted_password = $1, reset_password_token = NULL,
                  reset_password_sent_at = NULL, updated_at = now()
           WHERE id = $2"#,
        hash,
        user_id,
    )
    .execute(&state.db)
    .await
    .map_err(|_| "Could not set the password")?;
    if let Ok(tokens) = crate::sessions::destroy_all(&state.db, user_id).await {
        crate::sessions::kill_streams(state, tokens);
    }
    if let Err(error) = crate::sessions::revoke_access(state, user_id).await {
        tracing::warn!(%error, "could not revoke access after a password reset");
    }
    notify_password_change(state, user_id).await;
    Ok(user_id)
}

/// `send_confirmation_instructions`: a fresh confirmation token, mailed to the
/// address awaiting confirmation (`unconfirmed_email` when there is one, in
/// the `reconfirmation_instructions` template, as `pending_reconfirmation?`
/// picks it).
pub async fn send_confirmation_instructions(
    state: &crate::state::AppState,
    user_id: i64,
) -> Result<()> {
    let token = crate::crypto::generate_token(32);
    let Some(user) = sqlx::query!(
        r#"UPDATE users u SET confirmation_token = $2, confirmation_sent_at = now()
           FROM accounts a
           WHERE u.id = $1 AND a.id = u.account_id
           RETURNING COALESCE(NULLIF(u.unconfirmed_email, ''), u.email) AS "to!", u.locale,
                     a.username, (COALESCE(u.unconfirmed_email, '') <> '') AS "reconfirming!""#,
        user_id,
        token,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let url = format!(
        "https://{}/auth/confirm?token={token}",
        state.instance.domain
    );
    let email = state.email.clone();
    let locale = user.locale.unwrap_or_else(|| "en".into());
    let domain = state.instance.domain.clone();
    crate::tenants::spawn(async move {
        let sent = if user.reconfirming {
            email
                .send_reconfirmation_instructions(&user.to, &domain, &url)
                .await
        } else {
            email
                .send_confirmation(&user.to, &user.username, "", &url, &locale)
                .await
        };
        if let Err(error) = sent {
            tracing::error!(%error, "failed to send confirmation email");
        }
    });
    Ok(())
}

/// Devise's reconfirmable, short of the mail: a new address waits in
/// `unconfirmed_email`, the old confirmation token dropped, until the link
/// [`send_confirmation_instructions`] mails it is followed. What both the
/// admin's change of address and the member's own write.
pub async fn set_unconfirmed_email<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    user_id: i64,
    new_email: &str,
) -> sqlx::Result<()> {
    sqlx::query!(
        r#"UPDATE users SET unconfirmed_email = $2, confirmation_token = NULL,
                  updated_at = now()
           WHERE id = $1"#,
        user_id,
        new_email
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// `User#mark_email_as_confirmed!` and Devise's `confirm`, for a user who
/// exists: an address awaiting confirmation becomes the address, and a user
/// confirmed for the first time is welcomed in (or put before the staff, when
/// still awaiting approval), as `after_confirmation_tasks` does.
pub async fn confirm_user(
    state: &crate::state::AppState,
    user_id: i64,
    reconfirm: bool,
) -> Result<()> {
    let row = sqlx::query!(
        r#"UPDATE users u SET
             email = CASE WHEN $2 THEN COALESCE(lower(btrim(u.unconfirmed_email)), u.email) ELSE u.email END,
             unconfirmed_email = CASE WHEN $2 THEN NULL ELSE u.unconfirmed_email END,
             confirmed_at = COALESCE(u.confirmed_at, now()),
             confirmation_token = NULL,
             updated_at = now()
           FROM users before
           WHERE u.id = $1 AND before.id = u.id
           RETURNING u.account_id, u.approved, (before.confirmed_at IS NULL) AS "new_user!""#,
        user_id,
        reconfirm,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    if row.new_user {
        if row.approved {
            prepare_new_user(state, row.account_id).await;
        } else {
            let state = state.clone();
            let account_id = row.account_id;
            crate::tenants::spawn(async move {
                notify_staff_about_pending_account(&state, account_id).await;
            });
        }
    }
    Ok(())
}

/// One `@` between a non-empty local part and a domain, with none of the
/// characters Mastodon's `EmailAddressValidator` refuses outright (`%`, `,`,
/// `"`) and no whitespace — the shape it accepts, short of parsing the address
/// as the `mail` gem does.
pub fn valid_email(email: &str) -> bool {
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
    batch: Option<&ojak::deliverer::Batch>,
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
    let key_id = AccountUris::of(&state.uris, account).key_id()?.into();
    // `Account#sign?`: an actor's `Update` goes with its Linked Data
    // Signature, outside authorized fetch mode.
    let signed = crate::federation::delivery::LinkedData::UnlessAuthorizedFetch;
    match batch {
        Some(batch) => {
            crate::federation::delivery::deliver_to_inboxes_in_batch(
                state, activity, inboxes, key_id, signed, batch,
            )
            .await
        }
        None => {
            crate::federation::delivery::deliver_to_inboxes_signed(
                state, activity, inboxes, key_id, signed,
            )
            .await
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
    batch: &ojak::deliverer::Batch,
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
    batch: &ojak::deliverer::Batch,
    dry_run: bool,
) -> anyhow::Result<BatchReport> {
    anyhow::ensure!(
        state.instance.previous_domains.iter().any(|d| d == from),
        "{from} is not in instance.previous_domains, so no actor lists its old id there \
         and every server would refuse the Move; add it and restart first"
    );
    let (accounts, unknown) = select(&state.db, selection).await?;
    // The same URIs, in the domain the accounts' actors had before.
    let previous = crate::api::ap::serving::uris(from)?;
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
        let was = AccountUris::of(&previous, account);
        let now = AccountUris::of(&state.uris, account);
        let old = was.actor()?.to_string();
        let new = now.actor()?.to_string();
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
            let key_id: String = now.key_id()?.into();
            let mut refollowed = 0;
            for (table, id, target, inbox) in &following {
                let follow_id = format!("{new}#follows/{}", crate::snowflake::next_id());
                let activity = crate::federation::activity::follow(&follow_id, &new, target)?;
                refollowed += crate::federation::delivery::deliver_to_inboxes_in_batch(
                    state,
                    activity,
                    vec![inbox.clone()],
                    key_id.clone(),
                    crate::federation::delivery::LinkedData::Unsigned,
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
                was.key_id()?.into(),
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
         FROM eunha.ojak_queue
         WHERE queue = ANY($2) AND payload->>'tag' = $1
         ORDER BY id",
    )
    .bind(tag)
    // A small batch's deliveries wait in the priority queue.
    .bind([
        crate::federation::delivery::QUEUE,
        crate::federation::delivery::PRIORITY_QUEUE,
    ])
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

/// A boolean from `users.settings`, the flat JSON object Mastodon's
/// `UserSettings` keeps (`"notification_emails.report": false`), or `default`.
pub fn user_setting_bool(settings: Option<&str>, key: &str, default: bool) -> bool {
    settings
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v.get(key).and_then(serde_json::Value::as_bool))
        .unwrap_or(default)
}

/// `User#approve!`: approve a pending user, and once it is also confirmed, do
/// what a new user's arrival sets off.
pub async fn approve(state: &crate::state::AppState, account_id: i64) -> Result<()> {
    let approved = sqlx::query_scalar!(
        r#"UPDATE users SET approved = true, updated_at = now()
           WHERE account_id = $1 AND NOT approved
           RETURNING (confirmed_at IS NOT NULL) AS "confirmed!""#,
        account_id,
    )
    .fetch_optional(&state.db)
    .await?;
    if approved == Some(true) {
        prepare_new_user(state, account_id).await;
    }
    Ok(())
}

/// `User#prepare_new_user!`, the part eunha has: `BootstrapTimelineWorker`,
/// which follows the inviter when the invite says to and tells staff.
pub async fn prepare_new_user(state: &crate::state::AppState, account_id: i64) {
    let invite_id = sqlx::query_scalar!(
        "SELECT invite_id FROM users WHERE account_id = $1",
        account_id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .flatten();
    // `TriggerWebhookWorker.perform_async('account.approved', ...)`
    crate::moderation::webhooks::trigger(
        state,
        "account.approved",
        crate::moderation::webhooks::Object::Account(account_id),
    );
    let state = state.clone();
    crate::tenants::spawn(async move {
        // `autofollow_inviter!`
        if let Some(invite_id) = invite_id {
            crate::api::mastodon::signup::autofollow_inviter(&state, account_id, invite_id).await;
        }
        // `notify_staff!`
        match crate::push::accounts_who_can(&state, &[crate::moderation::role::flag::MANAGE_USERS])
            .await
        {
            Ok(staff) => {
                for staff_id in staff {
                    crate::push::notify_local(
                        &state,
                        staff_id,
                        "admin.sign_up",
                        "Account",
                        account_id,
                        account_id,
                    )
                    .await;
                }
            }
            Err(error) => tracing::warn!(%error, "could not list staff for a sign-up"),
        }
    });
}

/// `User#notify_staff_about_pending_account!`: mail those who may manage users
/// that a sign-up waits for them.
pub async fn notify_staff_about_pending_account(state: &crate::state::AppState, account_id: i64) {
    let result: Result<()> = async {
        let account = sqlx::query!(
            r#"SELECT a.username,
                      (SELECT r.text FROM user_invite_requests r WHERE r.user_id = u.id
                       ORDER BY r.id LIMIT 1) AS "invite_request?"
               FROM accounts a JOIN users u ON u.account_id = a.id
               WHERE a.id = $1"#,
            account_id,
        )
        .fetch_one(&state.db)
        .await?;
        let staff =
            crate::push::accounts_who_can(state, &[crate::moderation::role::flag::MANAGE_USERS])
                .await?;
        for staff_id in staff {
            let Some(recipient) = sqlx::query!(
                "SELECT email, settings FROM users WHERE account_id = $1",
                staff_id
            )
            .fetch_optional(&state.db)
            .await?
            else {
                continue;
            };
            // `allows_pending_account_emails?`
            if !user_setting_bool(
                recipient.settings.as_deref(),
                "notification_emails.pending_account",
                true,
            ) {
                continue;
            }
            if let Err(error) = state
                .email
                .send_new_pending_account(
                    &recipient.email,
                    &state.instance.domain,
                    account_id,
                    &account.username,
                    account.invite_request.as_deref(),
                )
                .await
            {
                tracing::warn!(%error, "could not mail staff about a pending account");
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, "could not notify staff about a pending account");
    }
}

/// Devise's `send_password_change_notification`: `UserMailer#password_change`.
pub async fn notify_password_change(state: &crate::state::AppState, user_id: i64) {
    if let Ok(Some(user)) = crate::two_factor::load(&state.db, user_id).await {
        crate::two_factor::notify(
            state,
            &user,
            crate::two_factor::NoticeKind::Security(crate::two_factor::OwnedNotice::PasswordChange),
        );
    }
}

/// `user.login_activities.create(...)`, as `Auth::SessionsController`
/// records each password sign-in to the web, successful or not.
pub async fn record_login(
    db: &PgPool,
    user_id: i64,
    ip: Option<std::net::IpAddr>,
    user_agent: Option<&str>,
    method: &str,
    success: bool,
    failure_reason: Option<&str>,
) {
    let result = sqlx::query!(
        r#"INSERT INTO login_activities
             (user_id, authentication_method, provider, success, failure_reason, ip, user_agent, created_at)
           VALUES ($1, $2, NULL, $3, $4, $5::text::inet, $6, now())"#,
        user_id,
        method,
        success,
        failure_reason,
        ip.map(|ip| ip.to_string()),
        user_agent.unwrap_or_default(),
    )
    .execute(db)
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, "could not record a sign-in");
    }
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
