//! Local accounts' signing keys.
//!
//! Mastodon 4.7.0 moved them out of `accounts.private_key` and into the
//! `keypairs` table, one row per key with a `local_fragment` naming the key
//! inside the actor document (`#main-key`), and the private half encrypted at
//! rest. Mastodon 5.0 dropped the old column, so `keypairs` is the only place a
//! private key can be, and an instance needs the encryption keys Mastodon
//! requires (`ACTIVE_RECORD_ENCRYPTION_PRIMARY_KEY` and
//! `_KEY_DERIVATION_SALT`). `accounts.public_key` remains, for remote actors
//! and for local ones whose key has not been regenerated.
//!
//! Mastodon: app/models/keypair.rb

use anyhow::{anyhow, bail, Context, Result};

use crate::state::AppState;

/// The fragment Mastodon gives an account's main signing key.
pub const MAIN_KEY_FRAGMENT: &str = "#main-key";

/// `keypairs.type`, from the model's enum: `rsa: 0`, `ed25519: 1`.
const TYPE_RSA: i32 = 0;
const TYPE_ED25519: i32 = 1;

/// The fragment eunha gives an account's Ed25519 assertion key.
///
/// Distinct from `#main-key`, which is the RSA key that signs HTTP requests:
/// the two live side by side, and a Multikey published under this fragment is
/// what a peer resolves a proof's `verificationMethod` to.
pub const ED25519_FRAGMENT: &str = "#ed25519-key";

/// A local account's signing key, whichever place it lives in.
pub struct SigningKey {
    pub private_key: String,
    pub public_key: String,
}

/// Read a local account's usable main key, preferring `keypairs`.
///
/// A revoked or expired keypair is skipped, matching the model's `usable`
/// scope.
pub async fn signing_key(state: &AppState, account_id: i64) -> Result<SigningKey> {
    signing_key_in(&state.db, state.encryptor.as_ref(), account_id).await
}

/// [`signing_key`], from a pool and an encryptor rather than the whole
/// state: what the deliverer, which the state holds, reads keys with.
pub async fn signing_key_in(
    db: &sqlx::PgPool,
    encryptor: Option<&crate::rails_encryption::Encryptor>,
    account_id: i64,
) -> Result<SigningKey> {
    let row = sqlx::query!(
        r#"SELECT private_key, public_key
           FROM keypairs
           WHERE account_id = $1
             AND local_fragment = $2
             AND NOT revoked
             AND (expires_at IS NULL OR expires_at > now())
           ORDER BY id DESC
           LIMIT 1"#,
        account_id,
        MAIN_KEY_FRAGMENT,
    )
    .fetch_optional(db)
    .await?
    .ok_or_else(|| anyhow!("no signing key for account {account_id}"))?;

    let sealed = row
        .private_key
        .filter(|key| !key.is_empty())
        .ok_or_else(|| anyhow!("keypair for account {account_id} has no private key"))?;
    let encryptor = encryptor.ok_or_else(|| {
        anyhow!(
            "account {account_id} keeps its signing key in `keypairs`, which is encrypted, \
             but no ActiveRecord encryption keys are configured"
        )
    })?;
    Ok(SigningKey {
        private_key: encryptor
            .decrypt(&sealed)
            .with_context(|| format!("decrypting the signing key for account {account_id}"))?,
        public_key: row.public_key,
    })
}

/// Whether a local account can sign, without decrypting anything.
///
/// The write paths only need to know that a key exists before they build an
/// activity for the delivery queue, which loads the key itself.
pub async fn has_signing_key(state: &AppState, account_id: i64) -> Result<bool> {
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM keypairs
             WHERE account_id = $1
               AND local_fragment = $2
               AND private_key IS NOT NULL AND private_key <> ''
               AND NOT revoked
               AND (expires_at IS NULL OR expires_at > now())
           ) AS "exists!""#,
        account_id,
        MAIN_KEY_FRAGMENT,
    )
    .fetch_one(&state.db)
    .await?;
    Ok(exists)
}

/// The public half of a local account's main key, for its actor document.
pub async fn public_key(state: &AppState, account_id: i64) -> Result<Option<String>> {
    let stored = sqlx::query_scalar!(
        r#"SELECT public_key
           FROM keypairs
           WHERE account_id = $1
             AND local_fragment = $2
             AND NOT revoked
             AND (expires_at IS NULL OR expires_at > now())
           ORDER BY id DESC
           LIMIT 1"#,
        account_id,
        MAIN_KEY_FRAGMENT,
    )
    .fetch_optional(&state.db)
    .await?;

    if let Some(key) = stored.filter(|key| !key.is_empty()) {
        return Ok(Some(key));
    }

    Ok(
        sqlx::query_scalar!("SELECT public_key FROM accounts WHERE id = $1", account_id,)
            .fetch_optional(&state.db)
            .await?
            .filter(|key| !key.is_empty()),
    )
}

