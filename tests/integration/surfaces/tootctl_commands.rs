//! `eunha feeds`, `cache`, `statuses`, `media` and `preview_cards`, which
//! are `tootctl`'s (`lib/mastodon/cli/*.rb`).

use eunha::tootctl::{cache, feeds, media, preview_cards, statuses};
use redis::AsyncCommands as _;

use crate::helpers::TestContext;

/// A snowflake id for something made `days` ago.
fn id_days_ago(days: i64, n: i64) -> i64 {
    let ms = (chrono::Utc::now() - chrono::Duration::days(days)).timestamp_millis();
    (ms << 16) + n
}

async fn remote_account(ctx: &TestContext, username: &str) -> i64 {
    let id = eunha::snowflake::next_id();
    let domain = format!("remote-{}", ctx.domain);
    let uri = format!("https://{domain}/users/{username}");
    sqlx::query(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, 'remote-key', $4||'/inbox', $4||'/outbox',
                   now(), now())"#,
    )
    .bind(id)
    .bind(username)
    .bind(&domain)
    .bind(&uri)
    .execute(&ctx.db)
    .await
    .unwrap();
    id
}

async fn remote_status(ctx: &TestContext, account_id: i64, id: i64) {
    sqlx::query(
        r#"INSERT INTO statuses (id, account_id, text, visibility, local, uri, created_at, updated_at)
           VALUES ($1, $2, 'elsewhere', 0, false, $3, now(), now())"#,
    )
    .bind(id)
    .bind(account_id)
    .bind(format!("https://remote-{}/statuses/{id}", ctx.domain))
    .execute(&ctx.db)
    .await
    .unwrap();
}

async fn exists(ctx: &TestContext, table: &str, id: i64) -> bool {
    sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM {table} WHERE id = $1)"
    ))
    .bind(id)
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

async fn zcard(ctx: &TestContext, key: &str) -> u64 {
    let mut redis = ctx.state.redis.clone();
    redis.zcard(ctx.state.redis_keys.key(key)).await.unwrap()
}

/// `feeds clear` empties the instance's feeds and nothing else; `feeds
/// build` fills one again, for an account by name or for every active user.
#[tokio::test]
async fn feeds_are_cleared_and_built_again() {
    let ctx = TestContext::new("tootctl-feeds").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    ctx.api
        .post_status(&ctx.alice_token, "hello", "public")
        .await;
    let home = format!("feed:home:{alice}");
    assert!(zcard(&ctx, &home).await > 0);

    let mut redis = ctx.state.redis.clone();
    let elsewhere = format!("{}-elsewhere:feed:home:{alice}", ctx.domain);
    let _: () = redis.zadd(&elsewhere, 1, 1).await.unwrap();

    feeds::clear(&ctx.state).await.unwrap();
    assert_eq!(zcard(&ctx, &home).await, 0);
    let kept: u64 = redis.zcard(&elsewhere).await.unwrap();
    assert_eq!(
        kept, 1,
        "another prefix's feed is not the instance's to clear"
    );
    let _: () = redis.del(&elsewhere).await.unwrap();

    let found = feeds::find_local(&ctx.state, "ALICE").await.unwrap();
    assert_eq!(found, Some(alice));
    eunha::home_feed::precompute(&ctx.state, alice, false).await;
    assert!(zcard(&ctx, &home).await > 0);

    // Only users who signed in within the week are active.
    feeds::clear(&ctx.state).await.unwrap();
    sqlx::query(
        "UPDATE users SET confirmed_at = now(),
           current_sign_in_at = CASE WHEN account_id = $1 THEN now()
                                     ELSE now() - interval '30 days' END
         WHERE account_id IN ($1, $2)",
    )
    .bind(alice)
    .bind(bob)
    .execute(&ctx.db)
    .await
    .unwrap();
    let built = feeds::build_all(&ctx.state, 2, false, true, false)
        .await
        .unwrap();
    assert_eq!(built, 1);
    assert_eq!(zcard(&ctx, &home).await, 0, "a dry run builds nothing");
    feeds::build_all(&ctx.state, 2, false, false, false)
        .await
        .unwrap();
    assert!(zcard(&ctx, &home).await > 0);

    // The vacuum keeps the active user's feed and drops the inactive one's.
    let stale = format!("feed:home:{bob}");
    let _: () = redis
        .zadd(ctx.state.redis_keys.key(&stale), 1, 1)
        .await
        .unwrap();
    assert_eq!(feeds::vacuum_home(&ctx.state).await.unwrap(), 1);
    assert_eq!(zcard(&ctx, &stale).await, 0);
    assert!(zcard(&ctx, &home).await > 0);
}

