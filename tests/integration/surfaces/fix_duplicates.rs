//! `eunha maintenance fix-duplicates`, which is `tootctl maintenance
//! fix-duplicates`: duplicates a corrupt unique index let in are removed or
//! merged, and the index is built again as the tracked schema has it.
//!
//! The duplicates are made the way a collation change makes them possible:
//! the unique indexes are dropped first, as an index that no longer finds
//! what it holds would not have stopped them.

use eunha::tootctl::{maintenance, Recorder};

use crate::helpers::{seed_user, TestContext};

/// Ids in the order the rows were made: snowflakes made in one millisecond
/// are not, and which of two duplicates is older is what is kept.
fn ascending_id() -> i64 {
    static NEXT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let base = eunha::snowflake::next_id();
    let _ = NEXT.compare_exchange(
        0,
        base,
        std::sync::atomic::Ordering::SeqCst,
        std::sync::atomic::Ordering::SeqCst,
    );
    NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// Drop the unique indexes the duplicates below would break.
async fn corrupt_indexes(ctx: &TestContext) {
    for index in [
        "index_users_on_email",
        "index_custom_emojis_on_shortcode_and_domain",
        "index_custom_emoji_categories_on_name",
        "index_tags_on_name_lower_btree",
        "index_statuses_on_uri",
        "index_accounts_on_username_and_domain_lower",
        "index_domain_blocks_on_domain",
        "index_preview_cards_on_url",
    ] {
        sqlx::query(&format!("DROP INDEX public.{index}"))
            .execute(&ctx.db)
            .await
            .unwrap();
    }
}

async fn account(ctx: &TestContext, username: &str, domain: Option<&str>, key: &str) -> i64 {
    let uri = domain.map(|domain| format!("https://{domain}/users/{username}"));
    sqlx::query_scalar(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, uri, public_key,
                                 created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $5, now(), now() - make_interval(secs => $6))
           RETURNING id"#,
    )
    .bind(ascending_id())
    .bind(username)
    .bind(domain)
    .bind(uri)
    .bind(key)
    // Later ones were updated earlier, so the first is the one kept.
    .bind(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM accounts")
            .fetch_one(&ctx.db)
            .await
            .unwrap() as f64,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

async fn status(ctx: &TestContext, account_id: i64, uri: &str) -> i64 {
    sqlx::query_scalar(
        r#"INSERT INTO statuses (id, account_id, text, uri, created_at, updated_at)
           VALUES ($1, $2, 'hi', $3, now(), now()) RETURNING id"#,
    )
    .bind(ascending_id())
    .bind(account_id)
    .bind(uri)
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

async fn count(ctx: &TestContext, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&ctx.db).await.unwrap()
}

/// Every kind of duplicate Mastodon's task handles is handled as it handles
/// it, the indexes are back as the tracked schema has them, and a local
/// account the operator did not pick is renamed rather than lost.
#[tokio::test]
async fn test_duplicates_are_merged_and_the_indexes_restored() {
    let ctx = TestContext::new("fix-dups").await;
    corrupt_indexes(&ctx).await;

    // Two users with one address: the most recently updated keeps it.
    let (carol, _) = seed_user(&ctx.db, &ctx.domain, "carol", "same@test.invalid").await;
    let (dave, _) = seed_user(&ctx.db, &ctx.domain, "dave", "same@test.invalid").await;
    sqlx::query("UPDATE users SET updated_at = now() - interval '1 day' WHERE account_id = $1")
        .bind(carol)
        .execute(&ctx.db)
        .await
        .unwrap();

    // Two local accounts that a broken index let share a name.
    let erin_old = account(&ctx, "erin", None, "").await;
    let erin_new = account(&ctx, "Erin", None, "").await;

    // Two rows for one remote actor (one key), and one for another actor
    // under the same handle (another key).
    let remote = format!("remote-{}.invalid", &ctx.domain[..8]);
    let kept = account(&ctx, "zed", Some(&remote), "KEY").await;
    let same = account(&ctx, "Zed", Some(&remote), "KEY").await;
    let other = account(&ctx, "ZED", Some(&remote), "OTHER").await;
    let same_status = status(&ctx, same, &format!("https://{remote}/notes/1")).await;
    let other_status = status(&ctx, other, &format!("https://{remote}/notes/2")).await;

    // One status twice: the oldest stays, and keeps the other's favourite.
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    let first = status(&ctx, kept, &format!("https://{remote}/notes/3")).await;
    let second = status(&ctx, kept, &format!("https://{remote}/notes/3")).await;
    sqlx::query(
        "INSERT INTO favourites (account_id, status_id, created_at, updated_at)
         VALUES ($1, $2, now(), now())",
    )
    .bind(alice)
    .bind(second)
    .execute(&ctx.db)
    .await
    .unwrap();

    // Tags: the one that is the most usable, trendable and listable stays.
    for (name, usable) in [("Rust", false), ("rust", true)] {
        sqlx::query(
            "INSERT INTO tags (name, usable, trendable, listable, created_at, updated_at)
             VALUES ($1, $2, $2, $2, now(), now())",
        )
        .bind(name)
        .bind(usable)
        .execute(&ctx.db)
        .await
        .unwrap();
    }
    let featured_tag: i64 = sqlx::query_scalar("SELECT id FROM tags WHERE name = 'Rust'")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO featured_tags (account_id, tag_id, name, created_at, updated_at)
         VALUES ($1, $2, 'Rust', now(), now())",
    )
    .bind(bob)
    .bind(featured_tag)
    .execute(&ctx.db)
    .await
    .unwrap();

    // Emoji and their categories, domain blocks and preview cards.
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO custom_emoji_categories (name, created_at, updated_at)
             VALUES ('Blobs', now(), now())",
        )
        .execute(&ctx.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO custom_emojis (shortcode, category_id, created_at, updated_at)
             VALUES ('blob', (SELECT min(id) FROM custom_emoji_categories), now(), now())",
        )
        .execute(&ctx.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO preview_cards (url, created_at, updated_at)
             VALUES ('https://card.invalid/', now(), now())",
        )
        .execute(&ctx.db)
        .await
        .unwrap();
    }
    for (severity, reject_media, comment) in [(0, true, Some("noisy")), (1, false, None)] {
        sqlx::query(
            "INSERT INTO domain_blocks (domain, severity, reject_media, private_comment,
                                        created_at, updated_at)
             VALUES ('blocked.invalid', $1, $2, $3, now(), now())",
        )
        .bind(severity)
        .bind(reject_media)
        .bind(comment)
        .execute(&ctx.db)
        .await
        .unwrap();
    }

    // Keep the second account listed, which is the older one.
    let console = Recorder::answering(["1"]);
    let mut conn = ctx.db.acquire().await.unwrap();
    maintenance::deduplicate(&mut conn, &console).await.unwrap();
    drop(conn);
    let output = console.output();

    // Users.
    assert!(output.contains("Multiple users registered with e-mail address same@test.invalid."));
    let emails: Vec<(i64, String)> = sqlx::query_as(
        "SELECT account_id, email FROM users WHERE account_id IN ($1, $2) ORDER BY account_id",
    )
    .bind(carol)
    .bind(dave)
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert!(emails.contains(&(dave, "same@test.invalid".into())));
    assert!(emails.contains(&(carol, "0 same@test.invalid".into())));

    // Local accounts: the operator kept the older; the newer is renamed.
    assert!(output.contains("Multiple local accounts were found for username 'Erin'."));
    assert!(output.contains(" 0. Erin: created at: "));
    let names: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, username FROM accounts WHERE id IN ($1, $2) ORDER BY id")
            .bind(erin_old)
            .bind(erin_new)
            .fetch_all(&ctx.db)
            .await
            .unwrap();
    assert_eq!(
        names,
        [
            (erin_old, "erin".to_owned()),
            (erin_new, "Erin_1".to_owned())
        ]
    );

    // Remote accounts: the same actor is merged, another is dropped.
    let left: Vec<i64> = sqlx::query_scalar("SELECT id FROM accounts WHERE domain = $1")
        .bind(&remote)
        .fetch_all(&ctx.db)
        .await
        .unwrap();
    assert_eq!(left, [kept]);
    let owner: Option<i64> = sqlx::query_scalar("SELECT account_id FROM statuses WHERE id = $1")
        .bind(same_status)
        .fetch_optional(&ctx.db)
        .await
        .unwrap();
    assert_eq!(owner, Some(kept), "the duplicate's post was given over");
    assert_eq!(
        count(
            &ctx,
            &format!("SELECT count(*) FROM statuses WHERE id = {other_status}")
        )
        .await,
        0,
        "another actor's post went with it"
    );

    // Statuses.
    assert_eq!(
        count(
            &ctx,
            &format!("SELECT count(*) FROM statuses WHERE id = {second}")
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &ctx,
            &format!("SELECT count(*) FROM favourites WHERE status_id = {first}")
        )
        .await,
        1
    );

    // Tags.
    let tags: Vec<String> = sqlx::query_scalar("SELECT name FROM tags WHERE lower(name) = 'rust'")
        .fetch_all(&ctx.db)
        .await
        .unwrap();
    assert_eq!(tags, ["rust"]);
    assert_eq!(
        count(
            &ctx,
            "SELECT count(*) FROM featured_tags f JOIN tags t ON t.id = f.tag_id WHERE t.name = 'rust'"
        )
        .await,
        1
    );

    // Emoji, categories, cards and blocks.
    assert_eq!(count(&ctx, "SELECT count(*) FROM custom_emojis").await, 1);
    assert_eq!(
        count(&ctx, "SELECT count(*) FROM custom_emoji_categories").await,
        1
    );
    assert_eq!(
        count(
            &ctx,
            "SELECT count(*) FROM custom_emojis e JOIN custom_emoji_categories c ON c.id = e.category_id"
        )
        .await,
        1
    );
    assert_eq!(count(&ctx, "SELECT count(*) FROM preview_cards").await, 1);
    let block: (i32, bool, Option<String>) = sqlx::query_as(
        "SELECT severity, reject_media, private_comment FROM domain_blocks
         WHERE domain = 'blocked.invalid'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(block, (1, true, Some("noisy".to_owned())));

    // The indexes are the tracked schema's again.
    let live = eunha::schema_check::introspect(&ctx.db).await.unwrap();
    let findings = eunha::schema_check::diff(&live, &eunha::upstream::reference_schema());
    assert!(
        findings.is_empty(),
        "fix-duplicates left the schema drifted:\n{}",
        findings
            .iter()
            .map(|f| format!("  {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The checks before anything is touched: the schema is the tracked one,
/// nothing else is connected, and the operator says to go on.
#[tokio::test]
async fn test_it_refuses_to_run_unasked_or_beside_a_running_instance() {
    let ctx = TestContext::new("fix-dups-checks").await;
    let mut conn = ctx.db.acquire().await.unwrap();

    let console = Recorder::default();
    maintenance::verify_schema_version(&mut conn, &console)
        .await
        .unwrap();
    assert!(console.lines().is_empty());

    // The test server's own pool is connected.
    let error = maintenance::verify_nothing_running(&mut conn)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("All processes need to be stopped"),
        "{error}"
    );

    let console = Recorder::answering(["no"]);
    let error = maintenance::verify_backup_warning(&console)
        .unwrap_err()
        .to_string();
    assert_eq!(error, "Maintenance process stopped.");
    assert!(console
        .output()
        .contains("This task will take a long time to run and is potentially destructive."));
    assert!(maintenance::verify_backup_warning(&Recorder::answering(["Yes"])).is_ok());

    // A schema newer than this binary is gone on with only when asked.
    sqlx::query("INSERT INTO public.schema_migrations (version) VALUES ('99990101000000')")
        .execute(&mut *conn)
        .await
        .unwrap();
    let console = Recorder::answering(["n"]);
    let error = maintenance::verify_schema_version(&mut conn, &console)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "Stopping maintenance script because data is more recent than script version."
    );
    assert!(
        maintenance::verify_schema_version(&mut conn, &Recorder::answering(["y"]))
            .await
            .is_ok()
    );
}
