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
    /// Write `confirmed_at` now. Otherwise the user waits for the link
    /// [`send_confirmation_instructions`] mails, as Devise's confirmable does.
    pub confirmed: bool,
    /// `users.confirmation_token`, written with `confirmation_sent_at` now:
    /// what Devise's `generate_confirmation_token` writes on create.
    pub confirmation_token: Option<&'a str>,
    /// An existing local account, holding no user, to attach the user to
    /// instead of making a new one: `tootctl accounts create --reattach`.
    pub account_id: Option<i64>,
}

/// The rows a new local account was written as.
pub struct LocalUser {
    pub account_id: i64,
    pub user_id: i64,
}

/// Write a local account and its user, with a fresh signing key, or attach the
/// user to an account that has none ([`NewLocalUser::account_id`]).
///
/// The account, its key and its user are written in one transaction, so a
/// failure part way leaves no account without a user holding the username.
pub async fn create_local(
    db: &PgPool,
    encryptor: Option<&Encryptor>,
    domain: &str,
    user: NewLocalUser<'_>,
) -> Result<LocalUser> {
    let prepared = prepare_local(db, user.account_id.is_none()).await?;
    let mut tx = db.begin().await?;
    let created = insert_local(&mut tx, encryptor, domain, user, prepared).await?;
    tx.commit().await?;
    Ok(created)
}

/// What [`insert_local`] needs worked out before its transaction opens.
pub struct PreparedLocal {
    keys: Option<(String, String)>,
    age_verified: bool,
}

/// A fresh signing key, when the user needs a new account, and whether the
/// instance asks for an age.
pub async fn prepare_local(db: &PgPool, new_account: bool) -> Result<PreparedLocal> {
    let keys = if new_account {
        // A 2048-bit key is on the order of a hundred milliseconds of CPU.
        Some(
            crate::tenants::spawn_blocking(|| {
                ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng)
            })
            .await
            .context("generating a signing key did not finish")??,
        )
    } else {
        None
    };
    // `User#set_age_verified_at`: when the instance asks for an age, every
    // user it creates has had theirs checked.
    let age_verified = crate::settings::min_age(db).await.is_some();
    Ok(PreparedLocal { keys, age_verified })
}

/// [`create_local`] inside a transaction the caller holds, so that what else
/// a sign-up writes — the invite's use, its access token — commits with it.
pub async fn insert_local(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    encryptor: Option<&Encryptor>,
    domain: &str,
    user: NewLocalUser<'_>,
    prepared: PreparedLocal,
) -> Result<LocalUser> {
    let PreparedLocal { keys, age_verified } = prepared;
    let account_id = match (user.account_id, keys) {
        // `account.suspended_at = nil; account.requested_deletion_at = nil`:
        // the account keeps its id, actor and keys.
        (Some(id), _) => sqlx::query_scalar!(
            r#"UPDATE accounts SET suspended_at = NULL, requested_deletion_at = NULL,
                      updated_at = now()
               WHERE id = $1 AND domain IS NULL
               RETURNING id"#,
            id,
        )
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| anyhow!("the account to reattach is gone"))?,
        (None, Some((private_key, public_key))) => {
            insert_local_account(
                tx,
                encryptor,
                domain,
                user.username,
                &private_key,
                &public_key,
            )
            .await?
        }
        (None, None) => bail!("no signing key was made for the new account"),
    };

    let user_id = sqlx::query_scalar!(
        r#"INSERT INTO users
             (account_id, email, encrypted_password, role_id,
              confirmed_at, invite_id, approved,
              locale, created_by_application_id, sign_up_ip, age_verified_at,
              time_zone, confirmation_token, confirmation_sent_at,
              created_at, updated_at)
           VALUES ($1,$2,$3,$4,
                   CASE WHEN $12 THEN now() END, $5, $6,
                   $7, $8, $9::text::inet, CASE WHEN $10 THEN now() END,
                   $11, $13::varchar, CASE WHEN $13::varchar IS NOT NULL THEN now() END,
                   now(), now())
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
        user.confirmed,
        user.confirmation_token,
    )
    .fetch_one(&mut **tx)
    .await?;

    if let Some(text) = user.invite_request.filter(|t| !t.is_empty()) {
        sqlx::query!(
            r#"INSERT INTO user_invite_requests (user_id, text, created_at, updated_at)
               VALUES ($1, $2, now(), now())"#,
            user_id,
            text,
        )
        .execute(&mut **tx)
        .await?;
    }

    Ok(LocalUser {
        account_id,
        user_id,
    })
}

