//! The `tootctl accounts` commands beyond `create` and `modify`: what each
//! does to the database and what it says, as Mastodon's do.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use eunha::tootctl::{accounts, Recorder};

use crate::helpers::{seed_user, TestContext};

/// A remote account, known here as Mastodon knows one.
async fn remote(ctx: &TestContext, username: &str, domain: &str, key: &str) -> i64 {
    remote_at(
        ctx,
        username,
        domain,
        &format!("https://{domain}/users/{username}"),
        key,
    )
    .await
}

async fn remote_at(ctx: &TestContext, username: &str, domain: &str, uri: &str, key: &str) -> i64 {
    sqlx::query_scalar(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key,
                                 inbox_url, outbox_url, shared_inbox_url, protocol,
                                 actor_type, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, $5, $4 || '/inbox', $4 || '/outbox', '', 1,
                   'Person', now(), now())
           RETURNING id"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(username)
    .bind(domain)
    .bind(uri)
    .bind(key)
    .fetch_one(&ctx.db)
    .await
    .unwrap()
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

async fn count(ctx: &TestContext, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&ctx.db).await.unwrap()
}

fn ids(ctx: &TestContext) -> (i64, i64) {
    (ctx.alice_id.parse().unwrap(), ctx.bob_id.parse().unwrap())
}

/// Polls `query` until it returns something.
async fn eventually<T>(mut query: impl AsyncFnMut() -> Option<T>) -> T {
    for _ in 0..100 {
        if let Some(found) = query().await {
            return found;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("it never happened");
}

/// A rotated account has a new key in `keypairs`, and the servers that
/// know it are sent its profile signed with the key they still hold.
#[tokio::test]
async fn test_rotate_replaces_the_key_and_signs_with_the_old_one() {
    let ctx = TestContext::new("cli-rotate").await;
    let (alice, _) = ids(&ctx);
    let (old_private, old_public) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    sqlx::query("UPDATE accounts SET private_key = $2, public_key = $3 WHERE id = $1")
        .bind(alice)
        .bind(&old_private)
        .bind(&old_public)
        .execute(&ctx.db)
        .await
        .unwrap();
    let nina = remote(&ctx, "nina", "nina.invalid", "").await;
    follow(&ctx, nina, alice).await;

    let console = Recorder::default();
    accounts::rotate(&ctx.state, &console, Some("alice".into()), false)
        .await
        .unwrap();
    assert_eq!(console.lines(), ["OK"]);

    let new_public: String = sqlx::query_scalar(
        "SELECT public_key FROM keypairs WHERE account_id = $1 AND local_fragment = '#main-key' AND type = 0",
    )
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_ne!(new_public, old_public);
    let legacy: (Option<String>, String) =
        sqlx::query_as("SELECT private_key, public_key FROM accounts WHERE id = $1")
            .bind(alice)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(
        legacy,
        (None, String::new()),
        "no second copy is left behind"
    );

    let update: Value = eventually(async || {
        sqlx::query_scalar(
            "SELECT payload->'activity' FROM eunha.ojak_queue
             WHERE payload->'activity'->>'type' = 'Update' AND payload->>'inbox' LIKE '%nina%'",
        )
        .fetch_optional(&ctx.db)
        .await
        .unwrap()
    })
    .await;
    assert_eq!(update["object"]["publicKey"]["publicKeyPem"], new_public);
    let verify = |key: &str| {
        ojak::sig::linked_data::verify(
            &ojak_jsonld::Registry::bundled(),
            &update,
            key,
            chrono::Utc::now().timestamp(),
        )
    };
    assert!(
        verify(&old_public).is_ok(),
        "signed with the key nina holds"
    );
    assert!(verify(&new_public).is_err());

    // Every local account but the instance actor, whose key a running server
    // holds for its life; and an account's other keys stay.
    let (_, actor_key) = eunha::federation::instance_actor::get_or_create(&ctx.state)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO keypairs (account_id, type, local_fragment, public_key, private_key,
                               created_at, updated_at)
         VALUES ($1, 1, '#ed25519-key', 'ED25519', 'sealed', now(), now())",
    )
    .bind(alice)
    .execute(&ctx.db)
    .await
    .unwrap();
    let console = Recorder::default();
    accounts::rotate(&ctx.state, &console, None, true)
        .await
        .unwrap();
    assert_eq!(console.lines(), ["OK, rotated keys for 2 accounts"]);
    assert_eq!(
        eunha::federation::instance_actor::get_or_create(&ctx.state)
            .await
            .unwrap()
            .1,
        actor_key
    );
    assert_eq!(
        count(
            &ctx,
            &format!("SELECT count(*) FROM keypairs WHERE account_id = {alice} AND type = 1")
        )
        .await,
        1
    );
    let error = accounts::rotate(&ctx.state, &console, Some(ctx.domain.clone()), false)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "No such account", "the instance actor");
    let error = accounts::rotate(&ctx.state, &console, Some("nobody".into()), false)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "No such account");
    let error = accounts::rotate(&ctx.state, &console, None, false)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "No account(s) given");
}

