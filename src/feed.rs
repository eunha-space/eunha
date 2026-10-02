use crate::redis_keys::RedisKeyspace;
use redis::{aio::ConnectionManager, AsyncCommands};
use sqlx::PgPool;
use std::sync::atomic::{AtomicBool, Ordering};

const FEED_MAX_ITEMS: isize = 800;
const FEED_TTL_SECS: u64 = 7 * 24 * 3600; // 1 week

// When true, fanout/populate/backfill run inline (no tokio::spawn).
// Set by integration tests to eliminate timing races.
static SYNC_FANOUT: AtomicBool = AtomicBool::new(false);

pub fn enable_sync_fanout() {
    SYNC_FANOUT.store(true, Ordering::Relaxed);
}

pub fn sync_fanout() -> bool {
    SYNC_FANOUT.load(Ordering::Relaxed)
}

fn feed_key(keys: &RedisKeyspace, account_id: i64) -> String {
    keys.key(format!("feed:home:{}", account_id))
}

fn populated_key(keys: &RedisKeyspace, account_id: i64) -> String {
    keys.key(format!("feed:home:{}:populated", account_id))
}

pub async fn is_feed_populated(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    account_id: i64,
) -> bool {
    redis
        .exists::<_, bool>(populated_key(keys, account_id))
        .await
        .unwrap_or(false)
}

pub async fn feed_push(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    account_id: i64,
    status_id: i64,
) {
    let key = feed_key(keys, account_id);
    let result: redis::RedisResult<()> = redis::pipe()
        .zadd(&key, status_id, status_id as f64)
        .zremrangebyrank(&key, 0, -(FEED_MAX_ITEMS + 1))
        .ignore()
        .query_async(redis)
        .await;
    if let Err(e) = result {
        tracing::warn!("feed_push error for account {}: {}", account_id, e);
    }
}

pub async fn feed_remove(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    account_id: i64,
    status_id: i64,
) {
    let result: redis::RedisResult<()> = redis.zrem(feed_key(keys, account_id), status_id).await;
    if let Err(e) = result {
        tracing::warn!("feed_remove error for account {}: {}", account_id, e);
    }
}

/// Fetch status IDs from the Redis feed, honouring Mastodon-style pagination.
/// Returns None if the feed has never been populated (cold start signal).
pub async fn feed_get(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: isize,
) -> Option<Vec<i64>> {
    if !is_feed_populated(redis, keys, account_id).await {
        return None;
    }

    let key = feed_key(keys, account_id);

    let ids: Vec<i64> = if let Some(min_id) = min_id {
        let min_score = format!("({min_id}");
        redis::cmd("ZRANGEBYSCORE")
            .arg(&key)
            .arg(&min_score)
            .arg("+inf")
            .arg("LIMIT")
            .arg(0i64)
            .arg(limit)
            .query_async(redis)
            .await
            .unwrap_or_default()
    } else {
        let max_score = max_id
            .map(|id| format!("({}", id))
            .unwrap_or_else(|| "+inf".to_string());
        let min_score = since_id
            .map(|id| format!("({}", id))
            .unwrap_or_else(|| "-inf".to_string());
        redis::cmd("ZREVRANGEBYSCORE")
            .arg(&key)
            .arg(&max_score)
            .arg(&min_score)
            .arg("LIMIT")
            .arg(0i64)
            .arg(limit)
            .query_async(redis)
            .await
            .unwrap_or_default()
    };

    Some(ids)
}

