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
    let quote_post_in_b = seed_remote_status(&b.db, alice_in_b, &quote_post_uri).await;
    // B took alice's Create of the quote post, and recorded the quote it
    // makes pending: a quote of a local post waits for its QuoteRequest
    // (`VerifyQuoteService` returns early for a local quoted author).
    let bob_in_b: i64 = b.alice_id.parse().unwrap();
    sqlx::query!(
        r#"INSERT INTO quotes (id, status_id, quoted_status_id, account_id, quoted_account_id, state, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, 0, now(), now())"#,
        eunha::snowflake::next_id(),
        quote_post_in_b,
        s_id,
        alice_in_b,
        bob_in_b,
    )
    .execute(&b.db)
    .await
    .unwrap();

    // Bob signs on B with the key A knows him by, so that what B sends for
    // him verifies on A.
    sqlx::query!(
        "UPDATE accounts SET private_key = $1, public_key = $2 WHERE username = 'alice' AND domain IS NULL",
        bob_priv,
        bob_pub,
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

    // B accepted the quote. A local quoted author's stamp is named, not
    // stored (`validates :approval_uri, absence: true, if: -> {
    // quoted_account&.local? }`), and the quoted author is mentioned silently
    // in the quote post (`ensure_quoted_access`).
    let (accepted_state, stored_approval): (i32, Option<String>) = sqlx::query!(
        "SELECT state, approval_uri FROM quotes WHERE status_id = $1",
        quote_post_in_b,
    )
    .fetch_one(&b.db)
    .await
    .map(|r| (r.state, r.approval_uri))
    .unwrap();
    assert_eq!(accepted_state, 1);
    assert_eq!(stored_approval, None);
    let silent: bool = sqlx::query_scalar!(
        "SELECT silent FROM mentions WHERE status_id = $1 AND account_id = $2",
        quote_post_in_b,
        bob_in_b,
    )
    .fetch_one(&b.db)
    .await
    .unwrap();
    assert!(silent, "the quoted author is mentioned silently");

    // The Accept went to alice's own inbox, with the QuoteRequest it answers
    // and the stamp it grants (`AcceptQuoteRequestSerializer`).
    let accept: Value = sqlx::query_scalar(
        r#"SELECT payload->'activity' FROM eunha.ojak_queue
           WHERE payload->'activity'->>'type' = 'Accept' AND payload->>'inbox' = $1"#,
    )
    .bind(format!("https://{}/users/alice-remote/inbox", a.domain))
    .fetch_one(&b.db)
    .await
    .unwrap();
    assert_eq!(accept["actor"].as_str(), Some(bob_uri.as_str()));
    assert_eq!(accept["object"]["type"], "QuoteRequest");
    assert_eq!(accept["object"]["id"].as_str(), Some(activity_uri.as_str()));
    assert_eq!(accept["object"]["actor"].as_str(), Some(alice_uri.as_str()));
    assert_eq!(accept["object"]["object"].as_str(), Some(s_uri.as_str()));
    assert_eq!(
        accept["object"]["instrument"].as_str(),
        Some(quote_post_uri.as_str())
    );
    let approval_uri = accept["result"].as_str().unwrap().to_owned();
    assert!(
        approval_uri.starts_with(&format!("{bob_uri}/quote_authorizations/")),
        "approval should point at a QuoteAuthorization: {approval_uri}",
    );

    // Bob hears that alice quoted him, about the Quote itself.
    let notified: Option<String> = sqlx::query_scalar!(
        r#"SELECT activity_type FROM notifications WHERE account_id = $1 AND "type" = 'quote'"#,
        bob_in_b,
    )
    .fetch_optional(&b.db)
    .await
    .unwrap();
    assert_eq!(notified.as_deref(), Some("Quote"));

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
    assert_eq!(auth["id"].as_str(), Some(approval_uri.as_str()));
    assert_eq!(
        auth["@context"][1]["QuoteAuthorization"],
        "https://w3id.org/fep/044f#QuoteAuthorization"
    );
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

    // ── B: bob takes the quote back ────────────────────────────────────────────
    let resp = b
        .api
        .post_json(
            &format!("/api/v1/statuses/{s_id}/quotes/{quote_post_in_b}/revoke"),
            Some(&b.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let revoked: Value = resp.json().await.unwrap();
    assert_eq!(
        revoked["id"].as_str(),
        Some(quote_post_in_b.to_string().as_str())
    );
    assert_eq!(revoked["quote"]["state"], "revoked");
    assert!(revoked["quote"]["quoted_status"].is_null());

    // `Quote#reject!`: an accepted quote is revoked, and stops counting.
    let revoked_state: i32 = sqlx::query_scalar!(
        "SELECT state FROM quotes WHERE status_id = $1",
        quote_post_in_b
    )
    .fetch_one(&b.db)
    .await
    .unwrap();
    assert_eq!(revoked_state, 3);
    let quotes_count: i64 = sqlx::query_scalar!(
        "SELECT quotes_count FROM status_stats WHERE status_id = $1",
        s_id
    )
    .fetch_one(&b.db)
    .await
    .unwrap();
    assert_eq!(quotes_count, 0, "a revoked quote no longer counts");
    // The stamp is gone with it.
    let resp = b.api.ap_get(auth_path, None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // The stamp's Delete goes to whoever saw either post — alice, who quoted
    // it — signed by bob whatever the mode (`always_sign`).
    let delete: Value = sqlx::query_scalar(
        r#"SELECT payload->'activity' FROM eunha.ojak_queue
           WHERE payload->'activity'->>'type' = 'Delete' AND payload->>'inbox' = $1"#,
    )
    .bind(format!("https://{}/users/alice-remote/inbox", a.domain))
    .fetch_one(&b.db)
    .await
    .unwrap();
    assert_eq!(
        delete["id"].as_str(),
        Some(format!("{approval_uri}#delete").as_str())
    );
    assert_eq!(delete["actor"].as_str(), Some(bob_uri.as_str()));
    assert_eq!(
        delete["to"],
        json!(["https://www.w3.org/ns/activitystreams#Public"])
    );
    assert_eq!(delete["object"]["type"], "QuoteAuthorization");
    assert_eq!(delete["object"]["id"].as_str(), Some(approval_uri.as_str()));
    assert_eq!(
        delete["object"]["attributedTo"].as_str(),
        Some(bob_uri.as_str())
    );
    assert_eq!(
        delete["object"]["interactingObject"].as_str(),
        Some(quote_post_uri.as_str())
    );
    assert_eq!(
        delete["object"]["interactionTarget"].as_str(),
        Some(s_uri.as_str())
    );
    assert_eq!(delete["signature"]["type"], "RsaSignature2017");
    ojak::sig::linked_data::verify(
        &ojak_jsonld::Registry::bundled(),
        &delete,
        &bob_pub,
        chrono::Utc::now().timestamp(),
    )
    .expect("the Delete carries bob's Linked Data signature");

    // ── Relay the Delete to A ──────────────────────────────────────────────────
    let resp = a
        .api
        .post_signed("/inbox", &delete, &format!("{bob_uri}#main-key"), &bob_priv)
        .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // `Delete#revoke_quote`: A's quote is revoked, its stamp forgotten.
    let (a_state, a_approval): (i32, Option<String>) = sqlx::query!(
        r#"SELECT q.state, q.approval_uri
           FROM quotes q JOIN statuses s ON s.id = q.status_id
           WHERE s.uri = $1"#,
        quote_post_uri,
    )
    .fetch_one(&a.db)
    .await
    .map(|r| (r.state, r.approval_uri))
    .unwrap();
    assert_eq!(a_state, 3, "A's quote is revoked by the stamp's Delete");
    assert_eq!(a_approval, None);
    // And the post no longer claims a stamp when A serves it.
    let note_path = quote_post_uri
        .strip_prefix(&format!("https://{}", a.domain))
        .unwrap();
    let note: Value = a.api.ap_get(note_path, None).await.json().await.unwrap();
    assert_eq!(note["quote"].as_str(), Some(s_uri.as_str()));
    assert!(note.get("quoteAuthorization").is_none());
}

// ── The rest of a quote's life ───────────────────────────────────────────────

/// A remote account with a key, known here as Mastodon would know it.
async fn seed_remote_with_key(
    ctx: &TestContext,
    username: &str,
    domain: &str,
) -> (i64, String, String) {
    let (priv_pem, pub_pem) = eunha::crypto::generate_rsa_keypair().unwrap();
    let uri = format!("https://{domain}/users/{username}");
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, $5, $4 || '/inbox', $4 || '/outbox', now(), now())"#,
    )
    .bind(id)
    .bind(username)
    .bind(domain)
    .bind(&uri)
    .bind(&pub_pem)
    .execute(&ctx.db)
    .await
    .unwrap();
    (id, uri, priv_pem)
}

/// Give `account_id` a key, and return its public half.
async fn give_key(ctx: &TestContext, account_id: i64) -> String {
    let (priv_pem, pub_pem) = eunha::crypto::generate_rsa_keypair().unwrap();
    sqlx::query("UPDATE accounts SET private_key = $2, public_key = $3 WHERE id = $1")
        .bind(account_id)
        .bind(&priv_pem)
        .bind(&pub_pem)
        .execute(&ctx.db)
        .await
        .unwrap();
    pub_pem
}

async fn follow(ctx: &TestContext, follower: i64, target: i64) {
    sqlx::query(
        "INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())",
    )
    .bind(eunha::snowflake::next_id())
    .bind(follower)
    .bind(target)
    .execute(&ctx.db)
    .await
    .unwrap();
}

/// The activities of `kind` queued for `inbox`, oldest first.
async fn queued_for(ctx: &TestContext, kind: &str, inbox: &str) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT payload->'activity' FROM eunha.ojak_queue
         WHERE payload->'activity'->>'type' = $1 AND payload->>'inbox' = $2
         ORDER BY id",
    )
    .bind(kind)
    .bind(inbox)
    .fetch_all(&ctx.db)
    .await
    .unwrap()
}