/// `delete` by username or address, with what it refuses; a dry run says
/// the same and deletes nothing.
#[tokio::test]
async fn test_delete_removes_the_user_and_keeps_the_username() {
    let ctx = TestContext::new("cli-delete").await;
    let (alice, bob) = ids(&ctx);

    let console = Recorder::default();
    accounts::delete(&ctx.state, &console, Some("alice".into()), None, true)
        .await
        .unwrap();
    assert_eq!(
        console.lines(),
        [
            "Deleting user with 0 statuses, this might take a while... (DRY RUN)",
            "OK (DRY RUN)"
        ]
    );
    assert_eq!(
        count(
            &ctx,
            &format!("SELECT count(*) FROM users WHERE account_id = {alice}")
        )
        .await,
        1
    );

    let console = Recorder::default();
    accounts::delete(
        &ctx.state,
        &console,
        None,
        Some("bob@test.invalid".into()),
        false,
    )
    .await
    .unwrap();
    assert_eq!(console.lines().last().unwrap(), "OK");
    assert_eq!(
        count(
            &ctx,
            &format!("SELECT count(*) FROM users WHERE account_id = {bob}")
        )
        .await,
        0,
        "the user goes, with its address"
    );
    assert_eq!(
        count(
            &ctx,
            &format!("SELECT count(*) FROM accounts WHERE id = {bob} AND requested_deletion_at IS NOT NULL")
        )
        .await,
        1,
        "the account stays, holding the username"
    );

    for (username, email, refusal) in [
        (
            Some("alice"),
            Some("alice@test.invalid"),
            "Use username or --email, not both",
        ),
        (None, None, "No username provided"),
        (Some("nobody"), None, "No user with such username"),
        (None, Some("nobody@test.invalid"), "No user with such email"),
    ] {
        let error = accounts::delete(
            &ctx.state,
            &console,
            username.map(Into::into),
            email.map(Into::into),
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), refusal);
    }
}

