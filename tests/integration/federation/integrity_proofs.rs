//! FEP-8b32 integrity proofs on outgoing activities, and the FEP-521a Multikey
//! that lets a peer check them.
//!
//! A proof authenticates the activity rather than the connection that carried
//! it, so a relayed or forwarded copy stays attributable. Mastodon 4.7 verifies
//! these; eunha produces them.

use serde_json::Value;

use crate::helpers::TestContext;

/// The activity queued for delivery carries a proof, and that proof verifies
/// against the key the actor publishes — the whole loop a receiving server
/// would walk.
#[tokio::test]
async fn test_delivered_activities_carry_a_verifiable_proof() {
    let ctx = TestContext::with_integrity_proofs("proof-sign", true).await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();

    // Only accounts with a signing key federate, and only remote followers
    // produce a delivery.
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    sqlx::query("UPDATE accounts SET private_key = $2, public_key = $3 WHERE id = $1")
        .bind(alice_id)
        .bind(&priv_pem)
        .bind(&pub_pem)
        .execute(&ctx.db)
        .await
        .unwrap();

    let remote_id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'nina', 'remote.invalid', 'Nina', '', $2, $2, '', $2 || '/inbox', $2 || '/outbox', now(), now())"#,
    )
    .bind(remote_id)
    .bind("https://remote.invalid/users/nina")
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())",
    )
    .bind(eunha::snowflake::next_id())
    .bind(remote_id)
    .bind(alice_id)
    .execute(&ctx.db)
    .await
    .unwrap();

    ctx.api
        .post_status(
            &ctx.alice_token,
            "a post that should travel with a proof",
            "public",
        )
        .await;

    // The queued activity is what every inbox receives.
    let activity: Value = sqlx::query_scalar(
        "SELECT payload->'activity' FROM eunha.ojak_queue WHERE queue IN ('delivery', 'delivery-priority') ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&ctx.db)
    .await
    .expect("an activity should have been queued");

    let proof = activity
        .get("proof")
        .expect("the queued activity should carry a proof");
    assert_eq!(
        proof.get("cryptosuite").and_then(Value::as_str),
        Some("eddsa-jcs-2022")
    );
    assert_eq!(
        proof.get("proofPurpose").and_then(Value::as_str),
        Some("assertionMethod")
    );

    // `proof` means nothing to a JSON-LD reader unless it is defined.
    let context = activity["@context"].to_string();
    assert!(
        context.contains("https://w3id.org/security/data-integrity/v1"),
        "the data integrity context must travel with the proof: {context}"
    );

    // The actor document publishes the key the proof names.
    let actor: Value = ctx
        .api
        .ap_get(&format!("/ap/users/{alice_id}"), None)
        .await
        .json()
        .await
        .unwrap();
    let methods = actor["assertionMethod"]
        .as_array()
        .expect("the actor should publish an assertionMethod");
    let method = &methods[0];
    assert_eq!(method["type"].as_str(), Some("Multikey"));
    assert_eq!(
        method["id"].as_str(),
        proof.get("verificationMethod").and_then(Value::as_str),
        "the published key should be the one the proof names"
    );

    // And the proof holds against it.
    let multikey = method["publicKeyMultibase"].as_str().unwrap();
    let key = ojak::sig::integrity::decode_multikey(multikey).expect("decode published key");
    let (parsed, _, _) =
        ojak::sig::integrity::extract_integrity_proof(&activity, chrono::Utc::now().timestamp())
            .expect("extract proof");
    ojak::sig::integrity::verify_object_integrity_proof(&activity, &parsed, &key)
        .expect("eunha's own proof must verify against the key it publishes");
}

/// An account that has never signed anything publishes no key, so a document
/// fetch does not mint one for every crawler that asks.
#[tokio::test]
async fn test_an_unused_account_publishes_no_key() {
    let ctx = TestContext::with_integrity_proofs("proof-nokey", true).await;
    let bob_id: i64 = ctx.bob_id.parse().unwrap();

    let actor: Value = ctx
        .api
        .ap_get(&format!("/ap/users/{bob_id}"), None)
        .await
        .json()
        .await
        .unwrap();

    assert!(
        actor.get("assertionMethod").is_none_or(Value::is_null),
        "an account that has signed nothing should publish no assertion key"
    );
}

/// Off by default, and the switch actually stops it: an instance that has not
/// asked for proofs sends what Mastodon sends.
#[tokio::test]
async fn test_proofs_are_not_attached_unless_asked_for() {
    assert!(
        !eunha::config::default_sign_integrity_proofs(),
        "signing proofs should be opt-in"
    );

    // Plain `new` takes the shipped default, which is what this asserts.
    let ctx = TestContext::new("proof-off").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();

    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    sqlx::query("UPDATE accounts SET private_key = $2, public_key = $3 WHERE id = $1")
        .bind(alice_id)
        .bind(&priv_pem)
        .bind(&pub_pem)
        .execute(&ctx.db)
        .await
        .unwrap();

    let remote_id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'olive', 'remote.invalid', 'Olive', '', $2, $2, '', $2 || '/inbox', $2 || '/outbox', now(), now())"#,
    )
    .bind(remote_id)
    .bind("https://remote.invalid/users/olive")
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())",
    )
    .bind(eunha::snowflake::next_id())
    .bind(remote_id)
    .bind(alice_id)
    .execute(&ctx.db)
    .await
    .unwrap();

    ctx.api
        .post_status(&ctx.alice_token, "a post that should travel bare", "public")
        .await;

    let activity: Value = sqlx::query_scalar(
        "SELECT payload->'activity' FROM eunha.ojak_queue WHERE queue IN ('delivery', 'delivery-priority') ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&ctx.db)
    .await
    .expect("an activity should have been queued");

    assert!(
        activity.get("proof").is_none(),
        "no proof should be attached when the option is off"
    );
    // And no key was minted for an account that never signed one.
    let keys: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM keypairs WHERE account_id = $1 AND local_fragment = '#ed25519-key'",
    )
    .bind(alice_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(keys, 0);
}
