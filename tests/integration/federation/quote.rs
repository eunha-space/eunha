//! End-to-end FEP-044f quote consent handshake across two eunha instances.
//!
//! Two real servers (separate databases + domains) talk through their actual
//! HTTP inbox / authorization endpoints. The test plays the network, relaying
//! the activities one server emits to the other's inbox. Because the test
//! domains are unreachable (`*.c2s-test.invalid`), each side is cross-seeded
//! with the other's account/status so no outbound fetch is required.

use reqwest::StatusCode;
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::helpers::TestContext;

/// Insert a remote account into `db`, returning its local id.
async fn seed_remote_account(db: &PgPool, username: &str, domain: &str) -> (i64, String) {
    let uri = format!("https://{domain}/users/{username}");
    let inbox = format!("{uri}/inbox");
    let id = sqlx::query_scalar!(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, shared_inbox_url, discoverable, created_at, updated_at)
           VALUES ($1,$2,$3,$2,'',$4::text,$4::text,'remote-key',$5,$4::text||'/outbox',''::text,true, now(), now())
           RETURNING id"#,
        eunha::snowflake::next_id(),
        username,
        domain,
        uri,
        inbox,
    )
    .fetch_one(db)
    .await
    .unwrap();
    (id, uri)
}

/// Insert a remote status into `db`, returning its local id.
async fn seed_remote_status(db: &PgPool, account_id: i64, uri: &str) -> i64 {
    sqlx::query_scalar!(
        r#"INSERT INTO statuses (id, account_id, text, visibility, uri, quote_approval_policy, created_at, updated_at)
           VALUES ($1, $2, 'remote post', 0, $3, 131072, now(), now())
           RETURNING id"#,
        eunha::snowflake::next_id(),
        account_id,
        uri,
    )
    .fetch_one(db)
    .await
    .unwrap()
}