/// `approve` by number, oldest first, by username, and all at once.
#[tokio::test]
async fn test_approve_pending_accounts() {
    let ctx = TestContext::new("cli-approve").await;
    let mut pending = Vec::new();
    for (n, name) in ["carol", "dave", "erin", "fay"].into_iter().enumerate() {
        let (id, _) = seed_user(&ctx.db, &ctx.domain, name, &format!("{name}@test.invalid")).await;
        sqlx::query(
            "UPDATE users SET approved = false, created_at = now() - make_interval(days => $2)
             WHERE account_id = $1",
        )
        .bind(id)
        .bind(10 - n as i32)
        .execute(&ctx.db)
        .await
        .unwrap();
        pending.push(id);
    }
    let approved = async || -> Vec<i64> {
        sqlx::query_scalar(
            "SELECT account_id FROM users WHERE approved AND account_id = ANY($1) ORDER BY account_id",
        )
        .bind(&pending)
        .fetch_all(&ctx.db)
        .await
        .unwrap()
    };
    let console = Recorder::default();

    accounts::approve(&ctx.state, &console, None, Some(1), false)
        .await
        .unwrap();
    assert_eq!(approved().await, [pending[0]], "the oldest");
    accounts::approve(&ctx.state, &console, Some("erin".into()), None, false)
        .await
        .unwrap();
    assert_eq!(approved().await, [pending[0], pending[2]]);
    accounts::approve(&ctx.state, &console, None, None, true)
        .await
        .unwrap();
    assert_eq!(approved().await, pending);
    assert_eq!(console.lines(), ["OK", "OK", "OK"]);

    let error = accounts::approve(&ctx.state, &console, None, Some(-1), false)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Number must be positive");
    let error = accounts::approve(&ctx.state, &console, Some("nobody".into()), None, false)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "No such account");
}

/// `follow` has every local account follow one; `unfollow` has its local
/// followers stop; `reset-relationships` clears an account's follows both
/// ways.
#[tokio::test]
async fn test_follow_unfollow_and_reset_relationships() {
    let ctx = TestContext::new("cli-follow").await;
    let (alice, bob) = ids(&ctx);
    let (carol, _) = seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    // Local, and not one of the accounts that follow.
    eunha::federation::instance_actor::get_or_create(&ctx.state)
        .await
        .unwrap();

    let console = Recorder::default();
    accounts::follow(&ctx.state, &console, "alice", 2, false)
        .await
        .unwrap();
    let lines = console.lines();
    assert_eq!(lines.last().unwrap(), "OK, followed target from 3 accounts");
    // Alice cannot follow herself, which is reported and counted.
    assert!(lines.contains(&format!("Error processing {alice}: NotFound")));
    let followers = format!("SELECT count(*) FROM follows WHERE target_account_id = {alice}");
    assert_eq!(count(&ctx, &followers).await, 2);

    let console = Recorder::default();
    accounts::unfollow(&ctx.state, &console, "alice", 5, false)
        .await
        .unwrap();
    assert_eq!(console.lines(), ["OK, unfollowed target from 2 accounts"]);
    assert_eq!(count(&ctx, &followers).await, 0);

    follow(&ctx, bob, alice).await;
    follow(&ctx, carol, alice).await;
    follow(&ctx, alice, bob).await;
    let error = accounts::reset_relationships(&ctx.state, &console, "alice", false, false)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Please specify either --follows or --followers, or both"
    );
    let console = Recorder::default();
    accounts::reset_relationships(&ctx.state, &console, "alice", true, true)
        .await
        .unwrap();
    assert_eq!(console.lines(), ["Processed 3 relationships"]);
    assert_eq!(
        count(
            &ctx,
            &format!(
                "SELECT count(*) FROM follows WHERE account_id = {alice} OR target_account_id = {alice}"
            )
        )
        .await,
        0
    );
}

/// `backup` queues an archive for the running server to build.
#[tokio::test]
async fn test_backup_requests_an_archive() {
    let ctx = TestContext::new("cli-backup").await;
    let console = Recorder::default();
    accounts::backup(&ctx.state, &console, "alice")
        .await
        .unwrap();
    assert_eq!(console.lines(), ["OK"]);
    assert_eq!(
        count(
            &ctx,
            &format!(
                "SELECT count(*) FROM backups b JOIN users u ON u.id = b.user_id
                 WHERE u.account_id = {}",
                ctx.alice_id
            )
        )
        .await,
        1
    );
    let error = accounts::backup(&ctx.state, &console, "nobody")
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "No user with such username");
}

