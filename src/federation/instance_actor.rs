//! The per-instance "instance actor".
//!
//! A single Application actor served at `https://{domain}/actor`, used to sign
//! outbound authorized-fetch GET requests (and to be dereferenced by remote
//! servers verifying those signatures). Mastodon stores this actor as a
//! reserved `accounts` row with id `-99`; Eunha follows that shape so restored
//! Mastodon databases can keep their existing instance actor keypair.

use std::sync::Arc;

use crate::state::AppState;

pub const INSTANCE_ACTOR_ID: i64 = -99;

/// The instance actor's id / URL for a domain.
pub fn actor_url(domain: &str) -> String {
    format!("https://{domain}/actor")
}

/// The instance actor's `keyId`.
pub fn key_id(domain: &str) -> String {
    format!("https://{domain}/actor#main-key")
}

/// Return the instance actor's (private_pem, public_pem), generating and
/// persisting a keypair on first use.
pub async fn get_or_create(state: &AppState) -> anyhow::Result<(String, String)> {
    let domain = &state.instance.domain;

    if let Ok(key) = crate::federation::keypair::signing_key(state, INSTANCE_ACTOR_ID).await {
        if !key.public_key.is_empty() {
            return Ok((key.private_key, key.public_key));
        }
    }

    let (private_pem, public_pem) =
        crate::tenants::spawn_blocking(crate::crypto::generate_rsa_keypair)
            .await?
            .map_err(|e| anyhow::anyhow!("instance actor keygen: {e}"))?;

    // The account row has to exist before a keypair can reference it.
    sqlx::query!(
        r#"INSERT INTO accounts
             (id, username, public_key, actor_type, locked, created_at, updated_at)
           VALUES ($1, $2, '', 'Application', true, now(), now())
           ON CONFLICT (id) DO UPDATE
             SET username = EXCLUDED.username,
                 actor_type = 'Application',
                 locked = true,
                 updated_at = now()"#,
        INSTANCE_ACTOR_ID,
        domain,
    )
    .execute(&state.db)
    .await?;

    let key = crate::federation::keypair::store_local(
        state,
        INSTANCE_ACTOR_ID,
        &private_pem,
        &public_pem,
    )
    .await?;

    Ok((key.private_key, key.public_key))
}

/// The instance actor's private key, parsed once per instance and kept.
///
/// Nothing replaces the instance actor's keypair once it exists, so the key
/// read the first time is the key for the life of the process.
pub async fn signing_key(
    state: &AppState,
) -> anyhow::Result<Arc<feder_runtime::signature::PrivateKey>> {
    state
        .instance_actor_key
        .get_or_try_init(|| async {
            let (private_pem, _) = get_or_create(state).await?;
            let key = crate::tenants::spawn_blocking(move || {
                feder_runtime::signature::PrivateKey::from_pem(&private_pem)
            })
            .await??;
            Ok(Arc::new(key))
        })
        .await
        .cloned()
}

/// Return just the instance actor's public key PEM (generating if needed).
pub async fn public_key(state: &AppState) -> anyhow::Result<String> {
    Ok(get_or_create(state).await?.1)
}