/// Populate the Redis feed from DB (called on first timeline load).
pub async fn feed_populate(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    account_id: i64,
    db: &PgPool,
) {
    let _: redis::RedisResult<()> = redis
        .set_ex(populated_key(keys, account_id), 1i64, FEED_TTL_SECS)
        .await;

    // The reply clause mirrors Mastodon's FeedManager#filter_from_home: a reply
    // is only kept when it is the viewer's own post, a reply to the viewer, a
    // self-reply, or a reply to someone the viewer follows. Orphan replies (no
    // in_reply_to_account_id) are dropped.
    let status_ids: Vec<i64> = sqlx::query_scalar!(
        r#"WITH candidate_ids AS (
               SELECT s.id FROM statuses s
               WHERE s.account_id IN (
                   SELECT target_account_id FROM follows
                   WHERE account_id = $1
                   UNION ALL SELECT $1
               )
               AND s.deleted_at IS NULL
               AND (
                   NOT s.reply
                   OR s.account_id = $1
                   OR (
                       s.in_reply_to_account_id IS NOT NULL
                       AND (
                           s.in_reply_to_account_id = $1
                           OR s.in_reply_to_account_id = s.account_id
                           OR EXISTS (
                               SELECT 1 FROM follows f
                               WHERE f.account_id = $1
                                 AND f.target_account_id = s.in_reply_to_account_id
                           )
                       )
                   )
               )
               -- Per-follow language filter (Mastodon crutches[:languages]):
               -- drop a followee's status whose language isn't in the language
               -- subset the viewer chose for that follow.
               AND (
                   s.language IS NULL
                   OR s.account_id = $1
                   OR NOT EXISTS (
                       SELECT 1 FROM follows fl
                       WHERE fl.account_id = $1
                         AND fl.target_account_id = s.account_id
                         AND fl.languages IS NOT NULL
                         AND array_length(fl.languages, 1) >= 1
                         AND NOT (s.language = ANY(fl.languages))
                   )
               )
               UNION
               SELECT st.status_id FROM statuses_tags st
               JOIN tag_follows tf ON tf.tag_id = st.tag_id
               JOIN statuses s ON s.id = st.status_id
               WHERE tf.account_id = $1
               AND s.visibility = 0
               AND s.deleted_at IS NULL
           )
           SELECT id FROM candidate_ids ORDER BY id DESC LIMIT $2"#,
        account_id,
        FEED_MAX_ITEMS as i64,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .flatten()
    .collect();

    if !status_ids.is_empty() {
        let key = feed_key(keys, account_id);
        let mut pipe = redis::pipe();
        for &id in &status_ids {
            pipe.zadd(&key, id, id as f64);
        }
        pipe.expire(&key, FEED_TTL_SECS as i64);
        let _: redis::RedisResult<()> = pipe.query_async(redis).await;
    }
}