/// `prune` destroys the remote accounts nothing here refers to, and leaves
/// bots, groups and moderated accounts.
#[tokio::test]
async fn test_prune_removes_remote_accounts_nobody_here_knows() {
    let ctx = TestContext::new("cli-prune").await;
    let (alice, _) = ids(&ctx);
    let domain = "prune.invalid";
    let lonely = remote(&ctx, "lonely", domain, "").await;
    let followed = remote(&ctx, "followed", domain, "").await;
    follow(&ctx, alice, followed).await;
    let group = remote(&ctx, "group", domain, "").await;
    let silenced = remote(&ctx, "silenced", domain, "").await;
    let service = remote(&ctx, "service", domain, "").await;
    sqlx::query("UPDATE accounts SET actor_type = 'Group' WHERE id = $1")
        .bind(group)
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET silenced_at = now() WHERE id = $1")
        .bind(silenced)
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET actor_type = 'Service' WHERE id = $1")
        .bind(service)
        .execute(&ctx.db)
        .await
        .unwrap();
    let left = async || -> Vec<i64> {
        sqlx::query_scalar("SELECT id FROM accounts WHERE domain = 'prune.invalid' ORDER BY id")
            .fetch_all(&ctx.db)
            .await
            .unwrap()
    };

    let console = Recorder::default();
    accounts::prune(&ctx.state, &console, 2, true)
        .await
        .unwrap();
    assert_eq!(console.lines(), ["OK, pruned 1 accounts (DRY RUN)"]);
    assert_eq!(left().await.len(), 5);

    let console = Recorder::default();
    accounts::prune(&ctx.state, &console, 2, false)
        .await
        .unwrap();
    assert_eq!(console.lines(), ["OK, pruned 1 accounts"]);
    let mut kept = vec![followed, group, silenced, service];
    kept.sort();
    assert_eq!(left().await, kept);
    assert!(!left().await.contains(&lonely));
}

/// `merge` gives one remote account what another had, when they are one
/// actor by their key, and refuses two that are not unless forced.
#[tokio::test]
async fn test_merge_joins_two_rows_for_one_actor() {
    let ctx = TestContext::new("cli-merge").await;
    let (alice, _) = ids(&ctx);
    let to = remote(&ctx, "zed", "new.invalid", "KEY").await;
    let from = remote(&ctx, "zed", "old.invalid", "KEY").await;
    // Since 4.7 a remote key lives in `keypairs`, and `public_key` is blank
    // for every remote account, so it tells none of them apart.
    let other = remote(&ctx, "yan", "old.invalid", "").await;
    sqlx::query(
        "INSERT INTO keypairs (account_id, type, uri, public_key, created_at, updated_at)
         VALUES ($1, 0, 'https://old.invalid/users/yan#main-key', 'OTHER', now(), now())",
    )
    .bind(other)
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query("UPDATE accounts SET public_key = '' WHERE id IN ($1, $2)")
        .bind(to)
        .bind(from)
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO keypairs (account_id, type, uri, public_key, created_at, updated_at)
         VALUES ($1, 0, 'https://new.invalid/users/zed#main-key', 'KEY', now(), now()),
                ($2, 0, 'https://old.invalid/users/zed#main-key', 'KEY', now(), now())",
    )
    .bind(to)
    .bind(from)
    .execute(&ctx.db)
    .await
    .unwrap();
    follow(&ctx, alice, from).await;

    let console = Recorder::default();
    accounts::merge(
        &ctx.state,
        &console,
        "zed@old.invalid",
        "zed@new.invalid",
        false,
    )
    .await
    .unwrap();
    assert_eq!(console.lines(), ["OK"]);
    assert_eq!(
        count(
            &ctx,
            &format!("SELECT count(*) FROM accounts WHERE id = {from}")
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &ctx,
            &format!("SELECT count(*) FROM follows WHERE account_id = {alice} AND target_account_id = {to}")
        )
        .await,
        1
    );

    let error = accounts::merge(
        &ctx.state,
        &console,
        "yan@old.invalid",
        "zed@new.invalid",
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Accounts don't have the same public key, might not be duplicates!\nOverride with --force"
    );
    accounts::merge(
        &ctx.state,
        &console,
        "yan@old.invalid",
        "zed@new.invalid",
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        count(
            &ctx,
            &format!("SELECT count(*) FROM accounts WHERE id = {other}")
        )
        .await,
        0
    );
    let error = accounts::merge(&ctx.state, &console, "alice", "zed@new.invalid", false)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "No such account (alice)");

    let console = Recorder::default();
    accounts::fix_duplicates(&console);
    assert_eq!(
        console.lines(),
        ["This command is deprecated as Mastodon v4.7.0 migrations enforce ActivityPub actor identifier uniqueness"]
    );
}