/// `cache recount` puts drifted counters right; `cache clear` deletes what
/// is cached and leaves the feeds.
#[tokio::test]
async fn counters_are_recounted_and_the_cache_cleared() {
    let ctx = TestContext::new("tootctl-cache").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "counted", "public")
        .await;
    let status_id: i64 = status["id"].as_str().unwrap().parse().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{status_id}/favourite"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;
    sqlx::query("UPDATE account_stats SET statuses_count = 99 WHERE account_id = $1")
        .bind(alice)
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query("UPDATE status_stats SET favourites_count = 7 WHERE status_id = $1")
        .bind(status_id)
        .execute(&ctx.db)
        .await
        .unwrap();

    let accounts = cache::recount_accounts(&ctx.state, 2, false).await.unwrap();
    assert!(accounts >= 2);
    let statuses: i64 =
        sqlx::query_scalar("SELECT statuses_count FROM account_stats WHERE account_id = $1")
            .bind(alice)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(statuses, 1);
    cache::recount_statuses(&ctx.state, 2, false).await.unwrap();
    let favourites: i64 =
        sqlx::query_scalar("SELECT favourites_count FROM status_stats WHERE status_id = $1")
            .bind(status_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(favourites, 1);

    let mut redis = ctx.state.redis.clone();
    let keys = &ctx.state.redis_keys;
    for key in [
        "cache:api/v1/instances/activity/show",
        "followers_hash:1:local",
    ] {
        let _: () = redis.set(keys.key(key), "x").await.unwrap();
    }
    let _: () = redis.zadd(keys.key("feed:home:1"), 1, 1).await.unwrap();
    assert_eq!(cache::clear(&ctx.state).await.unwrap(), 2);
    let feed: u64 = redis.zcard(keys.key("feed:home:1")).await.unwrap();
    assert_eq!(feed, 1);
}

/// `statuses remove` deletes old remote statuses nothing local refers to,
/// then the uploads and conversations left orphaned.
#[tokio::test]
async fn unreferenced_remote_statuses_are_removed() {
    let ctx = TestContext::new("tootctl-statuses").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let stranger = remote_account(&ctx, "stranger").await;
    let followed = remote_account(&ctx, "followed").await;
    let unreferenced = id_days_ago(120, 1);
    let favourited = id_days_ago(120, 2);
    let recent = id_days_ago(10, 3);
    let by_followed = id_days_ago(120, 4);
    for id in [unreferenced, favourited, recent] {
        remote_status(&ctx, stranger, id).await;
    }
    remote_status(&ctx, followed, by_followed).await;
    sqlx::query(
        "INSERT INTO favourites (account_id, status_id, created_at, updated_at)
         VALUES ($1, $2, now(), now())",
    )
    .bind(alice)
    .bind(favourited)
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, now(), now())",
    )
    .bind(alice)
    .bind(followed)
    .execute(&ctx.db)
    .await
    .unwrap();
    let orphan_media = eunha::snowflake::next_id();
    sqlx::query(
        "INSERT INTO media_attachments (id, account_id, type, created_at, updated_at)
         VALUES ($1, $2, 0, now() - interval '100 days', now())",
    )
    .bind(orphan_media)
    .bind(alice)
    .execute(&ctx.db)
    .await
    .unwrap();
    let conversation: i64 = sqlx::query_scalar(
        "INSERT INTO conversations (created_at, updated_at) VALUES (now(), now()) RETURNING id",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();

    let mut lines = Vec::new();
    let removed = statuses::remove(&ctx.state, &statuses::RemoveOptions::default(), |l| {
        lines.push(l.to_owned())
    })
    .await
    .unwrap();
    assert_eq!(removed.statuses, 1);
    assert!(!exists(&ctx, "statuses", unreferenced).await);
    for kept in [favourited, recent, by_followed] {
        assert!(
            exists(&ctx, "statuses", kept).await,
            "{kept} should be kept"
        );
    }
    assert!(!exists(&ctx, "media_attachments", orphan_media).await);
    assert!(!exists(&ctx, "conversations", conversation).await);
    assert!(lines.contains(&"Running \"ANALYZE statuses\"...".to_owned()));
    assert!(lines
        .iter()
        .any(|l| l.starts_with("Done after ") && l.ends_with("removed 1 out of 1 statuses.")));
    let leftover: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('eunha.statuses_to_be_deleted')::text")
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(leftover, None);

    // With `--clean-followed`, the followed account's go too.
    let options = statuses::RemoveOptions {
        clean_followed: true,
        skip_media_remove: true,
        ..Default::default()
    };
    statuses::remove(&ctx.state, &options, |_| {})
        .await
        .unwrap();
    assert!(!exists(&ctx, "statuses", by_followed).await);
}

