//! Account moves, redirects and aliases over REST, which Mastodon serves as
//! settings forms (`AccountMigration`, `Form::Redirect`, `AccountAlias`) and
//! eunha as `/api/v1/accounts/move`, `/api/v1/accounts/redirect` and
//! `/api/v1/profile/aliases`, answering with the same validation messages.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{seed_user, TestContext};

const PASSWORD: &str = "testpassword123";

fn actor_uri(ctx: &TestContext, username: &str) -> String {
    format!("https://{}/users/{username}", ctx.domain)
}

async fn also_known_as(ctx: &TestContext, account_id: &str) -> Vec<String> {
    sqlx::query_scalar::<_, Option<Vec<String>>>("SELECT also_known_as FROM accounts WHERE id = $1")
        .bind(account_id.parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap()
        .unwrap_or_default()
}

async fn follows(ctx: &TestContext, follower: i64, target: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)",
    )
    .bind(follower)
    .bind(target.parse::<i64>().unwrap())
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

async fn followers_count(ctx: &TestContext, account_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT followers_count FROM account_stats WHERE account_id = $1")
        .bind(account_id.parse::<i64>().unwrap())
        .fetch_optional(&ctx.db)
        .await
        .unwrap()
        .unwrap_or(0)
}

async fn note(ctx: &TestContext, author: i64, target: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT comment FROM account_notes WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(author)
    .bind(target.parse::<i64>().unwrap())
    .fetch_optional(&ctx.db)
    .await
    .unwrap()
}

async fn error_of(resp: reqwest::Response) -> String {
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    body["error"].as_str().unwrap_or_default().to_owned()
}

async fn add_alias(ctx: &TestContext, token: &str, acct: &str) -> reqwest::Response {
    ctx.api
        .post_json(
            "/api/v1/profile/aliases",
            Some(token),
            &json!({ "acct": acct }),
        )
        .await
}

async fn move_to(ctx: &TestContext, token: &str, acct: &str) -> reqwest::Response {
    ctx.api
        .post_json(
            "/api/v1/accounts/move",
            Some(token),
            &json!({ "acct": acct, "current_password": PASSWORD }),
        )
        .await
}