/// A new local account row for `username`, signing with the given key.
async fn insert_local_account(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    encryptor: Option<&Encryptor>,
    domain: &str,
    username: &str,
    private_key: &str,
    public_key: &str,
) -> Result<i64> {
    let url = format!("https://{domain}/@{username}");
    let new_account_id = crate::snowflake::next_id();
    // New local accounts use Mastodon's default `numeric_ap_id` scheme: the
    // ActivityPub actor is served at /ap/users/{id}. Build the canonical URI
    // (and its inbox/outbox) from the new account id.
    let uris = crate::api::ap::serving::uris(domain)?;
    let own = AccountUris::new(
        &uris,
        new_account_id,
        Some(crate::federation::tag::NUMERIC_AP_ID),
        username,
    );
    let uri = own.actor()?.to_string();
    let inbox_url = own.uri(Own::Inbox)?;
    let outbox_url = own.uri(Own::Outbox)?;
    let shared_inbox_url = uris.shared_inbox_uri()?;

    let account_id = sqlx::query_scalar!(
        r#"INSERT INTO accounts
             (id, username, url, uri, private_key, public_key,
              inbox_url, outbox_url, shared_inbox_url, id_scheme, created_at, updated_at)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9, 1, now(), now())
           RETURNING id"#,
        new_account_id,
        username,
        url,
        uri,
        private_key,
        public_key,
        inbox_url.as_str(),
        outbox_url.as_str(),
        shared_inbox_url.as_str(),
    )
    .fetch_one(&mut **tx)
    .await?;

    // Written to `accounts` above so that the account is never keyless; move it
    // to `keypairs` when this instance keeps signing keys there.
    if let Some(encryptor) = encryptor {
        crate::federation::keypair::store_sealed(
            tx,
            encryptor,
            account_id,
            private_key,
            public_key,
        )
        .await?;
    }
    Ok(account_id)
}

/// What `eunha accounts create` was asked for.
pub struct CreateOptions {
    pub username: String,
    pub email: String,
    pub role: Option<String>,
    pub confirmed: bool,
    pub approve: bool,
    /// Give the user the existing local account holding the username, if
    /// there is one: one whose user is gone, as a deleted account's is.
    pub reattach: bool,
    /// With `reattach`, delete the user still holding the account first.
    pub force: bool,
}

/// What `eunha accounts create` did.
#[derive(Debug)]
pub enum Created {
    /// The account, and the random password it was given.
    Account(String),
    /// `--reattach` found the username held by a user, and without `--force`
    /// left it alone.
    UsernameInUse,
}