async fn cached_media(ctx: &TestContext, account_id: i64, days: i64) -> i64 {
    let id = eunha::snowflake::next_id();
    sqlx::query(
        "INSERT INTO media_attachments
           (id, account_id, type, remote_url, file_file_name, file_file_size,
            thumbnail_file_name, thumbnail_file_size, created_at, updated_at)
         VALUES ($1, $2, 0, 'https://elsewhere.invalid/a.png', 'a.png', 1000,
                 't.png', 24, now() - make_interval(days => $3), now())",
    )
    .bind(id)
    .bind(account_id)
    .bind(days as i32)
    .execute(&ctx.db)
    .await
    .unwrap();
    id
}

async fn file_name(ctx: &TestContext, table: &str, column: &str, id: i64) -> Option<String> {
    sqlx::query_scalar(&format!("SELECT {column} FROM {table} WHERE id = $1"))
        .bind(id)
        .fetch_one(&ctx.db)
        .await
        .unwrap()
}

/// `media remove` forgets old copies of remote media, and with
/// `--prune-profiles` remote avatars and headers.
#[tokio::test]
async fn cached_remote_media_is_removed() {
    let ctx = TestContext::new("tootctl-media").await;
    let stranger = remote_account(&ctx, "stranger").await;
    let old = cached_media(&ctx, stranger, 30).await;
    let new = cached_media(&ctx, stranger, 1).await;

    let dry = media::remove(
        &ctx.state,
        &media::RemoveOptions {
            dry_run: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        dry,
        vec!["Removed 1 media attachments (approx. 1 KB) (DRY RUN)".to_owned()]
    );
    assert!(file_name(&ctx, "media_attachments", "file_file_name", old)
        .await
        .is_some());
    media::remove(&ctx.state, &media::RemoveOptions::default())
        .await
        .unwrap();
    assert_eq!(
        file_name(&ctx, "media_attachments", "file_file_name", old).await,
        None
    );
    assert_eq!(
        file_name(&ctx, "media_attachments", "thumbnail_file_name", old).await,
        None
    );
    assert!(file_name(&ctx, "media_attachments", "file_file_name", new)
        .await
        .is_some());

    sqlx::query(
        "UPDATE accounts SET avatar_file_name = 'me.png', avatar_file_size = 2048,
           header_file_name = 'h.png', header_file_size = 1024,
           last_webfingered_at = now() - interval '30 days',
           updated_at = now() - interval '30 days'
         WHERE id = $1",
    )
    .bind(stranger)
    .execute(&ctx.db)
    .await
    .unwrap();
    let headers = media::remove(
        &ctx.state,
        &media::RemoveOptions {
            remove_headers: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        headers,
        vec!["Visited 1 accounts and removed profile media totaling 1 KB".to_owned()]
    );
    assert_eq!(
        file_name(&ctx, "accounts", "header_file_name", stranger).await,
        None
    );
    assert!(file_name(&ctx, "accounts", "avatar_file_name", stranger)
        .await
        .is_some());

    let refused = media::remove(
        &ctx.state,
        &media::RemoveOptions {
            prune_profiles: true,
            remove_headers: true,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        refused.to_string(),
        "--prune-profiles and --remove-headers should not be specified simultaneously"
    );
    let refused = media::remove(
        &ctx.state,
        &media::RemoveOptions {
            include_follows: true,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        refused.to_string(),
        "--include-follows can only be used with --prune-profiles or --remove-headers"
    );
}

/// `media remove-orphans` deletes the objects no record holds, keeps those
/// it does not recognize, and `media usage` and `media lookup` read the
/// records.
#[tokio::test]
async fn orphaned_objects_are_removed_and_media_looked_up() {
    let ctx = TestContext::new("tootctl-orphans").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "with media", "public")
        .await;
    let status_id: i64 = status["id"].as_str().unwrap().parse().unwrap();
    let held = eunha::snowflake::next_id();
    sqlx::query(
        "INSERT INTO media_attachments
           (id, account_id, status_id, type, file_file_name, file_file_size, created_at, updated_at)
         VALUES ($1, $2, $3, 0, 'original.png', 3000, now(), now())",
    )
    .bind(held)
    .bind(alice)
    .bind(status_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    let partition = eunha::media::int_to_path(held);
    let kept = format!("media_attachments/files/{partition}/original/original.png");
    let stale = format!("media_attachments/files/{partition}/original/replaced.png");
    let gone = format!(
        "media_attachments/files/{}/original/x.png",
        eunha::media::int_to_path(held + 1)
    );
    let foreign = "instance/icon/abc.png".to_owned();
    for key in [&kept, &stale, &gone, &foreign] {
        ctx.state
            .storage
            .store(b"bytes", key, "image/png")
            .await
            .unwrap();
    }

    let mut logged = Vec::new();
    let dry =
        media::remove_orphans(&ctx.state, None, None, true, |l| logged.push(l.to_owned())).await;
    assert_eq!(dry.removed, 2);
    assert_eq!(dry.bytes, 10);
    assert_eq!(dry.unrecognized, vec![foreign.clone()]);
    assert_eq!(ctx.state.storage.list("", None).await.unwrap().len(), 4);

    let report =
        media::remove_orphans(&ctx.state, Some("media_attachments/"), None, false, |_| {}).await;
    assert_eq!(report.removed, 2);
    let left: Vec<String> = ctx
        .state
        .storage
        .list("", None)
        .await
        .unwrap()
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(left, vec![foreign, kept.clone()]);

    let usage = media::usage(&ctx.state).await.unwrap();
    assert_eq!(usage[0], ("Attachments".to_owned(), 3000, Some(3000)));

    let page = media::lookup(&ctx.state, &format!("https://files.example/{kept}"))
        .await
        .unwrap();
    assert_eq!(page, format!("https://{}/@alice/{status_id}", ctx.domain));
    let refused = media::lookup(&ctx.state, "https://files.example/a/b.png")
        .await
        .unwrap_err();
    assert_eq!(refused.to_string(), "Not a media URL");
}

/// `media refresh` downloads remote media again, and eunha keeps none.
#[tokio::test]
async fn media_refresh_is_not_offered() {
    let error = media::refresh().unwrap_err();
    assert!(error.to_string().contains("no copies of remote media"));
}

/// `preview_cards remove` forgets old card images, only link cards' with
/// `--link`.
#[tokio::test]
async fn preview_card_images_are_removed() {
    let ctx = TestContext::new("tootctl-cards").await;
    let mut ids = Vec::new();
    for (n, kind) in [(1, 0), (2, 2)] {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO preview_cards (url, type, image_file_name, image_file_size,
               created_at, updated_at)
             VALUES ($1, $2, 'card.png', 2048, now(), now() - interval '200 days')
             RETURNING id",
        )
        .bind(format!("https://{}/card/{n}", ctx.domain))
        .bind(kind)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
        ids.push(id);
    }
    let said = preview_cards::remove(
        &ctx.state,
        &preview_cards::RemoveOptions {
            link: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        said,
        "Removed media from 1 link-type preview cards (approx. 2 KB)"
    );
    assert_eq!(
        file_name(&ctx, "preview_cards", "image_file_name", ids[0]).await,
        None
    );
    assert!(file_name(&ctx, "preview_cards", "image_file_name", ids[1])
        .await
        .is_some());
    let said = preview_cards::remove(&ctx.state, &preview_cards::RemoveOptions::default())
        .await
        .unwrap();
    assert_eq!(said, "Removed media from 1 preview cards (approx. 2 KB)");
}