/// Fan-out a newly posted status to all followers' initialized feeds,
/// plus accounts following any of the status's hashtags.
pub async fn fanout_new_status(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    author_id: i64,
    status_id: i64,
    tag_ids: &[i64],
) {
    // Look up the status's reply shape and language so it is only fanned to
    // followers who should see it (Mastodon FeedManager#filter_from_home).
    let reply_meta = sqlx::query!(
        "SELECT reply, in_reply_to_account_id, language FROM statuses WHERE id = $1",
        status_id,
    )
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    let is_reply = reply_meta.as_ref().map(|m| m.reply).unwrap_or(false);
    let reply_to = reply_meta.as_ref().and_then(|m| m.in_reply_to_account_id);
    let language = reply_meta.as_ref().and_then(|m| m.language.clone());

    let mut follower_ids: Vec<i64> = if is_reply && reply_to.is_none() {
        // Orphan reply (parent gone): filtered from every follower's home.
        Vec::new()
    } else if let Some(target) = reply_to.filter(|&t| is_reply && t != author_id) {
        // Reply to someone else: only followers who are, or who follow, that
        // account (a reply to the target themselves is covered by the first arm).
        sqlx::query_scalar!(
            r#"SELECT f.account_id FROM follows f
               WHERE f.target_account_id = $1
                 AND (
                     f.account_id = $2
                     OR EXISTS (
                         SELECT 1 FROM follows f2
                         WHERE f2.account_id = f.account_id AND f2.target_account_id = $2
                     )
                 )"#,
            author_id,
            target,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default()
    } else {
        // Non-reply or self-reply: all followers.
        sqlx::query_scalar!(
            "SELECT account_id FROM follows WHERE target_account_id = $1",
            author_id,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default()
    };

    // Drop followers who restricted this follow to a language subset that
    // excludes the status's language (Mastodon crutches[:languages]).
    if let Some(ref lang) = language {
        if !follower_ids.is_empty() {
            let excluded: Vec<i64> = sqlx::query_scalar!(
                r#"SELECT account_id FROM follows
                   WHERE target_account_id = $1
                     AND account_id = ANY($2::bigint[])
                     AND languages IS NOT NULL
                     AND array_length(languages, 1) >= 1
                     AND NOT ($3 = ANY(languages))"#,
                author_id,
                &follower_ids,
                lang,
            )
            .fetch_all(db)
            .await
            .unwrap_or_default();
            if !excluded.is_empty() {
                let ex: std::collections::HashSet<i64> = excluded.into_iter().collect();
                follower_ids.retain(|id| !ex.contains(id));
            }
        }
    }

    let hashtag_recipients: Vec<i64> = if !tag_ids.is_empty() {
        sqlx::query_scalar!(
            r#"SELECT DISTINCT tf.account_id FROM tag_follows tf
               WHERE tf.tag_id = ANY($1::bigint[])
               AND tf.account_id != $2
               AND NOT EXISTS (
                   SELECT 1 FROM follows
                   WHERE account_id = tf.account_id
                   AND target_account_id = $2
               )"#,
            tag_ids as &[i64],
            author_id,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default()
    } else {
        vec![]
    };

    let recipients: Vec<i64> = std::iter::once(author_id)
        .chain(follower_ids)
        .chain(hashtag_recipients)
        .collect();

    if recipients.is_empty() {
        return;
    }

    let pop_keys: Vec<String> = recipients
        .iter()
        .map(|&id| populated_key(keys, id))
        .collect();
    let initialized: Vec<Option<i64>> = match redis.mget(&pop_keys).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("fanout mget error: {}", e);
            return;
        }
    };

    let score = status_id as f64;
    let mut pipe = redis::pipe();
    let mut any = false;
    for (&id, init) in recipients.iter().zip(initialized.iter()) {
        if init.is_some() {
            let key = feed_key(keys, id);
            pipe.zadd(&key, status_id, score);
            pipe.zremrangebyrank(&key, 0, -(FEED_MAX_ITEMS + 1));
            any = true;
        }
    }
    if any {
        let _: redis::RedisResult<()> = pipe.query_async(redis).await;
    }
}

/// Remove a deleted status from all followers' initialized feeds.
pub async fn fanout_remove_status(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    author_id: i64,
    status_id: i64,
) {
    let follower_ids: Vec<i64> = sqlx::query_scalar!(
        "SELECT account_id FROM follows WHERE target_account_id = $1",
        author_id,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();

    let recipients: Vec<i64> = std::iter::once(author_id).chain(follower_ids).collect();
    let pop_keys: Vec<String> = recipients
        .iter()
        .map(|&id| populated_key(keys, id))
        .collect();
    let initialized: Vec<Option<i64>> = match redis.mget(&pop_keys).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("fanout_remove mget error: {}", e);
            return;
        }
    };

    let mut pipe = redis::pipe();
    let mut any = false;
    for (&id, init) in recipients.iter().zip(initialized.iter()) {
        if init.is_some() {
            pipe.zrem(feed_key(keys, id), status_id);
            any = true;
        }
    }
    if any {
        let _: redis::RedisResult<()> = pipe.query_async(redis).await;
    }
}

// ── List feed ─────────────────────────────────────────────────────────────

fn list_feed_key(keys: &RedisKeyspace, list_id: i64) -> String {
    keys.key(format!("feed:list:{}", list_id))
}

fn list_populated_key(keys: &RedisKeyspace, list_id: i64) -> String {
    keys.key(format!("feed:list:{}:populated", list_id))
}

pub async fn is_list_feed_populated(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    list_id: i64,
) -> bool {
    redis
        .exists::<_, bool>(list_populated_key(keys, list_id))
        .await
        .unwrap_or(false)
}

/// Fetch status IDs from a list's Redis feed.
pub async fn list_feed_get(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    list_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: isize,
) -> Option<Vec<i64>> {
    if !is_list_feed_populated(redis, keys, list_id).await {
        return None;
    }
    let key = list_feed_key(keys, list_id);
    let ids: Vec<i64> = if let Some(min_id) = min_id {
        let min_score = format!("({min_id}");
        redis::cmd("ZRANGEBYSCORE")
            .arg(&key)
            .arg(&min_score)
            .arg("+inf")
            .arg("LIMIT")
            .arg(0i64)
            .arg(limit)
            .query_async(redis)
            .await
            .unwrap_or_default()
    } else {
        let max_score = max_id
            .map(|id| format!("({}", id))
            .unwrap_or_else(|| "+inf".to_string());
        let min_score = since_id
            .map(|id| format!("({}", id))
            .unwrap_or_else(|| "-inf".to_string());
        redis::cmd("ZREVRANGEBYSCORE")
            .arg(&key)
            .arg(&max_score)
            .arg(&min_score)
            .arg("LIMIT")
            .arg(0i64)
            .arg(limit)
            .query_async(redis)
            .await
            .unwrap_or_default()
    };
    Some(ids)
}