/// `tootctl accounts create`: make a local account outside the sign-up flow.
///
/// Like upstream it bypasses the registration checks — whether sign-ups are
/// open, and the username and email blocks — but not the account's own
/// validations: the username's format, length and uniqueness, and the email's.
/// The account is approved exactly when a sign-up would be, which is
/// `User#set_approved` on an open, approval-free instance, and then, as the
/// command goes on: confirmed with `confirmed`, which welcomes it in or puts it
/// before the staff as `User#mark_email_as_confirmed!` does, or else mailed a
/// confirmation link; then approved with `approve`, as `User#approve!` does.
pub async fn create_from_command(
    state: &crate::state::AppState,
    options: CreateOptions,
) -> Result<Created> {
    let db = &state.db;
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
    crate::moderation::signup::check_email(state, &email, options.confirmed)
        .await
        .map_err(|refusal| anyhow!(refusal.message()))?;

    let role_id = match options.role.as_deref() {
        None => None,
        Some(name) => Some(
            sqlx::query_scalar!("SELECT id FROM user_roles WHERE name = $1 LIMIT 1", name)
                .fetch_optional(db)
                .await?
                .ok_or_else(|| anyhow!("cannot find user role with that name"))?,
        ),
    };

    let existing = sqlx::query!(
        r#"SELECT a.id, EXISTS (SELECT 1 FROM users u WHERE u.account_id = a.id) AS "has_user!"
           FROM accounts a WHERE lower(a.username) = lower($1) AND a.domain IS NULL"#,
        username,
    )
    .fetch_optional(db)
    .await?;
    // The account to attach the user to, and the one whose user `--force`
    // deletes.
    let (reattach, replace) = match existing {
        None => (None, None),
        Some(_) if !options.reattach => bail!("username has already been taken"),
        Some(account) if !account.has_user => (Some(account.id), None),
        Some(_) if !options.force => return Ok(Created::UsernameInUse),
        Some(account) => (None, Some(account.id)),
    };

    // The user `--force` deletes gives up its address with it.
    let email_taken = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM users WHERE lower(email) = $1 AND account_id IS DISTINCT FROM $2
           ) AS "exists!""#,
        email,
        replace,
    )
    .fetch_one(db)
    .await?;
    if email_taken {
        bail!("email has already been taken");
    }

    if let Some(account_id) = replace {
        // `DeleteAccountService.new.call(account, reserve_email: false,
        // reserve_username: false)`: the user and the account both go.
        crate::delete_account::call(state, account_id, crate::delete_account::Options::purge())
            .await?;
    }

    // `SecureRandom.hex`: 16 random bytes, written as 32 hex digits.
    let password = crate::crypto::generate_token(16);
    let password_hash = crate::crypto::hash_password(&password)
        .await
        .map_err(|e| anyhow!("hashing the password: {e}"))?;

    let LocalUser {
        account_id,
        user_id,
    } = create_local(
        db,
        state.encryptor.as_ref(),
        &state.instance.domain,
        NewLocalUser {
            username,
            email: &email,
            password_hash: &password_hash,
            role_id,
            // `User#set_approved`; `--approve` comes after the save.
            approved: crate::settings::registrations_mode(state).await.open()
                && !crate::moderation::signup::requires_approval(state, username, &email, None)
                    .await,
            invite_id: None,
            locale: None,
            app_id: None,
            sign_up_ip: None,
            invite_request: None,
            time_zone: None,
            confirmed: false,
            confirmation_token: None,
            account_id: reattach,
        },
    )
    .await?;

    // `User#trigger_webhooks` and the account's `after_commit`s.
    crate::moderation::webhooks::trigger(
        state,
        "account.created",
        crate::moderation::webhooks::Object::Account(account_id),
    )
    .await;
    if reattach.is_some() {
        // The reattached account is saved with the user: its
        // `after_update_commit`.
        crate::moderation::webhooks::account_updated(state, account_id).await;
        crate::fasp::events::account_updated(state, account_id, false).await;
    } else {
        crate::fasp::events::account_created(state, account_id).await;
    }

    if options.confirmed {
        // `user.confirmed_at = nil; user.mark_email_as_confirmed!`.
        confirm_user(state, user_id, false).await?;
    } else {
        // Devise's `send_on_create_confirmation_instructions`.
        send_confirmation_instructions(state, user_id).await?;
    }
    if options.approve {
        approve(state, account_id).await?;
    }

    Ok(Created::Account(password))
}

/// `tootctl accounts modify --reset-password`: give a local account a new
/// random password, sign it out everywhere, and return the password.
pub async fn reset_password(state: &crate::state::AppState, username: &str) -> Result<String> {
    let user_id = sqlx::query_scalar!(
        r#"SELECT u.id FROM users u JOIN accounts a ON a.id = u.account_id
           WHERE lower(a.username) = lower($1) AND a.domain IS NULL"#,
        username,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| anyhow!("no user with such username"))?;
    change_password(state, user_id).await
}

/// What `eunha accounts modify` was asked for.
#[derive(Debug, Default)]
pub struct ModifyOptions {
    pub role: Option<String>,
    pub remove_role: bool,
    pub email: Option<String>,
    pub confirm: bool,
    pub enable: bool,
    pub disable: bool,
    pub approve: bool,
    pub disable_2fa: bool,
    pub reset_password: bool,
}