async fn quote_state(ctx: &TestContext, status_id: i64) -> i32 {
    sqlx::query_scalar("SELECT state FROM quotes WHERE status_id = $1")
        .bind(status_id)
        .fetch_one(&ctx.db)
        .await
        .unwrap()
}

async fn quotes_count(ctx: &TestContext, status_id: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT COALESCE((SELECT quotes_count FROM status_stats WHERE status_id = $1), 0)",
    )
    .bind(status_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

/// A local quote of a local post: the quoted author hears of it, about the
/// Quote; is mentioned silently in it, and stays so through an edit; and
/// revoking it tells the servers that saw the quoted post, by a signed
/// `Delete` of the stamp.
#[tokio::test]
async fn test_a_local_quote_is_notified_and_its_revocation_federated() {
    let ctx = TestContext::new("quote-local-life").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    let bob_id: i64 = ctx.bob_id.parse().unwrap();
    let alice_pub = give_key(&ctx, alice_id).await;
    let (nina_id, nina_uri, _) = seed_remote_with_key(&ctx, "nina", "nina.invalid").await;
    follow(&ctx, nina_id, alice_id).await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "quote me", "public")
        .await;
    let original_id: i64 = original["id"].as_str().unwrap().parse().unwrap();
    let quote: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "bob quotes", "quoted_status_id": original_id.to_string(), "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let quote_id: i64 = quote["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(quote_state(&ctx, quote_id).await, 1);
    assert_eq!(quotes_count(&ctx, original_id).await, 1);

    // `notify_quoted_account!`, about the Quote.
    let activity_type: String = sqlx::query_scalar(
        r#"SELECT activity_type FROM notifications WHERE account_id = $1 AND "type" = 'quote'"#,
    )
    .bind(alice_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(activity_type, "Quote");
    let notifications: Vec<Value> = ctx
        .api
        .get(
            "/api/v1/notifications?types[]=quote",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(notifications.len(), 1);
    assert_eq!(notifications[0]["type"], "quote");
    assert_eq!(
        notifications[0]["account"]["id"].as_str(),
        Some(ctx.bob_id.as_str())
    );
    assert_eq!(
        notifications[0]["status"]["id"].as_str(),
        Some(quote_id.to_string().as_str())
    );

    // `ensure_quoted_access`, which an edit leaves alone.
    let silent = |db: sqlx::PgPool| async move {
        sqlx::query_scalar::<_, bool>(
            "SELECT silent FROM mentions WHERE status_id = $1 AND account_id = $2",
        )
        .bind(quote_id)
        .bind(alice_id)
        .fetch_optional(&db)
        .await
        .unwrap()
    };
    assert_eq!(silent(ctx.db.clone()).await, Some(true));
    let resp = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{quote_id}"),
            Some(&ctx.bob_token),
            &json!({"status": "bob quotes, edited"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(silent(ctx.db.clone()).await, Some(true));

    // An edit of the quoted post tells the quoter, and a second edit replaces
    // the first word of it rather than adding to it.
    for text in ["quote me, edited", "quote me, edited again"] {
        let resp = ctx
            .api
            .put_json(
                &format!("/api/v1/statuses/{original_id}"),
                Some(&ctx.alice_token),
                &json!({"status": text}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
    let quoted_updates: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM notifications
           WHERE account_id = $1 AND "type" = 'quoted_update' AND activity_type = 'Status' AND activity_id = $2"#,
    )
    .bind(bob_id)
    .bind(quote_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(quoted_updates, 1);

    // Someone else may not revoke it.
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{original_id}/quotes/{quote_id}/revoke"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{original_id}/quotes/{quote_id}/revoke"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(quote_state(&ctx, quote_id).await, 3);
    assert_eq!(quotes_count(&ctx, original_id).await, 0);

    // Nina, who follows alice, saw the quoted post, and is told the stamp is
    // gone, in alice's name.
    let deletes = queued_for(&ctx, "Delete", &format!("{nina_uri}/inbox")).await;
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    let delete = &deletes[0];
    let alice_uri = format!("https://{}/users/alice", ctx.domain);
    let stamp = format!("{alice_uri}/quote_authorizations/");
    assert!(delete["id"].as_str().unwrap().starts_with(&stamp));
    assert!(delete["id"].as_str().unwrap().ends_with("#delete"));
    assert_eq!(delete["actor"].as_str(), Some(alice_uri.as_str()));
    assert_eq!(delete["object"]["type"], "QuoteAuthorization");
    assert_eq!(
        delete["object"]["attributedTo"].as_str(),
        Some(alice_uri.as_str())
    );
    assert_eq!(
        delete["@context"][1]["interactingObject"],
        json!({"@id": "gts:interactingObject", "@type": "@id"})
    );
    ojak::sig::linked_data::verify(
        &ojak_jsonld::Registry::bundled(),
        delete,
        &alice_pub,
        chrono::Utc::now().timestamp(),
    )
    .expect("alice's Linked Data signature");

    // The quoting post no longer carries the quoted one.
    let status: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{quote_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(status["quote"]["state"], "revoked");
    assert!(status["quote"]["quoted_status"].is_null());
}

/// Deleting a quote of a local post revokes it first (`RemoveStatusService`
/// calls `RevokeQuoteService`), so the servers that saw the quoted post hear
/// the stamp is gone.
#[tokio::test]
async fn test_deleting_a_quote_of_a_local_post_revokes_it() {
    let ctx = TestContext::new("quote-delete-revokes").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    let bob_id: i64 = ctx.bob_id.parse().unwrap();
    give_key(&ctx, alice_id).await;
    give_key(&ctx, bob_id).await;
    let (nina_id, nina_uri, _) = seed_remote_with_key(&ctx, "nina", "nina.invalid").await;
    follow(&ctx, nina_id, alice_id).await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "quote me", "public")
        .await;
    let original_id: i64 = original["id"].as_str().unwrap().parse().unwrap();
    let quote: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "bob quotes", "quoted_status_id": original_id.to_string(), "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let quote_id: i64 = quote["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(quotes_count(&ctx, original_id).await, 1);

    let resp = ctx
        .api
        .delete(&format!("/api/v1/statuses/{quote_id}"), &ctx.bob_token)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(quote_state(&ctx, quote_id).await, 3);
    assert_eq!(
        quotes_count(&ctx, original_id).await,
        0,
        "counted down once"
    );
    let deletes = queued_for(&ctx, "Delete", &format!("{nina_uri}/inbox")).await;
    assert!(
        deletes
            .iter()
            .any(|d| d["object"]["type"] == "QuoteAuthorization"),
        "{deletes:?}"
    );
}

/// A remote post's quote waits for its stamp: one of a local post is pending
/// until its author's `QuoteRequest` is accepted, a self-quote is accepted
/// at once, a quote of a post that is gone is `deleted`, and one whose stamp
/// cannot be fetched for now stays pending. The quoted author's answers,
/// `Accept` and `Reject`, move a quote of ours as Mastodon has them.
#[tokio::test]
async fn test_remote_quotes_wait_for_their_stamp() {
    let ctx = TestContext::new("quote-remote-verify").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    give_key(&ctx, alice_id).await;
    let alice_uri = format!("https://{}/users/alice", ctx.domain);
    let (carol_id, carol_uri, carol_key) =
        seed_remote_with_key(&ctx, "carol", "carol.invalid").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "quote me", "public")
        .await;
    let original_id: i64 = original["id"].as_str().unwrap().parse().unwrap();
    let original_uri: String = sqlx::query_scalar("SELECT uri FROM statuses WHERE id = $1")
        .bind(original_id)
        .fetch_one(&ctx.db)
        .await
        .unwrap();

    let create = |n: u32, quote: Value, extra: Value| {
        let mut note = json!({
            "id": format!("{carol_uri}/statuses/{n}"),
            "type": "Note",
            "attributedTo": carol_uri,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": [alice_uri],
            "content": "carol quotes",
            "quote": quote,
        });
        for (k, v) in extra.as_object().unwrap() {
            note[k] = v.clone();
        }
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{carol_uri}/statuses/{n}/activity"),
            "type": "Create",
            "actor": carol_uri,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": [alice_uri],
            "object": note,
        })
    };
    let post = |activity: Value| {
        let api = &ctx.api;
        let key = carol_key.clone();
        let key_id = format!("{carol_uri}#main-key");
        async move {
            let resp = api.post_signed("/inbox", &activity, &key_id, &key).await;
            assert!(resp.status().is_success(), "{}", resp.status());
        }
    };
    let status_of = |n: u32| {
        let db = ctx.db.clone();
        let uri = format!("{carol_uri}/statuses/{n}");
        async move {
            sqlx::query_scalar::<_, i64>("SELECT id FROM statuses WHERE uri = $1")
                .bind(uri)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };

    // A quote of alice's post: pending, uncounted, unannounced.
    post(create(1, json!(original_uri), json!({}))).await;
    let quoting = status_of(1).await;
    assert_eq!(quote_state(&ctx, quoting).await, 0);
    assert_eq!(quotes_count(&ctx, original_id).await, 0);
    let notified: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM notifications WHERE account_id = $1 AND "type" = 'quote'"#,
    )
    .bind(alice_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(notified, 0);
    let api_status: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{quoting}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(api_status["quote"]["state"], "pending");

    // A QuoteRequest named on another host than its sender is not carol's
    // (whether it is turned away at the door or ignored inside).
    let request = |id: String| {
        json!({
            "@context": ["https://www.w3.org/ns/activitystreams", {"QuoteRequest": "https://w3id.org/fep/044f#QuoteRequest"}],
            "id": id,
            "type": "QuoteRequest",
            "actor": carol_uri,
            "object": original_uri,
            "instrument": format!("{carol_uri}/statuses/1"),
        })
    };
    ctx.api
        .post_signed(
            "/inbox",
            &request("https://elsewhere.invalid/quote_requests/1".into()),
            &format!("{carol_uri}#main-key"),
            &carol_key,
        )
        .await;
    assert_eq!(quote_state(&ctx, quoting).await, 0);

    // Carol asks: accepted, counted, announced, answered at her own inbox.
    post(request(format!("{carol_uri}/quote_requests/1"))).await;
    assert_eq!(quote_state(&ctx, quoting).await, 1);
    assert_eq!(quotes_count(&ctx, original_id).await, 1);
    let accepts = queued_for(&ctx, "Accept", &format!("{carol_uri}/inbox")).await;
    assert_eq!(accepts.len(), 1);
    assert_eq!(accepts[0]["object"]["type"], "QuoteRequest");
    assert_eq!(
        accepts[0]["object"]["id"].as_str(),
        Some(format!("{carol_uri}/quote_requests/1").as_str())
    );
    let notified: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM notifications WHERE account_id = $1 AND "type" = 'quote' AND activity_type = 'Quote'"#,
    )
    .bind(alice_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(notified, 1);

    // A self-quote is accepted at once (`fast_track_approval!`).
    let carols_own =
        seed_remote_status(&ctx.db, carol_id, &format!("{carol_uri}/statuses/100")).await;
    post(create(
        2,
        json!(format!("{carol_uri}/statuses/100")),
        json!({}),
    ))
    .await;
    assert_eq!(quote_state(&ctx, status_of(2).await).await, 1);
    assert_eq!(quotes_count(&ctx, carols_own).await, 1);

    // A quote of a post that is gone.
    post(create(3, json!({"type": "Tombstone"}), json!({}))).await;
    let api_status: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{}", status_of(3).await),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(api_status["quote"]["state"], "deleted");

    // A quote of someone else's post whose stamp cannot be fetched now.
    let (dave_id, dave_uri, dave_key) = seed_remote_with_key(&ctx, "dave", "dave.invalid").await;
    seed_remote_status(&ctx.db, dave_id, &format!("{dave_uri}/statuses/7")).await;
    post(create(
        4,
        json!(format!("{dave_uri}/statuses/7")),
        json!({"quoteAuthorization": format!("{dave_uri}/quote_authorizations/9")}),
    ))
    .await;
    assert_eq!(quote_state(&ctx, status_of(4).await).await, 0);

    // ── Answers to a QuoteRequest of ours ──────────────────────────────────────
    let dave_post: i64 = sqlx::query_scalar("SELECT id FROM statuses WHERE uri = $1")
        .bind(format!("{dave_uri}/statuses/7"))
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    let ours: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "alice quotes dave", "quoted_status_id": dave_post.to_string(), "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let ours: i64 = ours["id"].as_str().unwrap().parse().unwrap();
    let request_id: String =
        sqlx::query_scalar("SELECT activity_uri FROM quotes WHERE status_id = $1")
            .bind(ours)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(request_id.starts_with(&format!("{alice_uri}/quote_requests/")));
    // Asked at dave's own inbox.
    let requests = queued_for(&ctx, "QuoteRequest", &format!("{dave_uri}/inbox")).await;
    assert_eq!(requests.len(), 1);

    let answer = |kind: &str, result: Option<String>| {
        let mut activity = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{dave_uri}#{kind}/{}", eunha::snowflake::next_id()),
            "type": kind,
            "actor": dave_uri,
            "object": request_id,
        });
        if let Some(result) = result {
            activity["result"] = json!(result);
        }
        let api = &ctx.api;
        let key = dave_key.clone();
        let key_id = format!("{dave_uri}#main-key");
        async move {
            let resp = api.post_signed("/inbox", &activity, &key_id, &key).await;
            assert!(resp.status().is_success(), "{}", resp.status());
        }
    };
    // A stamp on another host than dave's is not his to give.
    answer("Accept", Some("https://elsewhere.invalid/stamps/1".into())).await;
    assert_eq!(quote_state(&ctx, ours).await, 0);
    answer("Accept", Some(format!("{dave_uri}/quote_authorizations/1"))).await;
    assert_eq!(quote_state(&ctx, ours).await, 1);
    assert_eq!(quotes_count(&ctx, dave_post).await, 1);
    // A Reject after all: an accepted quote is revoked, and its stamp
    // forgotten.
    answer("Reject", None).await;
    assert_eq!(quote_state(&ctx, ours).await, 3);
    assert_eq!(quotes_count(&ctx, dave_post).await, 0);
    let approval: Option<String> =
        sqlx::query_scalar("SELECT approval_uri FROM quotes WHERE status_id = $1")
            .bind(ours)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(approval, None);
}

/// A QuoteRequest that may not quote is rejected, at the asker's own inbox,
/// under an id of its own.
#[tokio::test]
async fn test_a_quote_request_that_may_not_quote_is_rejected() {
    let ctx = TestContext::new("quote-request-reject").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    give_key(&ctx, alice_id).await;
    let (_, carol_uri, carol_key) = seed_remote_with_key(&ctx, "carol", "carol.invalid").await;
    let post: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "nobody may quote", "visibility": "public", "quote_approval_policy": "nobody"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let post_uri: String = sqlx::query_scalar("SELECT uri FROM statuses WHERE id = $1")
        .bind(post["id"].as_str().unwrap().parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    let request = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{carol_uri}/quote_requests/1"),
        "type": "QuoteRequest",
        "actor": carol_uri,
        "object": post_uri,
        "instrument": format!("{carol_uri}/statuses/1"),
    });
    let resp = ctx
        .api
        .post_signed(
            "/inbox",
            &request,
            &format!("{carol_uri}#main-key"),
            &carol_key,
        )
        .await;
    assert!(resp.status().is_success());
    let rejects = queued_for(&ctx, "Reject", &format!("{carol_uri}/inbox")).await;
    assert_eq!(rejects.len(), 1);
    let reject = &rejects[0];
    let prefix = format!("https://{}/users/alice#rejects/quote_requests/", ctx.domain);
    let id = reject["id"].as_str().unwrap();
    assert!(id.starts_with(&prefix) && id.len() > prefix.len(), "{id}");
    assert_eq!(reject["object"]["type"], "QuoteRequest");
    assert_eq!(reject["object"]["id"], request["id"]);
    assert_eq!(reject["object"]["object"].as_str(), Some(post_uri.as_str()));
}