/// A remote server: what each path answers.
#[derive(Clone, Default)]
struct Remote {
    answers: Arc<Mutex<HashMap<String, (u16, Value)>>>,
}

impl Remote {
    fn put(&self, path: &str, status: u16, body: Value) {
        self.answers
            .lock()
            .unwrap()
            .insert(path.to_owned(), (status, body));
    }
}

async fn spawn_remote() -> (Remote, String) {
    use axum::response::IntoResponse;
    let remote = Remote::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = listener.local_addr().unwrap().to_string();
    let app =
        axum::Router::new()
            .fallback(
                |axum::extract::State(remote): axum::extract::State<Remote>,
                 uri: axum::http::Uri| async move {
                    match remote.answers.lock().unwrap().get(uri.path()) {
                        Some((status, body)) => (
                            reqwest::StatusCode::from_u16(*status).unwrap(),
                            [("content-type", "application/activity+json")],
                            body.to_string(),
                        )
                            .into_response(),
                        None => reqwest::StatusCode::NOT_FOUND.into_response(),
                    }
                },
            )
            .with_state(remote.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (remote, host)
}

/// `cull` removes the accounts whose servers say they are gone, leaves
/// those seen this week, and names the servers it could not reach.
#[tokio::test]
async fn test_cull_removes_accounts_their_servers_say_are_gone() {
    let ctx = TestContext::reaching_loopback("cli-cull").await;
    let (remote_server, host) = spawn_remote().await;
    remote_server.put("/users/here", 200, json!({}));
    remote_server.put("/users/gone", 410, json!({}));
    let base = format!("http://{host}");
    let here = remote_at(&ctx, "here", &host, &format!("{base}/users/here"), "").await;
    let gone = remote_at(&ctx, "gone", &host, &format!("{base}/users/gone"), "").await;
    let recent = remote_at(&ctx, "recent", &host, &format!("{base}/users/recent"), "").await;
    // A port nothing listens on.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().to_string()
    };
    let away = remote_at(
        &ctx,
        "away",
        &closed,
        &format!("http://{closed}/users/away"),
        "",
    )
    .await;
    sqlx::query("UPDATE accounts SET updated_at = now() - interval '8 days' WHERE id = ANY($1)")
        .bind(vec![here, gone, away])
        .execute(&ctx.db)
        .await
        .unwrap();

    let console = Recorder::default();
    accounts::cull(&ctx.state, &console, &[], 2, true)
        .await
        .unwrap();
    assert_eq!(
        console.lines(),
        [
            "Visited 4 accounts, removed 1 (DRY RUN)".to_owned(),
            "The following domains were not available during the check:".to_owned(),
            format!("  {closed}"),
        ]
    );
    let exists = async |id: i64| {
        count(
            &ctx,
            &format!("SELECT count(*) FROM accounts WHERE id = {id}"),
        )
        .await
            == 1
    };
    assert!(exists(gone).await);

    // What was there, and what could not be asked, was touched even in the
    // dry run, so only the gone account is visited again.
    let console = Recorder::default();
    accounts::cull(&ctx.state, &console, std::slice::from_ref(&host), 2, false)
        .await
        .unwrap();
    assert_eq!(console.lines(), ["Visited 3 accounts, removed 1"]);
    assert!(!exists(gone).await);
    assert!(exists(here).await && exists(recent).await && exists(away).await);
}