/// `tootctl accounts modify`: change a local user, and return the new random
/// password when asked for one.
///
/// Each option writes what upstream's assigns, in its order: a role (which
/// wins over `remove_role`), an address that waits in `unconfirmed_email` for
/// the link mailed to it as Devise's reconfirmable has it, `disabled` (where
/// `disable` wins over `enable`), `approved`, then `User#disable_two_factor!`
/// and `User#change_password!`; last, `User#confirm`, which confirms the
/// address waiting and welcomes a user confirmed for the first time. Setting
/// `disabled` or `approved` is only the column, as upstream's assignments are:
/// a disabled user's open streams stay open, and an approved one is not
/// welcomed.
pub async fn modify_from_command(
    state: &crate::state::AppState,
    username: &str,
    options: ModifyOptions,
) -> Result<Option<String>> {
    let db = &state.db;
    let user = sqlx::query!(
        r#"SELECT u.id, u.email, (u.confirmed_at IS NOT NULL) AS "confirmed!"
           FROM users u JOIN accounts a ON a.id = u.account_id
           WHERE lower(a.username) = lower($1) AND a.domain IS NULL"#,
        username,
    )
    .fetch_optional(db)
    .await?
    .ok_or_else(|| anyhow!("no user with such username"))?;

    // `UserRole.find_by(name:)`, or `--remove-role`'s nil; untouched otherwise.
    let role = match (options.role.as_deref(), options.remove_role) {
        (Some(name), _) => Some(Some(
            sqlx::query_scalar!("SELECT id FROM user_roles WHERE name = $1 LIMIT 1", name)
                .fetch_optional(db)
                .await?
                .ok_or_else(|| anyhow!("cannot find user role with that name"))?,
        )),
        (None, true) => Some(None),
        (None, false) => None,
    };
    // Devise strips and downcases the address; one that does not change it is
    // no change.
    let email = options
        .email
        .as_deref()
        .map(|e| e.trim().to_lowercase())
        .filter(|e| *e != user.email);
    if let Some(email) = &email {
        if !valid_email(email) {
            bail!("email is invalid");
        }
        crate::moderation::signup::check_email(state, email, user.confirmed)
            .await
            .map_err(|refusal| anyhow!(refusal.message()))?;
        let taken = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM users WHERE lower(email) = $1 AND id <> $2) AS "e!""#,
            email,
            user.id,
        )
        .fetch_one(db)
        .await?;
        if taken {
            bail!("email has already been taken");
        }
    }
    let disabled = if options.disable {
        Some(true)
    } else if options.enable {
        Some(false)
    } else {
        None
    };

    let mut tx = db.begin().await?;
    sqlx::query!(
        r#"UPDATE users SET
             role_id = CASE WHEN $2 THEN $3 ELSE role_id END,
             disabled = COALESCE($4, disabled),
             approved = approved OR $5,
             updated_at = now()
           WHERE id = $1"#,
        user.id,
        role.is_some(),
        role.flatten(),
        disabled,
        options.approve,
    )
    .execute(&mut *tx)
    .await?;
    if let Some(email) = &email {
        set_unconfirmed_email(&mut *tx, user.id, email).await?;
    }
    if options.disable_2fa {
        crate::two_factor::disable(&mut tx, user.id).await?;
    }
    tx.commit().await?;
    if email.is_some() {
        // `send_reconfirmation_instructions`, after the save.
        send_confirmation_instructions(state, user.id).await?;
    }

    let password = if options.reset_password {
        Some(change_password(state, user.id).await?)
    } else {
        None
    };
    if options.confirm {
        confirm_user(state, user.id, true).await?;
    }
    Ok(password)
}

