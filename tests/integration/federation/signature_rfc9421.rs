//! RFC 9421 HTTP Message Signatures on what arrives.
//!
//! A peer that has moved on from the cavage draft still reaches this
//! instance. Sending in the other scheme when a peer refuses the first is
//! Ojak's, and tested there.

use serde_json::json;

/// An inbound activity signed with RFC 9421 is accepted, so a peer that has
/// stopped sending cavage signatures can still reach this instance.
#[tokio::test]
async fn test_inbound_rfc9421_activity_is_accepted() {
    use crate::helpers::TestContext;

    let ctx = TestContext::new("rfc9421-in").await;
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    let uri = "https://remote.invalid/users/kim";
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'kim', 'remote.invalid', 'Kim', '', $2, $2, $3, $2 || '/inbox', $2 || '/outbox', now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(uri)
    .bind(&pub_pem)
    .execute(&ctx.db)
    .await
    .unwrap();

    let activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{uri}#update-1"),
        "type": "Update",
        "actor": uri,
        "object": {
            "id": uri,
            "type": "Person",
            "preferredUsername": "kim",
            "name": "Kim, updated",
            "inbox": format!("{uri}/inbox"),
        },
    });

    let resp = ctx
        .api
        .post_signed_rfc9421("/inbox", &activity, &format!("{uri}#main-key"), &priv_pem)
        .await;
    assert!(
        resp.status().is_success(),
        "an RFC 9421 signed activity should be accepted, got {}",
        resp.status()
    );

    let display_name: String =
        sqlx::query_scalar("SELECT display_name FROM accounts WHERE uri = $1")
            .bind(uri)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(display_name, "Kim, updated");
}

/// A body swapped after signing is rejected: the signature covers the digest,
/// and the digest covers the body.
#[tokio::test]
async fn test_inbound_rfc9421_rejects_a_swapped_body() {
    use crate::helpers::TestContext;

    let ctx = TestContext::new("rfc9421-swap").await;
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    let uri = "https://remote.invalid/users/lee";
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'lee', 'remote.invalid', 'Lee', '', $2, $2, $3, $2 || '/inbox', $2 || '/outbox', now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(uri)
    .bind(&pub_pem)
    .execute(&ctx.db)
    .await
    .unwrap();

    let signed_body = json!({"id": format!("{uri}#u"), "type": "Update", "actor": uri,
                             "object": {"id": uri, "type": "Person", "name": "signed"}});
    let signed = ojak::sig::rfc9421::sign_request(
        "post",
        &format!("https://{}/inbox", ctx.api.host),
        Some(&serde_json::to_vec(&signed_body).unwrap()),
        &format!("{uri}#main-key"),
        &ojak::sig::rfc9421::SigningKey::RsaPem(&priv_pem),
        chrono::Utc::now().timestamp(),
    )
    .unwrap();

    let swapped = json!({"id": format!("{uri}#u"), "type": "Update", "actor": uri,
                         "object": {"id": uri, "type": "Person", "name": "swapped"}});
    let resp = ctx
        .api
        .http
        .post(ctx.api.url("/inbox"))
        .header("host", &ctx.api.host)
        .header("signature-input", signed.signature_input)
        .header("signature", signed.signature)
        .header("content-digest", signed.content_digest.unwrap())
        .header("content-type", "application/activity+json")
        .body(serde_json::to_vec(&swapped).unwrap())
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        401,
        "a body the signature does not cover must be rejected"
    );
}
