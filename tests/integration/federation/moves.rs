//! An inbound `Move` (`ActivityPub::Activity::Move`): an account on another
//! server says it has moved, here to a local account. It is believed only
//! about the sender itself and when the target names the sender in
//! `alsoKnownAs`; then local followers follow the new account
//! (`MoveWorker`), and local notes, blocks and mutes carry over. One Move
//! per account is processed a week.

use serde_json::{json, Value};

use crate::helpers::{seed_user, TestContext};

async fn seed_remote(ctx: &TestContext, username: &str) -> (i64, String, String) {
    let domain = format!("{username}.invalid");
    let (priv_pem, pub_pem) = eunha::crypto::generate_rsa_keypair().unwrap();
    let uri = format!("https://{domain}/users/{username}");
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, $5, $4 || '/inbox', $4 || '/outbox', now(), now())"#,
    )
    .bind(id)
    .bind(username)
    .bind(&domain)
    .bind(&uri)
    .bind(&pub_pem)
    .execute(&ctx.db)
    .await
    .unwrap();
    (id, uri, priv_pem)
}

fn move_activity(actor: &str, object: &str, target: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{actor}#moves/{}", eunha::snowflake::next_id()),
        "type": "Move",
        "actor": actor,
        "object": object,
        "target": target,
    })
}

async fn moved_to(ctx: &TestContext, id: i64) -> Option<i64> {
    sqlx::query_scalar("SELECT moved_to_account_id FROM accounts WHERE id = $1")
        .bind(id)
        .fetch_one(&ctx.db)
        .await
        .unwrap()
}

async fn exists(ctx: &TestContext, sql: &str, a: i64, b: i64) -> bool {
    sqlx::query_scalar::<_, bool>(&format!("SELECT EXISTS ({sql})"))
        .bind(a)
        .bind(b)
        .fetch_one(&ctx.db)
        .await
        .unwrap()
}

async fn in_progress_ttl(ctx: &TestContext, account_id: i64) -> i64 {
    let mut redis = ctx.state.redis.clone();
    redis::cmd("TTL")
        .arg(
            ctx.state
                .redis_keys
                .key(format!("move_in_progress:{account_id}")),
        )
        .query_async(&mut redis)
        .await
        .unwrap()
}