/// `AccountAlias` keeps the handle as typed, less a leading `@`, resolves it
/// to the account's actor id, and adds that id to `also_known_as`, which the
/// actor serves as `alsoKnownAs`. Removing the alias takes the id away again.
#[tokio::test]
async fn test_aliases_resolve_the_handle_and_keep_also_known_as() {
    let ctx = TestContext::new("alias-aka").await;
    let bob_uri = actor_uri(&ctx, "bob");

    let resp = add_alias(&ctx, &ctx.alice_token, " @bob").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let alias: Value = resp.json().await.unwrap();
    assert_eq!(alias["acct"], "bob");
    assert_eq!(alias["uri"], bob_uri.as_str());
    assert_eq!(
        also_known_as(&ctx, &ctx.alice_id).await,
        vec![bob_uri.clone()]
    );

    let actor: Value = ctx
        .api
        .ap_get("/users/alice", None)
        .await
        .json()
        .await
        .unwrap();
    // Served after the actors under the instance's previous domains.
    assert_eq!(actor["alsoKnownAs"][0], bob_uri.as_str());

    let list: Vec<Value> = ctx
        .api
        .get("/api/v1/profile/aliases", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 1);

    // The same account twice is refused, as `validates :uri, uniqueness:`.
    let again = add_alias(&ctx, &ctx.alice_token, "bob").await;
    assert_eq!(
        error_of(again).await,
        "Validation failed: Uri has already been taken"
    );

    let id = alias["id"].as_str().unwrap();
    let resp = ctx
        .api
        .delete(&format!("/api/v1/profile/aliases/{id}"), &ctx.alice_token)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(also_known_as(&ctx, &ctx.alice_id).await.is_empty());
    let resp = ctx
        .api
        .delete(&format!("/api/v1/profile/aliases/{id}"), &ctx.alice_token)
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// An alias that cannot be found, or that is the account itself, is refused
/// with `migrations.errors.*`.
#[tokio::test]
async fn test_alias_validation_errors() {
    let ctx = TestContext::new("alias-errors").await;

    let resp = add_alias(&ctx, &ctx.alice_token, "nobody").await;
    assert_eq!(
        error_of(resp).await,
        "Validation failed: Acct could not be found"
    );
    let resp = add_alias(&ctx, &ctx.alice_token, "alice").await;
    assert_eq!(
        error_of(resp).await,
        "Validation failed: Acct cannot be current account"
    );
    let resp = add_alias(&ctx, &ctx.alice_token, "").await;
    assert!(error_of(resp).await.contains("Acct can't be blank"));
    let resp = add_alias(&ctx, &ctx.alice_token, "x@exa_mple.invalid").await;
    assert!(error_of(resp)
        .await
        .contains("Acct is not a valid domain name"));

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM account_aliases")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

/// `AccountMigration`'s validations: the password, then a target that can be
/// found, lists this account as an alias, and is neither this account nor
/// the one it already moved to. A refused move writes nothing.
#[tokio::test]
async fn test_move_validation_errors() {
    let ctx = TestContext::new("move-errors").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/accounts/move",
            Some(&ctx.alice_token),
            &json!({ "acct": "bob", "current_password": "wrong" }),
        )
        .await;
    assert_eq!(
        error_of(resp).await,
        "Validation failed: Current password is invalid"
    );

    let resp = move_to(&ctx, &ctx.alice_token, "nobody").await;
    assert_eq!(
        error_of(resp).await,
        "Validation failed: Acct could not be found"
    );

    // Bob has not named alice as an alias.
    let resp = move_to(&ctx, &ctx.alice_token, "bob").await;
    assert_eq!(
        error_of(resp).await,
        "Validation failed: Acct is not an alias of this account"
    );

    let resp = move_to(&ctx, &ctx.alice_token, "@alice").await;
    assert!(error_of(resp)
        .await
        .contains("Acct cannot be current account"));

    let migrations: i64 = sqlx::query_scalar("SELECT count(*) FROM account_migrations")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(migrations, 0);
    let moved: Option<i64> =
        sqlx::query_scalar("SELECT moved_to_account_id FROM accounts WHERE id = $1")
            .bind(ctx.alice_id.parse::<i64>().unwrap())
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(moved, None);
}

/// An account without a password (signed up through SSO) confirms with its
/// username instead.
#[tokio::test]
async fn test_a_passwordless_account_confirms_with_its_username() {
    let ctx = TestContext::new("move-username").await;
    sqlx::query("UPDATE users SET encrypted_password = '' WHERE account_id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();

    let resp = ctx
        .api
        .post_json(
            "/api/v1/accounts/redirect",
            Some(&ctx.alice_token),
            &json!({ "acct": "bob", "current_username": "carol" }),
        )
        .await;
    assert_eq!(
        error_of(resp).await,
        "Validation failed: Current username is invalid"
    );

    let resp = ctx
        .api
        .post_json(
            "/api/v1/accounts/redirect",
            Some(&ctx.alice_token),
            &json!({ "acct": "bob", "current_username": "@alice" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// `Form::Redirect` sets `moved_to_account` without a migration: nobody is
/// moved, and the redirect can be cancelled.
#[tokio::test]
async fn test_a_redirect_moves_no_one_and_can_be_cancelled() {
    let ctx = TestContext::new("redirect").await;
    let (carol_id, carol_token) =
        seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    ctx.api.follow(&carol_token, &ctx.alice_id).await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/accounts/redirect",
            Some(&ctx.alice_token),
            &json!({ "acct": "bob", "current_password": PASSWORD }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let me: Value = resp.json().await.unwrap();
    assert_eq!(me["moved"]["id"], ctx.bob_id.as_str());
    assert!(follows(&ctx, carol_id, &ctx.alice_id).await);
    assert!(!follows(&ctx, carol_id, &ctx.bob_id).await);
    let migrations: i64 = sqlx::query_scalar("SELECT count(*) FROM account_migrations")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(migrations, 0);

    // Redirecting to the same account again is `already_moved`.
    let resp = ctx
        .api
        .post_json(
            "/api/v1/accounts/redirect",
            Some(&ctx.alice_token),
            &json!({ "acct": "bob", "current_password": PASSWORD }),
        )
        .await;
    assert_eq!(
        error_of(resp).await,
        "Validation failed: Acct is the same account you have already moved to"
    );

    let resp = ctx
        .api
        .delete("/api/v1/accounts/redirect", &ctx.alice_token)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let me: Value = resp.json().await.unwrap();
    assert!(me["moved"].is_null());
}

/// A move between two local accounts rewrites the follows themselves
/// (`MoveWorker#rewrite_follows!`): pending requests to the new account from
/// the old one's followers are approved, an account following both keeps
/// both and its lists gain the new account, and the rest have their follow
/// and list memberships pointed at the new account. Notes, blocks and mutes
/// of the old account carry over, with a note saying why. A second move
/// within thirty days is refused.
#[tokio::test]
async fn test_a_local_move_rewrites_follows_and_carries_relationships() {
    let ctx = TestContext::new("move-local").await;
    let (carol_id, carol_token) =
        seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let (dave_id, dave_token) = seed_user(&ctx.db, &ctx.domain, "dave", "dave@test.invalid").await;
    let (erin_id, erin_token) = seed_user(&ctx.db, &ctx.domain, "erin", "erin@test.invalid").await;
    let (frank_id, frank_token) =
        seed_user(&ctx.db, &ctx.domain, "frank", "frank@test.invalid").await;
    let (grace_id, grace_token) =
        seed_user(&ctx.db, &ctx.domain, "grace", "grace@test.invalid").await;
    sqlx::query("UPDATE users SET locale = 'ko' WHERE account_id = $1")
        .bind(grace_id)
        .execute(&ctx.db)
        .await
        .unwrap();

    // Bob names alice as his old account.
    assert_eq!(
        add_alias(&ctx, &ctx.bob_token, "alice").await.status(),
        StatusCode::OK
    );

    // Carol follows alice and has her on a list, with a note about her.
    ctx.api.follow(&carol_token, &ctx.alice_id).await;
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.alice_id),
            Some(&carol_token),
            &json!({ "reblogs": false, "notify": true }),
        )
        .await;
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&carol_token),
            &json!({"title": "old"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let carol_list = list["id"].as_str().unwrap().to_owned();
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/lists/{carol_list}/accounts"),
            Some(&carol_token),
            &json!({ "account_ids": [ctx.alice_id] }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/note", ctx.alice_id),
            Some(&carol_token),
            &json!({ "comment": "met at the meetup" }),
        )
        .await;

    // Dave follows both, with alice on a list.
    ctx.api.follow(&dave_token, &ctx.alice_id).await;
    ctx.api.follow(&dave_token, &ctx.bob_id).await;
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&dave_token),
            &json!({"title": "both"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let dave_list = list["id"].as_str().unwrap().to_owned();
    ctx.api
        .post_json(
            &format!("/api/v1/lists/{dave_list}/accounts"),
            Some(&dave_token),
            &json!({ "account_ids": [ctx.alice_id] }),
        )
        .await;

    // Erin follows alice and has asked to follow bob, who approves followers.
    ctx.api.follow(&erin_token, &ctx.alice_id).await;
    sqlx::query("UPDATE accounts SET locked = true WHERE id = $1")
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    ctx.api.follow(&erin_token, &ctx.bob_id).await;
    assert!(!follows(&ctx, erin_id, &ctx.bob_id).await);

    // Frank blocks alice; grace mutes her, notifications still on.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.alice_id),
            Some(&frank_token),
            &json!({}),
        )
        .await;
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/mute", ctx.alice_id),
            Some(&grace_token),
            &json!({ "notifications": false }),
        )
        .await;

    assert_eq!(followers_count(&ctx, &ctx.alice_id).await, 3);
    assert_eq!(followers_count(&ctx, &ctx.bob_id).await, 1);

    let resp = move_to(&ctx, &ctx.alice_token, "@bob").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let migration: Value = resp.json().await.unwrap();
    assert_eq!(migration["acct"], "bob");
    assert_eq!(migration["followers_count"], 3);
    assert_eq!(migration["target_account_id"], ctx.bob_id.as_str());

    let moved: Option<i64> =
        sqlx::query_scalar("SELECT moved_to_account_id FROM accounts WHERE id = $1")
            .bind(ctx.alice_id.parse::<i64>().unwrap())
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(moved, Some(ctx.bob_id.parse().unwrap()));

    // Carol's follow moved, settings and list membership with it.
    assert!(!follows(&ctx, carol_id, &ctx.alice_id).await);
    assert!(follows(&ctx, carol_id, &ctx.bob_id).await);
    let (reblogs, notify): (bool, bool) = sqlx::query_as(
        "SELECT show_reblogs, notify FROM follows WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(carol_id)
    .bind(ctx.bob_id.parse::<i64>().unwrap())
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!((reblogs, notify), (false, true));
    let members: Vec<i64> =
        sqlx::query_scalar("SELECT account_id FROM list_accounts WHERE list_id = $1")
            .bind(carol_list.parse::<i64>().unwrap())
            .fetch_all(&ctx.db)
            .await
            .unwrap();
    assert_eq!(members, vec![ctx.bob_id.parse::<i64>().unwrap()]);

    // Dave keeps both follows, and his list gains bob.
    assert!(follows(&ctx, dave_id, &ctx.alice_id).await);
    assert!(follows(&ctx, dave_id, &ctx.bob_id).await);
    let mut members: Vec<i64> =
        sqlx::query_scalar("SELECT account_id FROM list_accounts WHERE list_id = $1")
            .bind(dave_list.parse::<i64>().unwrap())
            .fetch_all(&ctx.db)
            .await
            .unwrap();
    members.sort();
    let mut expected = vec![
        ctx.alice_id.parse::<i64>().unwrap(),
        ctx.bob_id.parse::<i64>().unwrap(),
    ];
    expected.sort();
    assert_eq!(members, expected);

    // Erin's request was approved, so she follows both too.
    assert!(follows(&ctx, erin_id, &ctx.bob_id).await);
    assert!(follows(&ctx, erin_id, &ctx.alice_id).await);

    // Only carol's follow was rewritten; erin's approval counted itself.
    assert_eq!(followers_count(&ctx, &ctx.alice_id).await, 2);
    assert_eq!(followers_count(&ctx, &ctx.bob_id).await, 3);

    assert_eq!(
        note(&ctx, carol_id, &ctx.bob_id).await.as_deref(),
        Some("This user moved from alice, here were your previous notes about them:\nmet at the meetup")
    );

    let blocks: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = $2)",
    )
    .bind(frank_id)
    .bind(ctx.bob_id.parse::<i64>().unwrap())
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(blocks);
    assert_eq!(
        note(&ctx, frank_id, &ctx.bob_id).await.as_deref(),
        Some("This user moved from alice, which you had blocked.")
    );

    let hide: Option<bool> = sqlx::query_scalar(
        "SELECT hide_notifications FROM mutes WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(grace_id)
    .bind(ctx.bob_id.parse::<i64>().unwrap())
    .fetch_optional(&ctx.db)
    .await
    .unwrap();
    assert_eq!(hide, Some(false));
    assert_eq!(
        note(&ctx, grace_id, &ctx.bob_id).await.as_deref(),
        Some("이 사용자는 예전에 뮤트한 alice에서 이주 했습니다.")
    );

    // A second move within the cooldown is refused, whatever its target.
    let (_, _) = seed_user(&ctx.db, &ctx.domain, "heidi", "heidi@test.invalid").await;
    let resp = move_to(&ctx, &ctx.alice_token, "heidi").await;
    let error = error_of(resp).await;
    assert!(error.contains("You are on cooldown"), "{error}");
}

/// The `Move` a migration sends is `ActivityPub::MoveSerializer`'s, with
/// the migration's id in its own, and goes to the old account's remote
/// followers and to the remote accounts that block it.
#[tokio::test]
async fn test_a_move_is_sent_to_followers_and_blockers() {
    let ctx = TestContext::new("move-delivery").await;
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    sqlx::query(
        "UPDATE accounts SET private_key = $1, public_key = $2 WHERE username = 'alice' AND domain IS NULL",
    )
    .bind(&priv_pem)
    .bind(&pub_pem)
    .execute(&ctx.db)
    .await
    .unwrap();
    let alice_id: i64 = ctx.alice_id.parse().unwrap();

    let mut inboxes = Vec::new();
    for (username, relation) in [("nina", "follows"), ("rob", "blocks")] {
        let id = eunha::snowflake::next_id();
        let uri = format!("https://{username}.invalid/users/{username}");
        sqlx::query(
            r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, inbox_url, outbox_url, protocol, created_at, updated_at)
               VALUES ($1, $2, $3, $2, '', $4, $4, $4 || '/inbox', $4 || '/outbox', 1, now(), now())"#,
        )
        .bind(id)
        .bind(username)
        .bind(format!("{username}.invalid"))
        .bind(&uri)
        .execute(&ctx.db)
        .await
        .unwrap();
        sqlx::query(&format!(
            "INSERT INTO {relation} (account_id, target_account_id, created_at, updated_at)
             VALUES ($1, $2, now(), now())"
        ))
        .bind(id)
        .bind(alice_id)
        .execute(&ctx.db)
        .await
        .unwrap();
        inboxes.push(format!("{uri}/inbox"));
    }

    add_alias(&ctx, &ctx.bob_token, "alice").await;
    let resp = move_to(&ctx, &ctx.alice_token, "bob").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let migration: Value = resp.json().await.unwrap();
    let alice_uri = actor_uri(&ctx, "alice");

    for inbox in inboxes {
        let queued: Vec<Value> = sqlx::query_scalar(
            "SELECT payload->'activity' FROM eunha.ojak_queue
             WHERE payload->>'inbox' = $1 AND payload->'activity'->>'type' = 'Move'",
        )
        .bind(&inbox)
        .fetch_all(&ctx.db)
        .await
        .unwrap();
        assert_eq!(queued.len(), 1, "a Move for {inbox}");
        let activity = &queued[0];
        assert_eq!(
            activity["id"],
            format!("{alice_uri}#moves/{}", migration["id"].as_str().unwrap())
        );
        assert_eq!(activity["actor"], alice_uri.as_str());
        assert_eq!(activity["object"], alice_uri.as_str());
        assert_eq!(activity["target"], actor_uri(&ctx, "bob"));
    }
}