/// Populate a list's Redis feed from DB (called on first list timeline access).
pub async fn list_feed_populate(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    list_id: i64,
    owner_id: i64,
    replies_policy: &str,
    db: &PgPool,
) {
    let _: redis::RedisResult<()> = redis
        .set_ex(list_populated_key(keys, list_id), 1i64, FEED_TTL_SECS)
        .await;

    let status_ids: Vec<i64> = match replies_policy {
        "none" => sqlx::query_scalar!(
            r#"SELECT s.id FROM statuses s
               JOIN list_accounts la ON la.account_id = s.account_id
               WHERE la.list_id = $1
                 AND s.deleted_at IS NULL
                 AND s.visibility != 3
                 AND (s.in_reply_to_id IS NULL
                      OR s.in_reply_to_account_id = s.account_id
                      OR s.in_reply_to_account_id = $2)
               ORDER BY s.id DESC LIMIT $3"#,
            list_id,
            owner_id,
            FEED_MAX_ITEMS as i64,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default(),
        "list" => sqlx::query_scalar!(
            r#"SELECT s.id FROM statuses s
               JOIN list_accounts la ON la.account_id = s.account_id
               WHERE la.list_id = $1
                 AND s.deleted_at IS NULL
                 AND s.visibility != 3
                 AND (s.in_reply_to_id IS NULL
                      OR s.in_reply_to_account_id = $2
                      OR EXISTS (
                          SELECT 1 FROM statuses s2
                          JOIN list_accounts la2 ON la2.account_id = s2.account_id
                          WHERE s2.id = s.in_reply_to_id AND la2.list_id = $1
                      ))
               ORDER BY s.id DESC LIMIT $3"#,
            list_id,
            owner_id,
            FEED_MAX_ITEMS as i64,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default(),
        _ => sqlx::query_scalar!(
            r#"SELECT s.id FROM statuses s
               JOIN list_accounts la ON la.account_id = s.account_id
               WHERE la.list_id = $1
                 AND s.deleted_at IS NULL
                 AND s.visibility != 3
                 -- `FeedManager#filter_from_list?` with `show_followed?`.
                 AND (s.in_reply_to_id IS NULL
                      OR s.in_reply_to_account_id = s.account_id
                      OR s.in_reply_to_account_id = $2
                      OR EXISTS (
                          SELECT 1 FROM follows f
                          WHERE f.account_id = $2 AND f.target_account_id = s.in_reply_to_account_id
                      ))
               ORDER BY s.id DESC LIMIT $3"#,
            list_id,
            owner_id,
            FEED_MAX_ITEMS as i64,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default(),
    };

    if !status_ids.is_empty() {
        let key = list_feed_key(keys, list_id);
        let mut pipe = redis::pipe();
        for &id in &status_ids {
            pipe.zadd(&key, id, id as f64);
        }
        pipe.expire(&key, FEED_TTL_SECS as i64);
        let _: redis::RedisResult<()> = pipe.query_async(redis).await;
    }
}

/// Fan out a newly posted status to all initialized list feeds that contain the author.
pub async fn fanout_to_lists(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    author_id: i64,
    status_id: i64,
    in_reply_to_account_id: Option<i64>,
    visibility: &str,
) {
    if visibility == "direct" {
        return;
    }

    let lists = sqlx::query!(
        r#"SELECT l.id, l.account_id,
                  CASE l.replies_policy WHEN 0 THEN 'list' WHEN 1 THEN 'followed' WHEN 2 THEN 'none' ELSE 'list' END AS "replies_policy!"
           FROM lists l
           JOIN list_accounts la ON la.list_id = l.id
           WHERE la.account_id = $1"#,
        author_id,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();

    if lists.is_empty() {
        return;
    }

    let pop_keys: Vec<String> = lists
        .iter()
        .map(|l| list_populated_key(keys, l.id))
        .collect();
    let initialized: Vec<Option<i64>> = match redis.mget(&pop_keys).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("fanout_to_lists mget error: {}", e);
            return;
        }
    };

    let score = status_id as f64;
    let mut pipe = redis::pipe();
    let mut any = false;

    for (list, init) in lists.iter().zip(initialized.iter()) {
        if init.is_none() {
            continue;
        }

        let passes = if let Some(reply_author) = in_reply_to_account_id {
            if reply_author == author_id || reply_author == list.account_id {
                true
            } else {
                match list.replies_policy.as_str() {
                    "none" => false,
                    "list" => sqlx::query_scalar!(
                        "SELECT 1 FROM list_accounts WHERE list_id = $1 AND account_id = $2",
                        list.id,
                        reply_author,
                    )
                    .fetch_optional(db)
                    .await
                    .unwrap_or(None)
                    .is_some(),
                    _ => sqlx::query_scalar!(
                        "SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2",
                        list.account_id,
                        reply_author,
                    )
                    .fetch_optional(db)
                    .await
                    .unwrap_or(None)
                    .is_some(),
                }
            }
        } else {
            true
        };

        if passes {
            let key = list_feed_key(keys, list.id);
            pipe.zadd(&key, status_id, score);
            pipe.zremrangebyrank(&key, 0, -(FEED_MAX_ITEMS + 1));
            any = true;
        }
    }

    if any {
        let _: redis::RedisResult<()> = pipe.query_async(redis).await;
    }
}

/// Remove a deleted status from all initialized list feeds that contain the author.
pub async fn fanout_remove_from_lists(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    author_id: i64,
    status_id: i64,
) {
    let list_ids: Vec<i64> = sqlx::query_scalar!(
        "SELECT l.id FROM lists l JOIN list_accounts la ON la.list_id = l.id WHERE la.account_id = $1",
        author_id,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();

    if list_ids.is_empty() {
        return;
    }

    let pop_keys: Vec<String> = list_ids
        .iter()
        .map(|&id| list_populated_key(keys, id))
        .collect();
    let initialized: Vec<Option<i64>> = match redis.mget(&pop_keys).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("fanout_remove_from_lists mget error: {}", e);
            return;
        }
    };

    let mut pipe = redis::pipe();
    let mut any = false;
    for (&list_id, init) in list_ids.iter().zip(initialized.iter()) {
        if init.is_some() {
            pipe.zrem(list_feed_key(keys, list_id), status_id);
            any = true;
        }
    }
    if any {
        let _: redis::RedisResult<()> = pipe.query_async(redis).await;
    }
}

/// Backfill a list feed with recent statuses from a newly-added member.
pub async fn backfill_list_member(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    list_id: i64,
    member_id: i64,
    owner_id: i64,
    replies_policy: &str,
) {
    if !is_list_feed_populated(redis, keys, list_id).await {
        return;
    }

    let recent: Vec<i64> = match replies_policy {
        "none" => sqlx::query_scalar!(
            r#"SELECT id FROM statuses
               WHERE account_id = $1 AND deleted_at IS NULL AND visibility != 3
                 AND (in_reply_to_id IS NULL
                      OR in_reply_to_account_id = $1
                      OR in_reply_to_account_id = $2)
               ORDER BY id DESC LIMIT 20"#,
            member_id,
            owner_id,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default(),
        "list" => sqlx::query_scalar!(
            r#"SELECT s.id FROM statuses s
               WHERE s.account_id = $1 AND s.deleted_at IS NULL AND s.visibility != 3
                 AND (s.in_reply_to_id IS NULL
                      OR s.in_reply_to_account_id = $3
                      OR EXISTS (
                          SELECT 1 FROM statuses s2
                          JOIN list_accounts la ON la.account_id = s2.account_id
                          WHERE s2.id = s.in_reply_to_id AND la.list_id = $2
                      ))
               ORDER BY s.id DESC LIMIT 20"#,
            member_id,
            list_id,
            owner_id,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default(),
        _ => sqlx::query_scalar!(
            r#"SELECT s.id FROM statuses s
               WHERE s.account_id = $1 AND s.deleted_at IS NULL AND s.visibility != 3
                 -- `FeedManager#filter_from_list?` with `show_followed?`.
                 AND (s.in_reply_to_id IS NULL
                      OR s.in_reply_to_account_id = s.account_id
                      OR s.in_reply_to_account_id = $2
                      OR EXISTS (
                          SELECT 1 FROM follows f
                          WHERE f.account_id = $2 AND f.target_account_id = s.in_reply_to_account_id
                      ))
               ORDER BY s.id DESC LIMIT 20"#,
            member_id,
            owner_id,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default(),
    };

    if recent.is_empty() {
        return;
    }

    let key = list_feed_key(keys, list_id);
    let mut pipe = redis::pipe();
    for &id in &recent {
        pipe.zadd(&key, id, id as f64);
    }
    pipe.zremrangebyrank(&key, 0, -(FEED_MAX_ITEMS + 1));
    let _: redis::RedisResult<()> = pipe.query_async(redis).await;
}

/// Delete an account's home feed keys (Mastodon's `FeedManager#clean_feeds!`,
/// called when the account is deleted).
pub async fn delete_home_feed(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    account_id: i64,
) {
    let _: redis::RedisResult<()> = redis::pipe()
        .del(feed_key(keys, account_id))
        .del(populated_key(keys, account_id))
        .query_async(redis)
        .await;
}

/// Delete a list's Redis feed keys (called when the list itself is deleted).
pub async fn delete_list_feed(redis: &mut ConnectionManager, keys: &RedisKeyspace, list_id: i64) {
    let _: redis::RedisResult<()> = redis::pipe()
        .del(list_feed_key(keys, list_id))
        .del(list_populated_key(keys, list_id))
        .query_async(redis)
        .await;
}

/// Backfill the follower's feed with recent statuses from the newly-followed account.
pub async fn backfill_follow(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    follower_id: i64,
    followed_id: i64,
) {
    if !is_feed_populated(redis, keys, follower_id).await {
        return;
    }

    let recent: Vec<i64> = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE account_id = $1 AND deleted_at IS NULL ORDER BY id DESC LIMIT 20",
        followed_id,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();

    if recent.is_empty() {
        return;
    }

    let key = feed_key(keys, follower_id);
    let mut pipe = redis::pipe();
    for &id in &recent {
        pipe.zadd(&key, id, id as f64);
    }
    pipe.zremrangebyrank(&key, 0, -(FEED_MAX_ITEMS + 1));
    let _: redis::RedisResult<()> = pipe.query_async(redis).await;
}

/// Remove the (former) followee's statuses from the follower's home feed.
/// Mirrors Mastodon's `FeedManager#unmerge_from_home`, called on unfollow and
/// block so an ex-followee's posts (and their own reblogs) stop lingering in
/// the cached timeline until the next full repopulate.
pub async fn unmerge_from_home(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    from_account_id: i64,
    into_account_id: i64,
) {
    if !is_feed_populated(redis, keys, into_account_id).await {
        return;
    }

    let key = feed_key(keys, into_account_id);
    // The feed's *members* are exact status ids (only the ZSET scores are lossy
    // f64s), so read the members and keep the ones authored by the ex-followee.
    // This both bounds the DB scan (the feed holds at most FEED_MAX_ITEMS) and
    // avoids the snowflake-precision pitfall of comparing against a float score.
    let members: Vec<i64> = redis
        .zrange::<_, Vec<i64>>(&key, 0, -1)
        .await
        .unwrap_or_default();
    if members.is_empty() {
        return;
    }

    let ids: Vec<i64> = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE account_id = $1 AND id = ANY($2::bigint[])",
        from_account_id,
        &members,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();

    if ids.is_empty() {
        return;
    }

    let mut pipe = redis::pipe();
    for id in ids {
        pipe.zrem(&key, id);
    }
    let _: redis::RedisResult<()> = pipe.query_async(redis).await;
}

/// Remove every home-feed entry authored by an account on `domain`, used when a
/// user blocks a domain (Mastodon AfterBlockDomainFromAccountService clears the
/// blocker's timelines of that domain's content).
pub async fn unmerge_domain_from_home(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    domain: &str,
    into_account_id: i64,
) {
    if !is_feed_populated(redis, keys, into_account_id).await {
        return;
    }
    let key = feed_key(keys, into_account_id);
    let members: Vec<i64> = redis
        .zrange::<_, Vec<i64>>(&key, 0, -1)
        .await
        .unwrap_or_default();
    if members.is_empty() {
        return;
    }
    let ids: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT s.id FROM statuses s
           JOIN accounts a ON a.id = s.account_id
           WHERE s.id = ANY($1::bigint[]) AND a.domain = $2"#,
        &members,
        domain,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    if ids.is_empty() {
        return;
    }
    let mut pipe = redis::pipe();
    for id in ids {
        pipe.zrem(&key, id);
    }
    let _: redis::RedisResult<()> = pipe.query_async(redis).await;
}
