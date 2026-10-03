//! Local signing keys, in both the places Mastodon has kept them.
//!
//! Mastodon 4.7.0 moved them from `accounts.private_key` into the `keypairs`
//! table, encrypted at rest. Eunha has to read either, move one to the other,
//! and leave rows a Mastodon pointed at the same database could still use.

use eunha::federation::keypair;

use crate::helpers::TestContext;

/// Give an account a signing key the old way, as a database that has not run
/// the move still has.
async fn seed_legacy_key(ctx: &TestContext, account_id: i64) -> String {
    let (private_key, public_key) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    sqlx::query!(
        "UPDATE accounts SET private_key = $2, public_key = $3 WHERE id = $1",
        account_id,
        private_key,
        public_key,
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    private_key
}

/// An account whose key never moved still signs, so a 4.6-shaped database
/// keeps working.
#[tokio::test]
async fn test_legacy_account_key_is_still_readable() {
    let ctx = TestContext::new("kp-legacy").await;
    let account_id: i64 = ctx.alice_id.parse().unwrap();
    let private_key = seed_legacy_key(&ctx, account_id).await;

    assert!(keypair::has_signing_key(&ctx.state, account_id)
        .await
        .unwrap());
    let loaded = keypair::signing_key(&ctx.state, account_id).await.unwrap();
    assert_eq!(loaded.private_key, private_key);
}

/// Upstream's `migrate_local_account_keypairs`: the key moves, the old columns
/// are emptied, and what lands in `keypairs` is encrypted rather than a PEM.
#[tokio::test]
async fn test_migrate_moves_local_keys_and_encrypts_them() {
    let ctx = TestContext::new("kp-migrate").await;
    let account_id: i64 = ctx.alice_id.parse().unwrap();
    let private_key = seed_legacy_key(&ctx, account_id).await;

    keypair::migrate_local_keypairs(&ctx.state).await.unwrap();

    let stored = sqlx::query!(
        "SELECT private_key, public_key, local_fragment, type FROM keypairs WHERE account_id = $1",
        account_id,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let sealed = stored.private_key.unwrap();
    assert!(
        !sealed.contains("PRIVATE KEY"),
        "the private key should not be stored in the clear"
    );
    assert!(
        sealed.contains("\"p\":") && sealed.contains("\"iv\":"),
        "expected a Rails encrypted message, got {sealed}"
    );
    assert_eq!(stored.local_fragment.as_deref(), Some("#main-key"));
    assert_eq!(stored.r#type, 0, "RSA");

    let legacy = sqlx::query!(
        "SELECT private_key, public_key FROM accounts WHERE id = $1",
        account_id,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(legacy.private_key, None);
    assert_eq!(legacy.public_key, "");

    // The key still comes back, and it is the same one.
    let loaded = keypair::signing_key(&ctx.state, account_id).await.unwrap();
    assert_eq!(loaded.private_key, private_key);

    // Running twice moves nothing the second time.
    let moved = keypair::migrate_local_keypairs(&ctx.state).await.unwrap();
    assert_eq!(moved, 0);
}

/// Once every local key has moved, the database says so, so that a Mastodon
/// booted on it does not try to move them again.
#[tokio::test]
async fn test_migrate_records_the_upstream_migration() {
    let ctx = TestContext::new("kp-record").await;
    let account_id: i64 = ctx.alice_id.parse().unwrap();
    seed_legacy_key(&ctx, account_id).await;

    keypair::migrate_local_keypairs(&ctx.state).await.unwrap();

    let recorded: bool = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM public.schema_migrations WHERE version = '20260702144128'
           ) AS "exists!""#,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(recorded);
}

/// A revoked key is not offered for signing, matching the model's `usable`
/// scope, and the account falls back to whatever else it has.
#[tokio::test]
async fn test_revoked_keypair_is_not_used() {
    let ctx = TestContext::new("kp-revoked").await;
    let account_id: i64 = ctx.alice_id.parse().unwrap();
    seed_legacy_key(&ctx, account_id).await;
    keypair::migrate_local_keypairs(&ctx.state).await.unwrap();

    sqlx::query!(
        "UPDATE keypairs SET revoked = true WHERE account_id = $1",
        account_id,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    assert!(!keypair::has_signing_key(&ctx.state, account_id)
        .await
        .unwrap());
    assert!(keypair::signing_key(&ctx.state, account_id).await.is_err());
}

/// The actor document advertises the public key from wherever it lives, so a
/// remote server can still verify what this one signs.
#[tokio::test]
async fn test_actor_document_serves_the_moved_public_key() {
    let ctx = TestContext::new("kp-actor").await;
    let account_id: i64 = ctx.alice_id.parse().unwrap();
    seed_legacy_key(&ctx, account_id).await;
    let public_key = keypair::public_key(&ctx.state, account_id)
        .await
        .unwrap()
        .unwrap();

    keypair::migrate_local_keypairs(&ctx.state).await.unwrap();

    let resp = ctx
        .api
        .ap_get(&format!("/ap/users/{account_id}"), None)
        .await;
    let actor: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        actor["publicKey"]["publicKeyPem"].as_str(),
        Some(public_key.as_str()),
        "the actor document lost its key when it moved"
    );
}