/// Record a newly generated key for a local account, and return the key that
/// ends up in force.
///
/// An account that already has a key keeps it: two processes starting at once
/// both generate one, and the actor document can only advertise a single public
/// half, so the first key written is the one everyone must agree on.
pub async fn store_local(
    state: &AppState,
    account_id: i64,
    private_key: &str,
    public_key: &str,
) -> Result<SigningKey> {
    let encryptor = required_encryptor(state.encryptor.as_ref())?;
    let mut conn = state.db.acquire().await?;
    store_sealed(&mut conn, encryptor, account_id, private_key, public_key).await?;
    drop(conn);

    signing_key(state, account_id).await
}

/// The encryptor a new signing key has to go through.
pub fn required_encryptor(
    encryptor: Option<&crate::rails_encryption::Encryptor>,
) -> Result<&crate::rails_encryption::Encryptor> {
    encryptor.ok_or_else(|| {
        anyhow!(
            "signing keys are kept encrypted, but no ActiveRecord encryption keys are configured; \
             set [active_record_encryption], which `generate-secrets` can produce"
        )
    })
}

/// Put a local account's main key into `keypairs`, its private half encrypted.
/// An account that already has a main keypair keeps it.
///
/// Takes a connection rather than the pool so that an account being created can
/// be given its key in the same transaction.
pub async fn store_sealed(
    conn: &mut sqlx::PgConnection,
    encryptor: &crate::rails_encryption::Encryptor,
    account_id: i64,
    private_key: &str,
    public_key: &str,
) -> Result<()> {
    let sealed = encryptor.encrypt(private_key)?;
    sqlx::query!(
        r#"INSERT INTO keypairs
             (account_id, type, local_fragment, public_key, private_key, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, now(), now())
           ON CONFLICT (account_id, local_fragment) DO NOTHING"#,
        account_id,
        TYPE_RSA,
        MAIN_KEY_FRAGMENT,
        public_key,
        sealed,
    )
    .execute(&mut *conn)
    .await?;

    // `Account#keypair` reads the public half from `keypairs` now.
    sqlx::query!(
        "UPDATE accounts SET public_key = '', updated_at = now() WHERE id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;

    Ok(())
}

/// Replace a local account's RSA keys with a newly generated one, as `tootctl
/// accounts rotate` does: every RSA keypair it had goes, and the new key is
/// its `#main-key`. Its other keys, the Ed25519 assertion key among them, stay.
pub async fn replace_local(
    state: &AppState,
    account_id: i64,
    private_key: &str,
    public_key: &str,
) -> Result<()> {
    let encryptor = required_encryptor(state.encryptor.as_ref())?;
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM keypairs WHERE account_id = $1 AND type = $2")
        .bind(account_id)
        .bind(TYPE_RSA)
        .execute(&mut *tx)
        .await?;
    store_sealed(&mut tx, encryptor, account_id, private_key, public_key).await?;
    tx.commit().await?;
    Ok(())
}

/// `Account#keypair(type: :rsa)`'s public half: the oldest usable RSA
/// keypair's, or else the legacy column's. What tells two rows for one remote
/// actor apart from two actors.
pub async fn rsa_public_key<'e>(db: impl sqlx::PgExecutor<'e>, account_id: i64) -> Result<String> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(
           (SELECT public_key FROM keypairs
            WHERE account_id = $1 AND type = $2 AND NOT revoked
              AND (expires_at IS NULL OR expires_at > now())
            ORDER BY id ASC LIMIT 1),
           (SELECT public_key FROM accounts WHERE id = $1),
           '')",
    )
    .bind(account_id)
    .bind(TYPE_RSA)
    .fetch_one(db)
    .await?)
}

/// Mastodon's `MigrateLocalAccountKeypairs` post-deploy migration, which SQL
/// cannot do because the keys have to be encrypted with this instance's
/// secrets. `eunha migrate` runs it before the migrations, because the one for
/// Mastodon 5.0 drops the column these keys are in, and refuses to while one is.
///
/// Once that has run there is no column to read, so the queries are not checked
/// at compile time, and this does nothing. Refuses, rather than leave the
/// keys where the next migration would refuse them, when there are keys to move
/// and nothing to encrypt them with.
pub async fn move_legacy_keys(
    db: &sqlx::PgPool,
    encryptor: Option<&crate::rails_encryption::Encryptor>,
) -> Result<usize> {
    let column_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
           SELECT 1 FROM information_schema.columns
           WHERE table_schema = 'public' AND table_name = 'accounts'
             AND column_name = 'private_key'
         )",
    )
    .fetch_one(db)
    .await?;
    if !column_exists {
        return Ok(0);
    }

    let pending: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT id, private_key, public_key
           FROM public.accounts
           WHERE domain IS NULL AND private_key IS NOT NULL AND private_key <> ''",
    )
    .fetch_all(db)
    .await?;
    if pending.is_empty() {
        return Ok(0);
    }
    let Some(encryptor) = encryptor else {
        bail!(
            "{} local account(s) keep a signing key in `accounts.private_key`, which Mastodon 5.0 \
             drops. Moving them into `keypairs` means encrypting them: set \
             [active_record_encryption], which `generate-secrets` can produce, and run this again",
            pending.len()
        );
    };

    let mut moved = 0;
    for (id, private_key, public_key) in pending {
        let sealed = encryptor
            .encrypt(&private_key)
            .with_context(|| format!("encrypting the signing key for account {id}"))?;

        // An account that already has a keypair keeps it: the row in `keypairs`
        // is the newer of the two, and the legacy column is what is stale.
        let mut tx = db.begin().await?;
        sqlx::query(
            "INSERT INTO public.keypairs
               (account_id, type, local_fragment, public_key, private_key, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, now(), now())
             ON CONFLICT (account_id, local_fragment) DO NOTHING",
        )
        .bind(id)
        .bind(TYPE_RSA)
        .bind(MAIN_KEY_FRAGMENT)
        .bind(&public_key)
        .bind(&sealed)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE public.accounts
             SET public_key = '', private_key = NULL, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        moved += 1;
    }
    Ok(moved)
}