/// `User#change_password!(SecureRandom.hex)`: a new random password, every
/// session and authorization of the user gone with it, and each open stream
/// made with one of its tokens told `kill`. Returns the password.
pub async fn change_password(state: &crate::state::AppState, user_id: i64) -> Result<String> {
    // `SecureRandom.hex`: 16 random bytes, written as 32 hex digits.
    let password = crate::crypto::generate_token(16);
    let password_hash = crate::crypto::hash_password(&password)
        .await
        .map_err(|e| anyhow!("hashing the password: {e}"))?;

    // `update(password:)`, which clears a mailed reset token
    // (`clear_reset_password_token`), then `session_activations.destroy_all`,
    // whose access tokens go with them and close their streams.
    sqlx::query!(
        r#"UPDATE users
           SET encrypted_password = $1,
               reset_password_token = NULL, reset_password_sent_at = NULL,
               updated_at = now()
           WHERE id = $2"#,
        password_hash,
        user_id,
    )
    .execute(&state.db)
    .await?;
    let tokens = crate::sessions::destroy_all(&state.db, user_id).await?;
    crate::sessions::kill_streams(state, tokens).await;
    // `revoke_access!`: grants and tokens revoked, the push subscriptions made
    // through them deleted, and `kill` for each token.
    crate::sessions::revoke_access(state, user_id).await?;
    // Devise's `send_password_change_notification`.
    if let Ok(Some(user)) = crate::two_factor::load(&state.db, user_id).await {
        crate::two_factor::notify_now(
            state,
            &user,
            crate::two_factor::NoticeKind::Security(crate::two_factor::OwnedNotice::PasswordChange),
        )
        .await;
    }

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
    let locale = user.locale.unwrap_or_else(|| "en".into());
    if let Err(error) = state
        .mailer()
        .send_password_reset(&user.email, &user.username, &url, &locale)
        .await
    {
        tracing::error!(%error, "failed to send password reset email");
    }
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
        crate::sessions::kill_streams(state, tokens).await;
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
    // `generate_confirmation_token`: the token already mailed while it is
    // still good (`confirmation_period_expired?`), a new one otherwise.
    let fresh = crate::crypto::generate_token(32);
    let Some(user) = sqlx::query!(
        r#"UPDATE users u SET
             confirmation_token = CASE
                 WHEN u.confirmation_token IS NOT NULL
                      AND u.confirmation_sent_at > now() - make_interval(days => $3)
                 THEN u.confirmation_token ELSE $2 END,
             confirmation_sent_at = CASE
                 WHEN u.confirmation_token IS NOT NULL
                      AND u.confirmation_sent_at > now() - make_interval(days => $3)
                 THEN u.confirmation_sent_at ELSE now() END
           FROM accounts a
           WHERE u.id = $1 AND a.id = u.account_id
           RETURNING u.confirmation_token AS "token!",
                     COALESCE(NULLIF(u.unconfirmed_email, ''), u.email) AS "to!", u.locale,
                     a.username, (COALESCE(u.unconfirmed_email, '') <> '') AS "reconfirming!",
                     (u.created_by_application_id IS NOT NULL) AS "from_app!""#,
        user_id,
        fresh,
        CONFIRM_WITHIN_DAYS,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    // `confirmation_url`: with `redirect_to_app: 'true'` on the confirmation
    // of a user who signed up through an app, as its button carries it.
    let back_to_app = if user.from_app && !user.reconfirming {
        "&redirect_to_app=true"
    } else {
        ""
    };
    let url = format!(
        "https://{}/auth/confirm?token={}{back_to_app}",
        state.instance.domain, user.token
    );
    let locale = user.locale.unwrap_or_else(|| "en".into());
    let domain = &state.instance.domain;
    let email = state.mailer();
    let sent = if user.reconfirming {
        email
            .send_reconfirmation_instructions(&user.to, domain, &url)
            .await
    } else {
        email
            .send_confirmation(&user.to, &user.username, "", &url, &locale)
            .await
    };
    if let Err(error) = sent {
        tracing::error!(%error, "failed to send confirmation email");
    }
    Ok(())
}

/// Devise's `confirm_within`, as Mastodon configures it: a confirmation link
/// is good for two days after it was sent.
pub const CONFIRM_WITHIN_DAYS: i32 = 2;

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
    // `grant_approval_on_confirmation?`: a user still awaiting approval is
    // approved on confirming once registrations are open to all, unless a
    // block asks for approval (`requires_approval?`).
    let Some(user) = sqlx::query!(
        r#"SELECT u.email, u.approved, host(u.sign_up_ip) AS sign_up_ip, a.username
           FROM users u JOIN accounts a ON a.id = u.account_id WHERE u.id = $1"#,
        user_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let grant = !user.approved
        && crate::settings::registrations_mode(state).await.open()
        && !crate::moderation::signup::requires_approval(
            state,
            &user.username,
            &user.email,
            user.sign_up_ip.as_deref().and_then(|ip| ip.parse().ok()),
        )
        .await;
    let row = sqlx::query!(
        r#"UPDATE users u SET
             email = CASE WHEN $2 THEN COALESCE(lower(btrim(u.unconfirmed_email)), u.email) ELSE u.email END,
             unconfirmed_email = CASE WHEN $2 THEN NULL ELSE u.unconfirmed_email END,
             confirmed_at = COALESCE(u.confirmed_at, now()),
             confirmation_token = NULL,
             approved = u.approved OR $3,
             updated_at = now()
           FROM users before
           WHERE u.id = $1 AND before.id = u.id
           RETURNING u.account_id, u.approved, (before.confirmed_at IS NULL) AS "new_user!""#,
        user_id,
        reconfirm,
        grant,
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
            notify_staff_about_pending_account(state, row.account_id).await;
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
           AND ($1::text[] IS NULL OR lower(username) = ANY(SELECT lower(u) FROM unnest($1::text[]) u))
         ORDER BY id",
    )
    .bind(&names)
    .fetch_all(db)
    .await?;
    let unknown = names
        .unwrap_or_default()
        .into_iter()
        .filter(|name| {
            !accounts
                .iter()
                .any(|account| account.username.eq_ignore_ascii_case(name))
        })
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
/// which follows the inviter when the invite says to and tells staff, the
/// welcome mail an hour later, and the `account.approved` webhook.
pub async fn prepare_new_user(state: &crate::state::AppState, account_id: i64) {
    // Approved and confirmed, the account joins `Account.searchable`.
    crate::search::elasticsearch::indexing::account(state, account_id).await;
    let user = sqlx::query!(
        "SELECT id, invite_id FROM users WHERE account_id = $1",
        account_id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let invite_id = user.as_ref().and_then(|u| u.invite_id);
    // `ActivityTracker.increment('activity:accounts:local')` and
    // `ActivityTracker.record('activity:logins', id)`.
    crate::activity_tracker::increment(state, crate::activity_tracker::ACCOUNTS_LOCAL).await;
    if let Some(user) = &user {
        crate::activity_tracker::record(state, crate::activity_tracker::LOGINS, user.id).await;
    }
    // `UserMailer.welcome(self).deliver_later(wait: 1.hour)`.
    if let Some(user) = &user {
        crate::jobs::push_in(state, WELCOME_DELAY, WelcomeMailJob { user_id: user.id }).await;
    }
    // `TriggerWebhookWorker.perform_async('account.approved', ...)`
    crate::moderation::webhooks::trigger(
        state,
        "account.approved",
        crate::moderation::webhooks::Object::Account(account_id),
    )
    .await;
    let state = state.clone();
    async move {
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
    }
    .await;
}

/// How long after a new user is prepared its welcome mail goes.
pub const WELCOME_DELAY: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// `UserMailer.welcome(user).deliver_later`: rendered when it is sent, an
/// hour on, so that its checklist shows what the user has done since.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct WelcomeMailJob {
    pub user_id: i64,
}

impl crate::jobs::Job for WelcomeMailJob {
    const KIND: &'static str = "ActionMailer::MailDeliveryJob(UserMailer#welcome)";
    const OPTIONS: crate::jobs::Options =
        crate::jobs::Options::DEFAULT.queue(crate::jobs::Queue::Mailers);

    async fn perform(self, state: &crate::state::AppState) -> anyhow::Result<()> {
        send_welcome(state, self.user_id).await
    }
}

/// `UserMailer#welcome`: nothing for a user gone in the meantime, nor, as
/// `active_for_authentication?` holds it back, for a memorial account.
pub async fn send_welcome(state: &crate::state::AppState, user_id: i64) -> Result<()> {
    let Some(user) = sqlx::query!(
        r#"SELECT u.email, a.id AS account_id, a.username, a.memorial,
                  (a.display_name <> '' OR a.note <> ''
                   OR COALESCE(a.avatar_file_name, '') <> '') AS "has_profile!",
                  EXISTS (SELECT 1 FROM follows f WHERE f.account_id = a.id) AS "has_follows!",
                  EXISTS (SELECT 1 FROM statuses s WHERE s.account_id = a.id) AS "has_statuses!"
           FROM users u JOIN accounts a ON a.id = u.account_id
           WHERE u.id = $1"#,
        user_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    if user.memorial {
        return Ok(());
    }
    // `AccountSuggestions.new(account).get(5)`.
    let suggested: Vec<i64> = crate::suggestions::get(state, user.account_id, 5, 0)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let mut suggestions = Vec::new();
    for id in suggested {
        if let Some(account) = sqlx::query!(
            "SELECT username, domain, display_name FROM accounts WHERE id = $1",
            id
        )
        .fetch_optional(&state.db)
        .await?
        {
            let acct = match account.domain {
                Some(domain) => format!("{}@{domain}", account.username),
                None => account.username.clone(),
            };
            let name = if account.display_name.is_empty() {
                account.username
            } else {
                account.display_name
            };
            suggestions.push((name, acct));
        }
    }
    // `Trends.tags.query.allowed.limit(5)`, and `recent_tag_usage` of each.
    let trending = sqlx::query!(
        r#"SELECT t.id, COALESCE(NULLIF(t.display_name, ''), t.name) AS "name!"
           FROM tags t JOIN tag_trends tt ON tt.tag_id = t.id
           WHERE tt.allowed
           ORDER BY tt.score DESC
           LIMIT 5"#
    )
    .fetch_all(&state.db)
    .await?;
    let mut tags = Vec::with_capacity(trending.len());
    for tag in trending {
        let people = crate::moderation::history::aggregate_accounts(state, "tags", tag.id, 2).await;
        tags.push((tag.name, people));
    }
    let mail = crate::email::WelcomeMail {
        domain: state.instance.domain.clone(),
        username: user.username,
        has_profile: user.has_profile,
        has_follows: user.has_follows,
        has_statuses: user.has_statuses,
        suggestions,
        tags,
    };
    state.email.send_welcome(&user.email, &mail).await
}

/// What [`convert_pending_signups`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PendingSignupConversion {
    /// Sign-ups made unconfirmed users, their links still good.
    pub converted: u64,
    /// Sign-ups past their day, or whose username or address someone else
    /// has taken since, left to go with the table.
    pub dropped: u64,
}

/// How many sign-ups wait in `eunha.pending_signups`, while it is there.
pub async fn pending_signups_waiting(db: &PgPool) -> Result<u64> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('eunha.pending_signups') IS NOT NULL")
            .fetch_one(db)
            .await?;
    if !exists {
        return Ok(0);
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM eunha.pending_signups")
        .fetch_one(db)
        .await?;
    Ok(count as u64)
}

/// Sign-ups made before eunha wrote them as Mastodon does, waiting in
/// `eunha.pending_signups` for their link to be followed, turned into what a
/// sign-up writes now: an account and an unconfirmed user, with the invite's
/// use counted. Each keeps the confirmation token its mail carries, sent when
/// the sign-up was made, so the link still confirms it within
/// [`CONFIRM_WITHIN_DAYS`]. Approval is what `User#set_approved` gives on the
/// instance's registrations and the invite; a block asking for approval is
/// weighed again on confirming (`grant_approval_on_confirmation?`). The
/// `account.created` webhook is queued for the server's job loops. `eunha
/// migrate` runs this before the migration that drops the table; nothing
/// happens once it is gone.
pub async fn convert_pending_signups(
    db: &PgPool,
    encryptor: Option<&Encryptor>,
    instance: &crate::config::InstanceConfig,
) -> Result<PendingSignupConversion> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('eunha.pending_signups') IS NOT NULL")
            .fetch_one(db)
            .await?;
    let mut report = PendingSignupConversion::default();
    if !exists {
        return Ok(report);
    }
    // Read without the query macros: the table is gone from the schema they
    // are checked against.
    type Row = (
        String,
        String,
        String,
        Option<i64>,
        Option<String>,
        String,
        Option<i64>,
        String,
        Option<String>,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
        bool,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT username, email, password_hash, invite_id, reason, locale, app_id,
                confirmation_token, host(sign_up_ip), time_zone, created_at,
                expires_at > now()
         FROM eunha.pending_signups ORDER BY created_at",
    )
    .fetch_all(db)
    .await?;
    let open = crate::settings::registrations_mode_in(db, instance)
        .await
        .open();
    let jobs_ready: bool = sqlx::query_scalar("SELECT to_regclass('eunha.jobs') IS NOT NULL")
        .fetch_one(db)
        .await?;
    for (
        username,
        email,
        password_hash,
        invite_id,
        reason,
        locale,
        app_id,
        token,
        sign_up_ip,
        time_zone,
        created_at,
        live,
    ) in rows
    {
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM accounts WHERE lower(username) = lower($1) AND domain IS NULL)
                 OR EXISTS (SELECT 1 FROM users WHERE lower(email) = lower($2))",
        )
        .bind(&username)
        .bind(&email)
        .fetch_one(db)
        .await?;
        if !live || taken {
            report.dropped += 1;
            continue;
        }
        // `valid_bypassing_invitation?`: an invite still good for use, written
        // by someone whose role may waive approval.
        let bypass: bool = match invite_id {
            None => false,
            Some(id) => sqlx::query_scalar(
                "SELECT COALESCE((
                   SELECT ((COALESCE(r.permissions, 0) | COALESCE(e.permissions, 0)) & ($2 | 1)) <> 0
                   FROM invites i
                   JOIN users u ON u.id = i.user_id
                   LEFT JOIN user_roles r ON r.id = u.role_id
                   LEFT JOIN user_roles e ON e.id = -99
                   WHERE i.id = $1
                     AND (i.max_uses IS NULL OR i.uses < i.max_uses)
                     AND (i.expires_at IS NULL OR i.expires_at > now())
                 ), false)",
            )
            .bind(id)
            .bind(crate::moderation::role::flag::INVITE_BYPASS_APPROVAL)
            .fetch_one(db)
            .await?,
        };
        let created = create_local(
            db,
            encryptor,
            &instance.domain,
            NewLocalUser {
                username: &username,
                email: &email,
                password_hash: &password_hash,
                role_id: None,
                approved: open || bypass,
                invite_id,
                locale: Some(&locale),
                app_id,
                sign_up_ip: sign_up_ip.as_deref().and_then(|ip| ip.parse().ok()),
                invite_request: reason.as_deref(),
                time_zone: time_zone.as_deref(),
                confirmed: false,
                confirmation_token: Some(&token),
                account_id: None,
            },
        )
        .await?;
        sqlx::query("UPDATE users SET confirmation_sent_at = $2 WHERE id = $1")
            .bind(created.user_id)
            .bind(created_at.naive_utc())
            .execute(db)
            .await?;
        if let Some(id) = invite_id {
            sqlx::query("UPDATE invites SET uses = uses + 1 WHERE id = $1")
                .bind(id)
                .execute(db)
                .await?;
        }
        // `User#trigger_webhooks`, `after_create_commit`: queued for the
        // server's job loops, as `TriggerWebhookWorker.perform_async` queues
        // it for Sidekiq. A database too old to have the job queue yet has no
        // webhooks to deliver to either.
        if jobs_ready {
            crate::jobs::perform_async_in(
                db,
                crate::moderation::webhooks::TriggerWebhookWorker {
                    event: "account.created".to_owned(),
                    object: crate::moderation::webhooks::Object::Account(created.account_id),
                },
            )
            .await?;
        }
        sqlx::query("DELETE FROM eunha.pending_signups WHERE confirmation_token = $1")
            .bind(&token)
            .execute(db)
            .await?;
        report.converted += 1;
    }
    Ok(report)
}

