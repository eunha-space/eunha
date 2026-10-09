//! Remote accounts changing handles (Mastodon 4.7.0).
//!
//! An actor's `id` is the account's identity, so a rename is a rename rather
//! than a second account — but only once webfinger agrees that the new handle
//! belongs to the same actor.

use serde_json::json;

use crate::helpers::TestContext;

async fn seed_remote(ctx: &TestContext, username: &str, domain: &str) -> (i64, String, String) {
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    let uri = format!("https://{domain}/users/{username}");
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, protocol, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, $5, $4 || '/inbox', $4 || '/outbox', 1, now(), now())"#,
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

/// A handle nobody can verify is not adopted: an actor document may claim any
/// `preferredUsername`, and taking its word for it would let one account seize
/// another's handle. As in Mastodon, whose `check_webfinger!` raises, the rest
/// of the update is not applied either: it is the same document.
#[tokio::test]
async fn test_unverifiable_handle_change_is_ignored() {
    let ctx = TestContext::new("handle-unverified").await;
    let (remote_id, remote_uri, priv_pem) = seed_remote(&ctx, "erin", "remote.invalid").await;

    let update = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{remote_uri}#update-1"),
        "type": "Update",
        "actor": remote_uri,
        "object": {
            "id": remote_uri,
            "type": "Person",
            "preferredUsername": "erin_renamed",
            "name": "Erin, renamed",
            "inbox": format!("{remote_uri}/inbox"),
        },
    });

    let resp = ctx
        .api
        .post_signed(
            "/inbox",
            &update,
            &format!("{remote_uri}#main-key"),
            &priv_pem,
        )
        .await;
    assert!(
        resp.status().is_success(),
        "Update(Person) should be accepted, got {}",
        resp.status()
    );

    let account = sqlx::query!(
        "SELECT username, display_name FROM accounts WHERE id = $1",
        remote_id,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        account.username, "erin",
        "an unverifiable handle change must not rename the account"
    );
    assert_eq!(
        account.display_name, "erin",
        "an update whose handle cannot be verified is dropped whole"
    );
}

/// An account whose handle has not changed is left alone, and no webfinger
/// lookup is needed to decide that.
#[tokio::test]
async fn test_unchanged_handle_is_left_alone() {
    let ctx = TestContext::new("handle-unchanged").await;
    let (remote_id, remote_uri, priv_pem) = seed_remote(&ctx, "frank", "remote.invalid").await;

    let update = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{remote_uri}#update-1"),
        "type": "Update",
        "actor": remote_uri,
        "object": {
            "id": remote_uri,
            "type": "Person",
            "preferredUsername": "frank",
            "name": "Frank",
            "inbox": format!("{remote_uri}/inbox"),
        },
    });

    ctx.api
        .post_signed(
            "/inbox",
            &update,
            &format!("{remote_uri}#main-key"),
            &priv_pem,
        )
        .await;

    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
        .bind(remote_id)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(username, "frank");
}

/// The handle goes to whichever actor webfinger points at; whoever was holding
/// it keeps its actor id and everything hanging off it, but its handle becomes
/// one no server could issue (Mastodon's `invalidate_username!`).
#[tokio::test]
async fn test_conflicting_handle_is_taken_from_its_old_holder() {
    let ctx = TestContext::new("handle-conflict").await;
    let (old_holder, _, _) = seed_remote(&ctx, "gina", "remote.invalid").await;
    // A second actor that will claim the same handle.
    let (claimant, _, _) = seed_remote(&ctx, "gina2", "remote.invalid").await;

    eunha::federation::handle::invalidate_conflicting_handle(
        &ctx.state,
        claimant,
        "gina",
        "remote.invalid",
    )
    .await
    .unwrap();

    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
        .bind(old_holder)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(username, format!("! {old_holder}"));

    // The handle is now free for the actor that owns it.
    sqlx::query("UPDATE accounts SET username = 'gina' WHERE id = $1")
        .bind(claimant)
        .execute(&ctx.db)
        .await
        .expect("the freed handle should be available");
}

/// The claimant's own row is never the one invalidated, and neither is a local
/// account: a remote handle cannot collide with one.
#[tokio::test]
async fn test_invalidation_leaves_the_claimant_and_local_accounts_alone() {
    let ctx = TestContext::new("handle-conflict-self").await;
    let (claimant, _, _) = seed_remote(&ctx, "hana", "remote.invalid").await;

    eunha::federation::handle::invalidate_conflicting_handle(
        &ctx.state,
        claimant,
        "hana",
        "remote.invalid",
    )
    .await
    .unwrap();
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
        .bind(claimant)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(username, "hana");

    // `alice` is local and holds that handle on this domain; a remote actor
    // claiming it must not disturb her.
    eunha::federation::handle::invalidate_conflicting_handle(
        &ctx.state,
        claimant,
        "alice",
        &ctx.domain,
    )
    .await
    .unwrap();
    let alice: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(alice, "alice");
}

/// What clients see for an account whose handle could not be verified: the
/// `invalid_handle` flag, and neither the claimed handle nor its domain.
#[tokio::test]
async fn test_invalid_handle_is_reported_to_clients() {
    let ctx = TestContext::new("handle-invalid-api").await;
    let (account_id, _, _) = seed_remote(&ctx, "ivan", "remote.invalid").await;
    sqlx::query("UPDATE accounts SET username = '! ' || id::text WHERE id = $1")
        .bind(account_id)
        .execute(&ctx.db)
        .await
        .unwrap();

    let account: serde_json::Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{account_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert_eq!(account["invalid_handle"].as_bool(), Some(true));
    assert_eq!(
        account["username"].as_str(),
        Some(account_id.to_string().as_str())
    );
    assert_eq!(
        account["acct"].as_str(),
        Some(format!("{account_id}@handle.invalid").as_str())
    );
}

/// An ordinary account says nothing about `invalid_handle` at all, matching
/// Mastodon, which only serializes the attribute when it is true.
#[tokio::test]
async fn test_valid_handle_omits_the_attribute() {
    let ctx = TestContext::new("handle-valid-api").await;
    let (account_id, _, _) = seed_remote(&ctx, "jack", "remote.invalid").await;

    let account: serde_json::Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{account_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(account.get("invalid_handle").is_none());
    assert_eq!(account["acct"].as_str(), Some("jack@remote.invalid"));
}