#[tokio::test]
async fn test_an_inbound_move_moves_local_followers_and_relationships() {
    let ctx = TestContext::new("move-inbound").await;
    let bob_id: i64 = ctx.bob_id.parse().unwrap();
    let bob_uri = format!("https://{}/users/bob", ctx.domain);
    let (olga_id, olga, olga_key) = seed_remote(&ctx, "olga").await;
    let key_id = format!("{olga}#main-key");
    let (carol_id, carol_token) =
        seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let (dave_id, dave_token) = seed_user(&ctx.db, &ctx.domain, "dave", "dave@test.invalid").await;
    let (grace_id, grace_token) =
        seed_user(&ctx.db, &ctx.domain, "grace", "grace@test.invalid").await;

    // Carol, who can sign, follows olga without reblogs, and keeps a note
    // on her.
    let (carol_private, carol_public) = eunha::crypto::generate_rsa_keypair().unwrap();
    sqlx::query("UPDATE accounts SET private_key = $1, public_key = $2 WHERE id = $3")
        .bind(&carol_private)
        .bind(&carol_public)
        .bind(carol_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, show_reblogs, created_at, updated_at)
         VALUES ($1, $2, false, now(), now())",
    )
    .bind(carol_id)
    .bind(olga_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{olga_id}/note"),
            Some(&carol_token),
            &json!({ "comment": "from the old server" }),
        )
        .await;
    // Dave blocks her; grace mutes her.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{olga_id}/block"),
            Some(&dave_token),
            &json!({}),
        )
        .await;
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{olga_id}/mute"),
            Some(&grace_token),
            &json!({}),
        )
        .await;
    // Bob approves followers, which a move to a local account bypasses.
    sqlx::query("UPDATE accounts SET locked = true WHERE id = $1")
        .bind(bob_id)
        .execute(&ctx.db)
        .await
        .unwrap();

    // A Move about someone other than its sender is ignored outright.
    let resp = ctx
        .api
        .post_signed(
            "/inbox",
            &move_activity(&olga, "https://elsewhere.invalid/users/x", &bob_uri),
            &key_id,
            &olga_key,
        )
        .await;
    assert!(resp.status().is_success(), "{}", resp.status());
    assert_eq!(in_progress_ttl(&ctx, olga_id).await, -2, "not marked");

    // A target that does not name olga as an alias refuses the Move, and
    // lets the next one through.
    let resp = ctx
        .api
        .post_signed(
            "/inbox",
            &move_activity(&olga, &olga, &bob_uri),
            &key_id,
            &olga_key,
        )
        .await;
    assert!(resp.status().is_success());
    assert_eq!(moved_to(&ctx, olga_id).await, None);
    assert_eq!(in_progress_ttl(&ctx, olga_id).await, -2, "unmarked");

    // Bob names her; the Move now holds. The new account is `target`.
    let resp = ctx
        .api
        .post_json(
            "/api/v1/profile/aliases",
            Some(&ctx.bob_token),
            &json!({ "acct": "olga@olga.invalid" }),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let resp = ctx
        .api
        .post_signed(
            "/inbox",
            &move_activity(&olga, &olga, &bob_uri),
            &key_id,
            &olga_key,
        )
        .await;
    assert!(resp.status().is_success());
    assert_eq!(moved_to(&ctx, olga_id).await, Some(bob_id));
    let ttl = in_progress_ttl(&ctx, olga_id).await;
    assert!(ttl > 6 * 24 * 60 * 60, "a week's lock, got {ttl}");

    // Carol follows bob now, straight away, with her old settings, and
    // olga is told she unfollowed.
    let follows = "SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2";
    assert!(exists(&ctx, follows, carol_id, bob_id).await);
    assert!(!exists(&ctx, follows, carol_id, olga_id).await);
    let reblogs: bool = sqlx::query_scalar(
        "SELECT show_reblogs FROM follows WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(carol_id)
    .bind(bob_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(!reblogs);
    let undo: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM eunha.ojak_queue
         WHERE payload->>'inbox' = $1 AND payload->'activity'->>'type' = 'Undo'",
    )
    .bind(format!("{olga}/inbox"))
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(undo, 1);

    let note: Option<String> = sqlx::query_scalar(
        "SELECT comment FROM account_notes WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(carol_id)
    .bind(bob_id)
    .fetch_optional(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        note.as_deref(),
        Some("This user moved from olga@olga.invalid, here were your previous notes about them:\nfrom the old server")
    );
    assert!(
        exists(
            &ctx,
            "SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = $2",
            dave_id,
            bob_id
        )
        .await
    );
    assert!(
        exists(
            &ctx,
            "SELECT 1 FROM mutes WHERE account_id = $1 AND target_account_id = $2",
            grace_id,
            bob_id
        )
        .await
    );

    // While the lock holds, another Move from her is not processed.
    sqlx::query("UPDATE accounts SET moved_to_account_id = NULL WHERE id = $1")
        .bind(olga_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    ctx.api
        .post_signed(
            "/inbox",
            &move_activity(&olga, &olga, &bob_uri),
            &key_id,
            &olga_key,
        )
        .await;
    assert_eq!(moved_to(&ctx, olga_id).await, None);
}

/// An Update of a remote actor keeps its `alsoKnownAs` and `movedTo`, which
/// is what a Move is checked against and what a redirect looks like.
#[tokio::test]
async fn test_an_actor_update_keeps_also_known_as_and_moved_to() {
    let ctx = TestContext::new("move-update").await;
    let (olga_id, olga, olga_key) = seed_remote(&ctx, "olga").await;
    let bob_uri = format!("https://{}/users/bob", ctx.domain);
    let update = |moved: Option<&str>| {
        let mut actor = json!({
            "id": olga,
            "type": "Person",
            "preferredUsername": "olga",
            "inbox": format!("{olga}/inbox"),
            "alsoKnownAs": ["https://older.invalid/users/olga", { "id": "https://oldest.invalid/@olga" }],
        });
        if let Some(moved) = moved {
            actor["movedTo"] = json!(moved);
        }
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{olga}#updates/{}", eunha::snowflake::next_id()),
            "type": "Update",
            "actor": olga,
            "object": actor,
        })
    };

    let resp = ctx
        .api
        .post_signed(
            "/inbox",
            &update(Some(&bob_uri)),
            &format!("{olga}#main-key"),
            &olga_key,
        )
        .await;
    assert!(resp.status().is_success());
    let aka: Option<Vec<String>> =
        sqlx::query_scalar("SELECT also_known_as FROM accounts WHERE id = $1")
            .bind(olga_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(
        aka.unwrap_or_default(),
        vec![
            "https://older.invalid/users/olga".to_owned(),
            "https://oldest.invalid/@olga".to_owned()
        ]
    );
    assert_eq!(
        moved_to(&ctx, olga_id).await,
        Some(ctx.bob_id.parse().unwrap())
    );

    ctx.api
        .post_signed(
            "/inbox",
            &update(None),
            &format!("{olga}#main-key"),
            &olga_key,
        )
        .await;
    assert_eq!(moved_to(&ctx, olga_id).await, None);
}