/// `Scheduler::UserCleanupScheduler#clean_unconfirmed_accounts!`: users who
/// were mailed a confirmation link a week ago or more and never followed it go,
/// with their accounts. Returns how many.
pub async fn clean_unconfirmed(db: &PgPool) -> Result<u64> {
    let mut tx = db.begin().await?;
    let gone = sqlx::query!(
        r#"SELECT id, account_id FROM users
           WHERE confirmed_at IS NULL
             AND confirmation_sent_at <= now() - make_interval(days => $1)"#,
        crate::email_subscriptions::UNCONFIRMED_MAX_AGE_DAYS as i32,
    )
    .fetch_all(&mut *tx)
    .await?;
    if gone.is_empty() {
        return Ok(0);
    }
    let user_ids: Vec<i64> = gone.iter().map(|u| u.id).collect();
    let account_ids: Vec<i64> = gone.iter().map(|u| u.account_id).collect();
    // Removed on their own, for want of database constraints, as upstream
    // removes them.
    sqlx::query!(
        "DELETE FROM account_moderation_notes WHERE target_account_id = ANY($1)",
        &account_ids
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM webauthn_credentials WHERE user_id = ANY($1)",
        &user_ids
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM accounts WHERE id = ANY($1)", &account_ids)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM users WHERE id = ANY($1)", &user_ids)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(gone.len() as u64)
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
                .mailer()
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