#[tokio::test]
async fn test_quote_consent_handshake_between_instances() {
    // Instance B hosts the quoted author (bob); instance A hosts the quoter (alice).
    let a = TestContext::new("qfed-a").await;
    let b = TestContext::new("qfed-b").await;

    // ── B: bob publishes a public status to be quoted ──────────────────────────
    let s: Value = b
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&b.alice_token),
            &json!({"status": "quote me", "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    // B matches incoming QuoteRequests against the stored uri, and that is what
    // it federates, so read it from B's own DB.
    let s_id: i64 = s["id"].as_str().unwrap().parse().unwrap();
    let s_uri: String = sqlx::query_scalar!("SELECT uri FROM statuses WHERE id = $1", s_id)
        .fetch_one(&b.db)
        .await
        .unwrap()
        .expect("local status must have a stored uri");
    // The API's `uri` used to come from a process-wide domain, set by whichever
    // instance in the process started first. Two instances share this process.
    assert!(
        s["uri"]
            .as_str()
            .is_some_and(|uri| uri.starts_with(&format!("https://{}/", b.domain))),
        "B's API must name B's own domain, not another instance's in the same process: {}",
        s["uri"],
    );
    // b.alice is the author on instance B; treat it as "bob" for clarity.
    let bob_uri = format!("https://{}/users/alice", b.domain);

    // ── A: cross-seed bob + his status, then alice quotes it ───────────────────
    let (bob_in_a, _) = seed_remote_account(&a.db, "alice", &b.domain).await;
    let s_in_a = seed_remote_status(&a.db, bob_in_a, &s_uri).await;
    // bob signs the Accept he later sends to A, so A must know bob's public key.
    let (bob_priv, bob_pub) = eunha::crypto::generate_rsa_keypair().unwrap();
    sqlx::query!(
        "UPDATE accounts SET public_key = $2 WHERE id = $1",
        bob_in_a,
        bob_pub
    )
    .execute(&a.db)
    .await
    .unwrap();

    let quote_post: Value = a
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&a.alice_token),
            &json!({"status": "great post!", "visibility": "public", "quoted_status_id": s_in_a.to_string()}),
        )
        .await
        .json()
        .await
        .unwrap();
    // As for bob's status above: the stored uri is what A federates, and the
    // API's must name A, not B.
    assert!(
        quote_post["uri"]
            .as_str()
            .is_some_and(|uri| uri.starts_with(&format!("https://{}/", a.domain))),
        "A's API must name A's own domain, not another instance's in the same process: {}",
        quote_post["uri"],
    );
    let quote_post_id: i64 = quote_post["id"].as_str().unwrap().parse().unwrap();
    let quote_post_uri: String =
        sqlx::query_scalar!("SELECT uri FROM statuses WHERE id = $1", quote_post_id)
            .fetch_one(&a.db)
            .await
            .unwrap()
            .expect("local status must have a stored uri");
    let alice_uri = format!("https://{}/users/alice", a.domain);

    // A recorded the quote as pending with a QuoteRequest activity_uri.
    let (activity_uri, state): (Option<String>, i32) = sqlx::query!(
        r#"SELECT q.activity_uri, q.state
           FROM quotes q JOIN statuses s ON s.id = q.status_id
           WHERE s.uri = $1"#,
        quote_post_uri,
    )
    .fetch_one(&a.db)
    .await
    .map(|r| (r.activity_uri, r.state))
    .unwrap();
    assert_eq!(state, 0, "quote of a remote post should start pending");
    let activity_uri =
        activity_uri.expect("pending remote quote must carry a QuoteRequest activity_uri");

    // ── Relay the QuoteRequest to B's inbox ────────────────────────────────────
    // Cross-seed alice + her quote post on B so it needs no outbound fetch.
    let (alice_in_b, _) = seed_remote_account(&b.db, "alice-remote", &a.domain).await;
    // Override the seeded uri to alice's real actor uri so resolution matches,
    // and store alice's public key so B can verify her signed QuoteRequest.
    let (alice_priv, alice_pub) = eunha::crypto::generate_rsa_keypair().unwrap();
    sqlx::query!(
        "UPDATE accounts SET uri = $2, url = $2, public_key = $3 WHERE id = $1",
        alice_in_b,
        alice_uri,
        alice_pub,
    )
    .execute(&b.db)
    .await
    .unwrap();
    seed_remote_status(&b.db, alice_in_b, &quote_post_uri).await;

    // The quoted account needs a private key to get past the delivery guard in
    // handle_quote_request (the Accept it sends is best-effort / fire-and-forget).
    sqlx::query!(
        "UPDATE accounts SET private_key = 'test-key' WHERE username = 'alice' AND domain IS NULL",
    )
    .execute(&b.db)
    .await
    .unwrap();

    let quote_request = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": activity_uri,
        "type": "QuoteRequest",
        "actor": alice_uri,
        "object": s_uri,
        "instrument": quote_post_uri,
    });
    let resp = b
        .api
        .post_signed(
            "/inbox",
            &quote_request,
            &format!("{alice_uri}#main-key"),
            &alice_priv,
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "B should accept the QuoteRequest"
    );

    // B recorded an accepted quote with an approval (authorization) URI.
    let approval_uri: String = sqlx::query_scalar!(
        r#"SELECT q.approval_uri
           FROM quotes q JOIN statuses s ON s.id = q.quoted_status_id
           WHERE s.uri = $1 AND q.state = 1"#,
        s_uri,
    )
    .fetch_one(&b.db)
    .await
    .unwrap()
    .expect("B should stamp an approval_uri on accept");
    assert!(
        approval_uri.contains("/quote_authorizations/"),
        "approval should point at a QuoteAuthorization: {approval_uri}",
    );

    // And it counts. Mastodon's `Quote#increment_counter_caches!` runs
    // `return unless accepted?`, so accepting is exactly when the quoted post's
    // count should rise — whether the quote came from a local client or, as
    // here, from another instance.
    let quotes_count: Option<i64> = sqlx::query_scalar!(
        r#"SELECT ss.quotes_count FROM status_stats ss
           JOIN statuses s ON s.id = ss.status_id
           WHERE s.uri = $1"#,
        s_uri,
    )
    .fetch_optional(&b.db)
    .await
    .unwrap();
    assert_eq!(
        quotes_count,
        Some(1),
        "accepting a federated quote must count it on the quoted post"
    );

    // The QuoteAuthorization stamp is fetchable on B and well-formed.
    let auth_path = approval_uri
        .strip_prefix(&format!("https://{}", b.domain))
        .unwrap();
    let auth: Value = b.api.ap_get(auth_path, None).await.json().await.unwrap();
    assert_eq!(auth["type"].as_str(), Some("QuoteAuthorization"));
    assert_eq!(auth["interactionTarget"].as_str(), Some(s_uri.as_str()));
    assert_eq!(
        auth["interactingObject"].as_str(),
        Some(quote_post_uri.as_str())
    );
    assert_eq!(auth["attributedTo"].as_str(), Some(bob_uri.as_str()));

    // ── Relay B's Accept back to A's inbox ─────────────────────────────────────
    let accept = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{bob_uri}#accepts/quote_requests/1"),
        "type": "Accept",
        "actor": bob_uri,
        "to": alice_uri,
        "object": activity_uri,
        "result": approval_uri,
    });
    let resp = a
        .api
        .post_signed("/inbox", &accept, &format!("{bob_uri}#main-key"), &bob_priv)
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "A should accept the Accept"
    );

    // A's quote is now accepted and carries the approval URI from B.
    let (final_state, final_approval): (i32, Option<String>) = sqlx::query!(
        r#"SELECT q.state, q.approval_uri
           FROM quotes q JOIN statuses s ON s.id = q.status_id
           WHERE s.uri = $1"#,
        quote_post_uri,
    )
    .fetch_one(&a.db)
    .await
    .map(|r| (r.state, r.approval_uri))
    .unwrap();
    assert_eq!(
        final_state, 1,
        "A's quote should be accepted after B's Accept"
    );
    assert_eq!(final_approval.as_deref(), Some(approval_uri.as_str()));
}
