//! Inbound HTTP Signature enforcement on the shared inbox.

use reqwest::StatusCode;
use serde_json::json;

use crate::helpers::TestContext;

/// An activity with no HTTP Signature is rejected with 401.
#[tokio::test]
async fn test_unsigned_activity_rejected() {
    let ctx = TestContext::new("sig-unsigned").await;

    let follow = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.invalid/activities/1",
        "type": "Follow",
        "actor": "https://remote.invalid/users/eve",
        "object": format!("https://{}/users/alice", ctx.domain),
    });

    let resp = ctx.api.post_json("/inbox", None, &follow).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "unsigned activity must be rejected"
    );
}

/// A signature whose keyId host differs from the activity's actor host
/// authenticates the signer, not the actor: the activity is taken as
/// forwarded, and since the actor's server does not serve it, it is dropped
/// and does nothing. The answer is 202, as Mastodon 4.7.1 answers it — it
/// accepts a delivery whose signature verifies (`InboxesController#create`)
/// and drops a relayed activity it cannot verify
/// (`ActivityPub::ProcessActivityService`) — so the sender does not retry.
#[tokio::test]
async fn test_signature_actor_host_mismatch_does_nothing() {
    let ctx = TestContext::new("sig-mismatch").await;

    // A real keypair for a key on attacker.invalid …
    let (priv_pem, pub_pem) = eunha::crypto::generate_rsa_keypair().unwrap();
    let attacker_uri = "https://attacker.invalid/users/eve";
    sqlx::query!(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'eve', 'attacker.invalid', 'eve', '', $2::text, $2::text, $3, $2::text||'/inbox', $2::text||'/outbox', now(), now())"#,
        eunha::snowflake::next_id(),
        attacker_uri,
        pub_pem,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    // … used to sign an activity that claims to be from victim.invalid.
    let activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://victim.invalid/activities/1",
        "type": "Follow",
        "actor": "https://victim.invalid/users/victim",
        "object": format!("https://{}/users/alice", ctx.domain),
    });
    let resp = ctx
        .api
        .post_signed(
            "/inbox",
            &activity,
            &format!("{attacker_uri}#main-key"),
            &priv_pem,
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "a forwarded activity that cannot be established is accepted and dropped"
    );
    let follows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM follows f JOIN accounts a ON a.id = f.target_account_id
         WHERE a.username = 'alice' AND a.domain IS NULL",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let requests: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM follow_requests f JOIN accounts a ON a.id = f.target_account_id
         WHERE a.username = 'alice' AND a.domain IS NULL",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        (follows, requests),
        (0, 0),
        "the forged Follow must not take effect"
    );
    let victim: i64 =
        sqlx::query_scalar("SELECT count(*) FROM accounts WHERE domain = 'victim.invalid'")
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(victim, 0, "nor create the actor it claimed");
}

/// An unverifiable `Delete` is accepted-and-ignored (202) rather than rejected,
/// to avoid backscatter when the actor or its key is already gone.
#[tokio::test]
async fn test_unsigned_delete_accepted_without_processing() {
    let ctx = TestContext::new("sig-delete").await;

    let delete = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.invalid/activities/del-1",
        "type": "Delete",
        "actor": "https://remote.invalid/users/ghost",
        "object": "https://remote.invalid/users/ghost",
    });

    let resp = ctx.api.post_json("/inbox", None, &delete).await;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "unverified Delete should be accepted without processing"
    );
}

/// A signature built the way Mastodon builds it — covered headers in its
/// order, `(request-target)` last and carrying the query string — is accepted.
/// Constructed by hand rather than through eunha's signer, so that it stays a
/// test of what the network sends rather than of what eunha happens to emit.
#[tokio::test]
async fn test_mastodon_shaped_signature_is_accepted() {
    use base64::Engine as _;
    use sha2::Digest as _;

    let ctx = TestContext::new("cavage-mastodon").await;
    let (priv_pem, pub_pem) = eunha::crypto::generate_rsa_keypair().unwrap();
    let uri = "https://remote.invalid/users/mona";
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'mona', 'remote.invalid', 'Mona', '', $2, $2, $3, $2 || '/inbox', $2 || '/outbox', now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(uri)
    .bind(&pub_pem)
    .execute(&ctx.db)
    .await
    .unwrap();

    let activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{uri}#update-mastodon-shape"),
        "type": "Update",
        "actor": uri,
        "object": {
            "id": uri,
            "type": "Person",
            "preferredUsername": "mona",
            "name": "Mona, via a Mastodon-shaped signature",
            "inbox": format!("{uri}/inbox"),
        },
    });
    let body = serde_json::to_vec(&activity).unwrap();

    let digest = format!(
        "SHA-256={}",
        base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(&body))
    );
    let date = chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let content_type = "application/activity+json";
    // A query string, which Mastodon's `(request-target)` covers and which a
    // verifier that only rebuilt the path would get wrong.
    let path_and_query = "/inbox?shared=true";

    // Mastodon's order: host, date, content-type, digest, (request-target).
    let signing_string = format!(
        "host: {}\ndate: {date}\ncontent-type: {content_type}\ndigest: {digest}\n\
         (request-target): post {path_and_query}",
        ctx.api.host,
    );
    let signature = sign_rsa_sha256(&priv_pem, signing_string.as_bytes());
    let header = format!(
        r#"keyId="{uri}#main-key",algorithm="rsa-sha256",headers="host date content-type digest (request-target)",signature="{signature}""#
    );

    let resp = ctx
        .api
        .http
        .post(ctx.api.url(path_and_query))
        .header("host", &ctx.api.host)
        .header("date", date)
        .header("content-type", content_type)
        .header("digest", digest)
        .header("signature", header)
        .body(body)
        .send()
        .await
        .unwrap();

    assert!(
        resp.status().is_success(),
        "a Mastodon-shaped signature should verify, got {}",
        resp.status()
    );

    let display_name: String =
        sqlx::query_scalar("SELECT display_name FROM accounts WHERE uri = $1")
            .bind(uri)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(display_name, "Mona, via a Mastodon-shaped signature");
}

/// Sign with RSASSA-PKCS1-v1_5 over SHA-256, the way any HTTP Signature
/// implementation does. Written out here so the test does not lean on the
/// signer it is meant to be checking against.
fn sign_rsa_sha256(private_key_pem: &str, message: &[u8]) -> String {
    use base64::Engine as _;
    use rsa::pkcs1v15::SigningKey;
    use rsa::pkcs8::DecodePrivateKey as _;
    use rsa::signature::{SignatureEncoding as _, Signer as _};

    let key = rsa::RsaPrivateKey::from_pkcs8_pem(private_key_pem).expect("parse test key");
    let signature: rsa::pkcs1v15::Signature = SigningKey::<sha2::Sha256>::new(key).sign(message);
    base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
}