// ── Ed25519 assertion keys ────────────────────────────────────────────────

/// An account's Ed25519 key: the seed that signs, and the public half to
/// publish.
pub struct AssertionKey {
    pub seed: [u8; 32],
    pub public_key: [u8; 32],
}

/// Read a local account's Ed25519 assertion key, generating one on first use.
///
/// Unlike the RSA signing key, this has no legacy home: an account either has
/// one in `keypairs` or does not yet. Generation needs the encryption keys, for
/// the same reason the RSA key does — a private key written in the clear is one
/// nothing else could safely read.
pub async fn assertion_key(state: &AppState, account_id: i64) -> Result<AssertionKey> {
    let encryptor = state.encryptor.as_ref().ok_or_else(|| {
        anyhow!("no ActiveRecord encryption keys are configured, so no key can be stored")
    })?;

    if let Some(row) = sqlx::query!(
        r#"SELECT private_key FROM keypairs
           WHERE account_id = $1
             AND local_fragment = $2
             AND type = $3
             AND NOT revoked
             AND (expires_at IS NULL OR expires_at > now())
           ORDER BY id DESC
           LIMIT 1"#,
        account_id,
        ED25519_FRAGMENT,
        TYPE_ED25519,
    )
    .fetch_optional(&state.db)
    .await?
    {
        if let Some(sealed) = row.private_key.filter(|key| !key.is_empty()) {
            let pem = encryptor.decrypt(&sealed).with_context(|| {
                format!("decrypting the assertion key for account {account_id}")
            })?;
            let (seed, public_key) = ojak::sig::integrity::parse_ed25519_key(&pem)?;
            return Ok(AssertionKey { seed, public_key });
        }
    }

    // Generate and store. `DO NOTHING` on conflict, then read back: two
    // processes starting at once must agree on which key was published, and the
    // first one written is the one peers may already have seen.
    let pem = ojak::sig::integrity::generate_ed25519_key(&mut rand::rand_core::UnwrapErr(
        rand::rngs::SysRng,
    ))?;
    // Only the public half is needed here; the key that ends up in force is
    // read back below, which may be another process's if it won the race.
    let (_, public_key) = ojak::sig::integrity::parse_ed25519_key(&pem)?;
    let multikey = ojak::sig::integrity::encode_ed25519_multikey(&public_key);
    let sealed = encryptor.encrypt(&pem)?;

    sqlx::query!(
        r#"INSERT INTO keypairs
             (account_id, type, local_fragment, public_key, private_key, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, now(), now())
           ON CONFLICT (account_id, local_fragment) DO NOTHING"#,
        account_id,
        TYPE_ED25519,
        ED25519_FRAGMENT,
        multikey,
        sealed,
    )
    .execute(&state.db)
    .await?;

    let stored = sqlx::query_scalar!(
        r#"SELECT private_key FROM keypairs
           WHERE account_id = $1 AND local_fragment = $2 AND type = $3"#,
        account_id,
        ED25519_FRAGMENT,
        TYPE_ED25519,
    )
    .fetch_optional(&state.db)
    .await?
    .flatten()
    .ok_or_else(|| anyhow!("assertion key for account {account_id} vanished after writing it"))?;

    let pem = encryptor.decrypt(&stored)?;
    let (seed, public_key) = ojak::sig::integrity::parse_ed25519_key(&pem)?;
    Ok(AssertionKey { seed, public_key })
}

/// The `publicKeyMultibase` an account publishes, if it has one.
///
/// Read-only: an actor document is served far more often than an activity is
/// signed, and generating a key to answer a GET would mean every crawler
/// minting keys. The key appears once the account first signs something.
pub async fn assertion_multikey(state: &AppState, account_id: i64) -> Result<Option<String>> {
    Ok(sqlx::query_scalar!(
        r#"SELECT public_key FROM keypairs
           WHERE account_id = $1
             AND local_fragment = $2
             AND type = $3
             AND NOT revoked
             AND (expires_at IS NULL OR expires_at > now())
           ORDER BY id DESC
           LIMIT 1"#,
        account_id,
        ED25519_FRAGMENT,
        TYPE_ED25519,
    )
    .fetch_optional(&state.db)
    .await?
    .filter(|key| !key.is_empty()))
}
