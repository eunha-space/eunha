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

// ── Reblog aggregation ────────────────────────────────────────────────────

/// `FeedManager::REBLOG_FALLOFF`: a boost of a post already among this many
/// of the newest in a feed, or boosted by something that is, is not added
/// again.
pub const REBLOG_FALLOFF: usize = 80;

/// A feed and the keys `FeedManager` tracks its boosts under:
/// `<feed>:reblogs`, the boosted posts with a boost in the feed scored by
/// that boost, and `<feed>:reblogs:<id>`, the other boosts of one held back.
struct Timeline {
    key: String,
    reblogs: String,
}

impl Timeline {
    fn home(keys: &RedisKeyspace, account_id: i64) -> Self {
        Self {
            key: feed_key(keys, account_id),
            reblogs: keys.key(format!("feed:home:{account_id}:reblogs")),
        }
    }

    fn list(keys: &RedisKeyspace, list_id: i64) -> Self {
        Self {
            key: list_feed_key(keys, list_id),
            reblogs: keys.key(format!("feed:list:{list_id}:reblogs")),
        }
    }

    fn reblog_set(&self, reblog_of_id: i64) -> String {
        format!("{}:{reblog_of_id}", self.reblogs)
    }
}

/// `User#aggregates_reblogs?`: the `aggregate_reblogs` setting, on unless the
/// user turned it off.
pub fn aggregates_reblogs(settings: Option<&str>) -> bool {
    crate::accounts::user_setting_bool(settings, "aggregate_reblogs", true)
}

/// [`aggregates_reblogs`] for each of `account_ids` that has a user who
/// turned it off.
async fn not_aggregating(db: &PgPool, account_ids: &[i64]) -> std::collections::HashSet<i64> {
    sqlx::query!(
        "SELECT account_id, settings FROM users WHERE account_id = ANY($1)",
        account_ids,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .filter(|u| !aggregates_reblogs(u.settings.as_deref()))
    .map(|u| u.account_id)
    .collect()
}

/// The `reblog_of_id` of each of `ids` that is a boost.
async fn reblogs_of(db: &PgPool, ids: &[i64]) -> std::collections::HashMap<i64, i64> {
    sqlx::query!(
        r#"SELECT id, reblog_of_id AS "reblog_of_id!" FROM statuses
           WHERE id = ANY($1) AND reblog_of_id IS NOT NULL"#,
        ids,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|s| (s.id, s.reblog_of_id))
    .collect()
}

/// `FeedManager#add_to_feed`: whether the status went in. A boost, when the
/// user aggregates them, stays out if the post it boosts is among the
/// [`REBLOG_FALLOFF`] newest, or another boost of it is; the latter is kept
/// aside in case that boost is deleted. A post stays out if a boost of it
/// already went in.
async fn add_to_feed(
    redis: &mut ConnectionManager,
    timeline: &Timeline,
    status_id: i64,
    reblog_of_id: Option<i64>,
    aggregate: bool,
) -> redis::RedisResult<bool> {
    match reblog_of_id.filter(|_| aggregate) {
        Some(reblog_of_id) => {
            let rank: Option<usize> = redis::cmd("ZREVRANK")
                .arg(&timeline.key)
                .arg(reblog_of_id)
                .query_async(redis)
                .await?;
            if rank.is_some_and(|rank| rank < REBLOG_FALLOFF) {
                return Ok(false);
            }
            let tracked: i64 = redis::cmd("ZADD")
                .arg(&timeline.reblogs)
                .arg("NX")
                .arg(status_id as f64)
                .arg(reblog_of_id)
                .query_async(redis)
                .await?;
            if tracked == 1 {
                redis::pipe()
                    .zadd(&timeline.key, status_id, status_id as f64)
                    .ignore()
                    .expire(&timeline.reblogs, FEED_TTL_SECS as i64)
                    .ignore()
                    .query_async::<()>(redis)
                    .await?;
                Ok(true)
            } else {
                let set = timeline.reblog_set(reblog_of_id);
                redis::pipe()
                    .sadd(&set, status_id)
                    .ignore()
                    .expire(&set, FEED_TTL_SECS as i64)
                    .ignore()
                    .query_async::<()>(redis)
                    .await?;
                Ok(false)
            }
        }
        None => {
            // A boost may arrive before the post it boosts; then the post
            // stays out.
            let boosted: Option<f64> = redis::cmd("ZSCORE")
                .arg(&timeline.reblogs)
                .arg(status_id)
                .query_async(redis)
                .await?;
            if boosted.is_some() {
                return Ok(false);
            }
            redis
                .zadd::<_, _, _, ()>(&timeline.key, status_id, status_id as f64)
                .await?;
            Ok(true)
        }
    }
}

/// `FeedManager#remove_from_feed`: take the status out, and when it was a
/// boost standing in for others, put the oldest boost held back in its place.
async fn remove_from_feed(
    redis: &mut ConnectionManager,
    timeline: &Timeline,
    status_id: i64,
    reblog_of_id: Option<i64>,
    aggregate: bool,
) -> redis::RedisResult<()> {
    match reblog_of_id.filter(|_| aggregate) {
        Some(reblog_of_id) => {
            let rank: Option<usize> = redis::cmd("ZREVRANK")
                .arg(&timeline.key)
                .arg(status_id)
                .query_async(redis)
                .await?;
            if rank.is_none() {
                return Ok(());
            }
            let set = timeline.reblog_set(reblog_of_id);
            let (others,): (Vec<i64>,) = redis::pipe()
                .srem(&set, status_id)
                .ignore()
                .zrem(&timeline.reblogs, reblog_of_id)
                .ignore()
                .smembers(&set)
                .query_async(redis)
                .await?;
            let mut pipe = redis::pipe();
            if let Some(other) = others.into_iter().min() {
                pipe.zadd(&timeline.key, other, other as f64)
                    .ignore()
                    .zadd(&timeline.reblogs, reblog_of_id, other as f64)
                    .ignore();
            }
            pipe.zrem(&timeline.key, status_id)
                .ignore()
                .query_async::<()>(redis)
                .await
        }
        None => {
            redis::pipe()
                .del(timeline.reblog_set(status_id))
                .ignore()
                .zrem(&timeline.reblogs, status_id)
                .ignore()
                .zrem(&timeline.key, status_id)
                .ignore()
                .query_async::<()>(redis)
                .await
        }
    }
}

/// `FeedManager#trim`: keep the newest [`FEED_MAX_ITEMS`], and stop tracking
/// boosts older than the [`REBLOG_FALLOFF`]th entry, dropping the boosts held
/// back for them.
async fn trim(redis: &mut ConnectionManager, timeline: &Timeline) -> redis::RedisResult<()> {
    let (falloff,): (Vec<(i64, f64)>,) = redis::pipe()
        .zremrangebyrank(&timeline.key, 0, -(FEED_MAX_ITEMS + 1))
        .ignore()
        .cmd("ZREVRANGEBYSCORE")
        .arg(&timeline.key)
        .arg("+inf")
        .arg("-inf")
        .arg("WITHSCORES")
        .arg("LIMIT")
        .arg(REBLOG_FALLOFF)
        .arg(1)
        .query_async(redis)
        .await?;
    let Some((_, falloff_score)) = falloff.first() else {
        return Ok(());
    };
    let stale: Vec<i64> = redis::cmd("ZRANGEBYSCORE")
        .arg(&timeline.reblogs)
        .arg(0)
        .arg(*falloff_score)
        .query_async(redis)
        .await?;
    if stale.is_empty() {
        return Ok(());
    }
    let mut pipe = redis::pipe();
    for reblog_of_id in stale {
        pipe.zrem(&timeline.reblogs, reblog_of_id)
            .ignore()
            .del(timeline.reblog_set(reblog_of_id))
            .ignore();
    }
    pipe.query_async(redis).await
}

/// `add_to_feed` then `trim`, as `push_to_home` and `push_to_list` do:
/// whether the status went in.
async fn push(
    redis: &mut ConnectionManager,
    timeline: &Timeline,
    status_id: i64,
    reblog_of_id: Option<i64>,
    aggregate: bool,
) -> bool {
    let pushed = async {
        let added = add_to_feed(redis, timeline, status_id, reblog_of_id, aggregate).await?;
        if added {
            trim(redis, timeline).await?;
        }
        redis::RedisResult::Ok(added)
    }
    .await;
    match pushed {
        Ok(added) => added,
        Err(error) => {
            tracing::warn!(%error, key = %timeline.key, "could not add a status to a feed");
            false
        }
    }
}

/// What `FeedManager#add_to_feed` answered for a status in each home feed
/// (by account) and list feed (by list) the fan-out delivered it to, which is
/// what decides whether `push_to_home` and `push_to_list` stream it. A feed
/// Redis does not hold yet, which eunha builds when it is first read, answers
/// as `add_to_feed` answers on an empty one: the status goes in.
#[derive(Debug, Default, Clone)]
pub struct Pushed {
    pub homes: std::collections::HashMap<i64, bool>,
    pub lists: std::collections::HashMap<i64, bool>,
}

impl Pushed {
    /// Whether the status went into `account_id`'s home feed.
    pub fn home(&self, account_id: i64) -> bool {
        self.homes.get(&account_id).copied().unwrap_or(false)
    }

    /// Whether it went into the list's feed.
    pub fn list(&self, list_id: i64) -> bool {
        self.lists.get(&list_id).copied().unwrap_or(false)
    }
}

/// `FanOutOnWriteService` for a status already stored, its fan-out read from
/// its row: [`fanout_new_status`] and [`fanout_to_lists`], as
/// `DistributionWorker` runs them for an update too.
pub async fn fanout_status(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    status_id: i64,
) -> Pushed {
    let Ok(Some(row)) = sqlx::query!(
        r#"SELECT account_id, in_reply_to_account_id, visibility,
                  ARRAY(SELECT tag_id FROM statuses_tags WHERE status_id = s.id) AS "tag_ids!"
           FROM statuses s WHERE id = $1"#,
        status_id,
    )
    .fetch_optional(db)
    .await
    else {
        return Pushed::default();
    };
    let homes = fanout_new_status(redis, keys, db, row.account_id, status_id, &row.tag_ids).await;
    let lists = fanout_to_lists(
        redis,
        keys,
        db,
        row.account_id,
        status_id,
        row.in_reply_to_account_id,
        crate::db::models::vis::to_str(row.visibility),
    )
    .await;
    Pushed { homes, lists }
}

/// `FeedManager#merge_into_home` and `#merge_into_list`: [`add_to_feed`] for
/// each of `newest_first`, oldest first, then one [`trim`].
async fn merge(
    redis: &mut ConnectionManager,
    db: &PgPool,
    timeline: &Timeline,
    newest_first: &[i64],
    aggregate: bool,
) {
    let reblogs = reblogs_of(db, newest_first).await;
    let merged = async {
        for &id in newest_first.iter().rev() {
            add_to_feed(redis, timeline, id, reblogs.get(&id).copied(), aggregate).await?;
        }
        trim(redis, timeline).await
    }
    .await;
    if let Err(error) = merged {
        tracing::warn!(%error, key = %timeline.key, "could not merge statuses into a feed");
    }
}

/// What [`add_to_feed`] and [`trim`], run over `candidates` oldest first on an
/// empty feed, leave: the feed, the boosts tracked (`(boosted, boost)`), and
/// the boosts held back. How a feed is filled from the database.
#[derive(Debug, Default, PartialEq)]
pub struct Aggregated {
    pub feed: Vec<i64>,
    pub tracked: Vec<(i64, i64)>,
    pub held_back: Vec<(i64, i64)>,
}

/// [`Aggregated`] for `candidates`, each a status and what it boosts, oldest
/// first.
pub fn aggregate(candidates: &[(i64, Option<i64>)], aggregate: bool) -> Aggregated {
    use std::collections::{BTreeMap, HashMap};
    let mut feed: Vec<i64> = Vec::new();
    let mut position: HashMap<i64, usize> = HashMap::new();
    let mut tracked: BTreeMap<i64, i64> = BTreeMap::new();
    let mut held_back: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for &(id, reblog_of_id) in candidates {
        match reblog_of_id.filter(|_| aggregate) {
            Some(boosted) => {
                let rank = position.get(&boosted).map(|&p| feed.len() - 1 - p);
                if rank.is_some_and(|rank| rank < REBLOG_FALLOFF) {
                    continue;
                }
                if tracked.contains_key(&boosted) {
                    held_back.entry(boosted).or_default().push(id);
                    continue;
                }
                tracked.insert(boosted, id);
            }
            None => {
                if tracked.contains_key(&id) {
                    continue;
                }
            }
        }
        position.insert(id, feed.len());
        feed.push(id);
        // `trim`
        if feed.len() > REBLOG_FALLOFF {
            let falloff = feed[feed.len() - 1 - REBLOG_FALLOFF];
            tracked.retain(|boosted, boost| {
                let keep = *boost > falloff;
                if !keep {
                    held_back.remove(boosted);
                }
                keep
            });
        }
    }
    let keep_from = feed.len().saturating_sub(FEED_MAX_ITEMS as usize);
    Aggregated {
        feed: feed.split_off(keep_from),
        tracked: tracked.into_iter().collect(),
        held_back: held_back
            .into_iter()
            .flat_map(|(boosted, boosts)| boosts.into_iter().map(move |b| (boosted, b)))
            .collect(),
    }
}

/// Write `candidates` (newest first, as the database lists them) into an
/// emptied feed, aggregated as [`aggregate`] says.
async fn fill(
    redis: &mut ConnectionManager,
    db: &PgPool,
    timeline: &Timeline,
    newest_first: &[i64],
    aggregate_reblogs: bool,
) {
    let reblogs = reblogs_of(db, newest_first).await;
    let candidates: Vec<(i64, Option<i64>)> = newest_first
        .iter()
        .rev()
        .map(|id| (*id, reblogs.get(id).copied()))
        .collect();
    let plan = aggregate(&candidates, aggregate_reblogs);
    let mut pipe = redis::pipe();
    for &id in &plan.feed {
        pipe.zadd(&timeline.key, id, id as f64).ignore();
    }
    pipe.expire(&timeline.key, FEED_TTL_SECS as i64).ignore();
    for &(boosted, boost) in &plan.tracked {
        pipe.zadd(&timeline.reblogs, boosted, boost as f64).ignore();
    }
    pipe.expire(&timeline.reblogs, FEED_TTL_SECS as i64)
        .ignore();
    for &(boosted, boost) in &plan.held_back {
        let set = timeline.reblog_set(boosted);
        pipe.sadd(&set, boost)
            .ignore()
            .expire(&set, FEED_TTL_SECS as i64)
            .ignore();
    }
    let _: redis::RedisResult<()> = pipe.query_async(redis).await;
}

/// `FeedManager#clean_feeds!` for one feed's boost tracking: the tracked set
/// and every set of boosts held back.
async fn clean_reblogs(redis: &mut ConnectionManager, timeline: &Timeline) {
    let tracked: Vec<i64> = redis
        .zrange(&timeline.reblogs, 0, -1)
        .await
        .unwrap_or_default();
    let mut pipe = redis::pipe();
    pipe.del(&timeline.reblogs).ignore();
    for boosted in tracked {
        pipe.del(timeline.reblog_set(boosted)).ignore();
    }
    let _: redis::RedisResult<()> = pipe.query_async(redis).await;
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

/// What `FeedManager#remove_from_feed` will answer for the status, asked
/// before the feed lets go of it: whether the home feed holds it. A feed
/// Redis does not hold yet took everything the fan-out offered it (see
/// [`Pushed`]), so it answers that it holds the status.
pub async fn home_holds(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    account_id: i64,
    status_id: i64,
) -> bool {
    if !is_feed_populated(redis, keys, account_id).await {
        return true;
    }
    redis
        .zscore::<_, _, Option<f64>>(feed_key(keys, account_id), status_id)
        .await
        .ok()
        .flatten()
        .is_some()
}

/// [`home_holds`] for a list's feed.
pub async fn list_holds(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    list_id: i64,
    status_id: i64,
) -> bool {
    if !is_list_feed_populated(redis, keys, list_id).await {
        return true;
    }
    redis
        .zscore::<_, _, Option<f64>>(list_feed_key(keys, list_id), status_id)
        .await
        .ok()
        .flatten()
        .is_some()
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

    let timeline = Timeline::home(keys, account_id);
    clean_reblogs(redis, &timeline).await;
    if !status_ids.is_empty() {
        let aggregate = !not_aggregating(db, &[account_id])
            .await
            .contains(&account_id);
        fill(redis, db, &timeline, &status_ids, aggregate).await;
    }
}

/// Fan-out a newly posted status to all followers' initialized feeds,
/// plus accounts following any of the status's hashtags: for each of them,
/// whether it went in ([`Pushed::homes`]).
pub async fn fanout_new_status(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    author_id: i64,
    status_id: i64,
    tag_ids: &[i64],
) -> std::collections::HashMap<i64, bool> {
    // Look up the status's reply shape and language so it is only fanned to
    // followers who should see it (Mastodon FeedManager#filter_from_home).
    let reply_meta = sqlx::query!(
        "SELECT reply, in_reply_to_account_id, language, reblog_of_id FROM statuses WHERE id = $1",
        status_id,
    )
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    let is_reply = reply_meta.as_ref().map(|m| m.reply).unwrap_or(false);
    let reply_to = reply_meta.as_ref().and_then(|m| m.in_reply_to_account_id);
    let language = reply_meta.as_ref().and_then(|m| m.language.clone());
    let reblog_of_id = reply_meta.as_ref().and_then(|m| m.reblog_of_id);

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

    let mut pushed = std::collections::HashMap::new();
    if recipients.is_empty() {
        return pushed;
    }

    let pop_keys: Vec<String> = recipients
        .iter()
        .map(|&id| populated_key(keys, id))
        .collect();
    let initialized: Vec<Option<i64>> = match redis.mget(&pop_keys).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("fanout mget error: {}", e);
            return pushed;
        }
    };

    let mut ready = Vec::new();
    for (&id, init) in recipients.iter().zip(initialized.iter()) {
        if init.is_some() {
            ready.push(id);
        } else {
            // Built from the database when first read; `add_to_feed` on an
            // empty feed takes the status.
            pushed.insert(id, true);
        }
    }
    let separate = if reblog_of_id.is_some() {
        not_aggregating(db, &ready).await
    } else {
        Default::default()
    };
    for id in ready {
        let timeline = Timeline::home(keys, id);
        let added = push(
            redis,
            &timeline,
            status_id,
            reblog_of_id,
            !separate.contains(&id),
        )
        .await;
        pushed.insert(id, added);
    }
    pushed
}

/// Remove a deleted status from all followers' initialized feeds. The status
/// row must still be there to say whether it was a boost; for one already
/// gone, use [`fanout_remove_boost`].
pub async fn fanout_remove_status(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    author_id: i64,
    status_id: i64,
) {
    let reblog_of_id = reblogs_of(db, &[status_id]).await.get(&status_id).copied();
    fanout_remove_boost(redis, keys, db, author_id, status_id, reblog_of_id).await;
}

/// Take a boost already deleted from the database out of its author's
/// followers' feeds and lists, putting back another boost of the same post it
/// held back. Runs inline under [`sync_fanout`], otherwise in a task.
pub async fn unpush_boost(
    state: &crate::state::AppState,
    author_id: i64,
    boost_id: i64,
    reblog_of_id: i64,
) {
    let mut redis = state.redis.clone();
    let keys = state.redis_keys.clone();
    let db = state.db.clone();
    let work = async move {
        fanout_remove_boost(
            &mut redis,
            &keys,
            &db,
            author_id,
            boost_id,
            Some(reblog_of_id),
        )
        .await;
        fanout_remove_boost_from_lists(
            &mut redis,
            &keys,
            &db,
            author_id,
            boost_id,
            Some(reblog_of_id),
        )
        .await;
    };
    if sync_fanout() {
        work.await;
    } else {
        crate::tenants::spawn(work);
    }
}

/// `FeedManager#unpush_from_home` for every follower of `author_id`:
/// [`fanout_remove_status`] for a status that boosted `reblog_of_id`.
pub async fn fanout_remove_boost(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    author_id: i64,
    status_id: i64,
    reblog_of_id: Option<i64>,
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

    let ready: Vec<i64> = recipients
        .iter()
        .zip(initialized.iter())
        .filter(|(_, init)| init.is_some())
        .map(|(&id, _)| id)
        .collect();
    let separate = if reblog_of_id.is_some() {
        not_aggregating(db, &ready).await
    } else {
        Default::default()
    };
    for id in ready {
        let timeline = Timeline::home(keys, id);
        let aggregate = !separate.contains(&id);
        if let Err(error) =
            remove_from_feed(redis, &timeline, status_id, reblog_of_id, aggregate).await
        {
            tracing::warn!(%error, account_id = id, "could not remove a status from a home feed");
        }
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

    let timeline = Timeline::list(keys, list_id);
    clean_reblogs(redis, &timeline).await;
    if !status_ids.is_empty() {
        let aggregate = !not_aggregating(db, &[owner_id]).await.contains(&owner_id);
        fill(redis, db, &timeline, &status_ids, aggregate).await;
    }
}

/// Fan out a newly posted status to all initialized list feeds that contain
/// the author: for each list it passes, whether it went in
/// ([`Pushed::lists`]).
pub async fn fanout_to_lists(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    author_id: i64,
    status_id: i64,
    in_reply_to_account_id: Option<i64>,
    visibility: &str,
) -> std::collections::HashMap<i64, bool> {
    let mut pushed = std::collections::HashMap::new();
    if visibility == "direct" {
        return pushed;
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
        return pushed;
    }

    let pop_keys: Vec<String> = lists
        .iter()
        .map(|l| list_populated_key(keys, l.id))
        .collect();
    let initialized: Vec<Option<i64>> = match redis.mget(&pop_keys).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("fanout_to_lists mget error: {}", e);
            return pushed;
        }
    };

    let reblog_of_id = reblogs_of(db, &[status_id]).await.get(&status_id).copied();
    let owners: Vec<i64> = lists.iter().map(|l| l.account_id).collect();
    let separate = if reblog_of_id.is_some() {
        not_aggregating(db, &owners).await
    } else {
        Default::default()
    };

    for (list, init) in lists.iter().zip(initialized.iter()) {
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

        if !passes {
            continue;
        }
        if init.is_none() {
            pushed.insert(list.id, true);
            continue;
        }
        let timeline = Timeline::list(keys, list.id);
        let aggregate = !separate.contains(&list.account_id);
        let added = push(redis, &timeline, status_id, reblog_of_id, aggregate).await;
        pushed.insert(list.id, added);
    }
    pushed
}

/// Remove a deleted status from all initialized list feeds that contain the
/// author. As with [`fanout_remove_status`], the row must still be there; for
/// one already gone, use [`fanout_remove_boost_from_lists`].
pub async fn fanout_remove_from_lists(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    author_id: i64,
    status_id: i64,
) {
    let reblog_of_id = reblogs_of(db, &[status_id]).await.get(&status_id).copied();
    fanout_remove_boost_from_lists(redis, keys, db, author_id, status_id, reblog_of_id).await;
}

/// `FeedManager#unpush_from_list` for every list holding `author_id`.
pub async fn fanout_remove_boost_from_lists(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    author_id: i64,
    status_id: i64,
    reblog_of_id: Option<i64>,
) {
    let lists = sqlx::query!(
        "SELECT l.id, l.account_id FROM lists l JOIN list_accounts la ON la.list_id = l.id WHERE la.account_id = $1",
        author_id,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let list_ids: Vec<i64> = lists.iter().map(|l| l.id).collect();

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

    let owners: Vec<i64> = lists.iter().map(|l| l.account_id).collect();
    let separate = if reblog_of_id.is_some() {
        not_aggregating(db, &owners).await
    } else {
        Default::default()
    };
    for (list, init) in lists.iter().zip(initialized.iter()) {
        if init.is_none() {
            continue;
        }
        let timeline = Timeline::list(keys, list.id);
        let aggregate = !separate.contains(&list.account_id);
        if let Err(error) =
            remove_from_feed(redis, &timeline, status_id, reblog_of_id, aggregate).await
        {
            tracing::warn!(%error, list_id = list.id, "could not remove a status from a list feed");
        }
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

    let aggregate = !not_aggregating(db, &[owner_id]).await.contains(&owner_id);
    merge(
        redis,
        db,
        &Timeline::list(keys, list_id),
        &recent,
        aggregate,
    )
    .await;
}

/// Delete an account's home feed keys (Mastodon's `FeedManager#clean_feeds!`,
/// called when the account is deleted).
pub async fn delete_home_feed(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    account_id: i64,
) {
    clean_reblogs(redis, &Timeline::home(keys, account_id)).await;
    let _: redis::RedisResult<()> = redis::pipe()
        .del(feed_key(keys, account_id))
        .del(populated_key(keys, account_id))
        .query_async(redis)
        .await;
}

/// Delete a list's Redis feed keys (called when the list itself is deleted).
pub async fn delete_list_feed(redis: &mut ConnectionManager, keys: &RedisKeyspace, list_id: i64) {
    clean_reblogs(redis, &Timeline::list(keys, list_id)).await;
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

    let aggregate = !not_aggregating(db, &[follower_id])
        .await
        .contains(&follower_id);
    merge(
        redis,
        db,
        &Timeline::home(keys, follower_id),
        &recent,
        aggregate,
    )
    .await;
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

    unmerge(
        redis,
        db,
        into_account_id,
        &Timeline::home(keys, into_account_id),
        &ids,
    )
    .await;
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
    unmerge(
        redis,
        db,
        into_account_id,
        &Timeline::home(keys, into_account_id),
        &ids,
    )
    .await;
}

/// [`remove_from_feed`] for each of `ids`, as `unmerge_from_home` runs it.
async fn unmerge(
    redis: &mut ConnectionManager,
    db: &PgPool,
    owner_id: i64,
    timeline: &Timeline,
    ids: &[i64],
) {
    if ids.is_empty() {
        return;
    }
    let reblogs = reblogs_of(db, ids).await;
    let aggregate = !not_aggregating(db, &[owner_id]).await.contains(&owner_id);
    for &id in ids {
        let reblog_of_id = reblogs.get(&id).copied();
        if let Err(error) = remove_from_feed(redis, timeline, id, reblog_of_id, aggregate).await {
            tracing::warn!(%error, key = %timeline.key, "could not remove a status from a feed");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{aggregate, Aggregated, REBLOG_FALLOFF};

    #[test]
    fn a_second_boost_is_held_back() {
        // 1 is a post from someone not followed; 10 and 11 boost it.
        let plan = aggregate(&[(10, Some(1)), (11, Some(1)), (12, None)], true);
        assert_eq!(
            plan,
            Aggregated {
                feed: vec![10, 12],
                tracked: vec![(1, 10)],
                held_back: vec![(1, 11)],
            }
        );
        // Not aggregating, every boost goes in.
        assert_eq!(
            aggregate(&[(10, Some(1)), (11, Some(1))], false).feed,
            vec![10, 11]
        );
    }

    #[test]
    fn a_boost_of_a_recent_post_stays_out() {
        let plan = aggregate(&[(1, None), (10, Some(1))], true);
        assert_eq!(plan.feed, vec![1]);
        assert!(plan.tracked.is_empty());
    }

    #[test]
    fn tracking_falls_off_after_eighty_entries() {
        let mut candidates = vec![(1000, None), (1001, Some(1000))];
        candidates.extend((0..REBLOG_FALLOFF as i64).map(|i| (2000 + i, None)));
        // The post is now further down than the falloff: a new boost goes in.
        candidates.push((3000, Some(1000)));
        let plan = aggregate(&candidates, true);
        assert_eq!(plan.feed.last(), Some(&3000));
        assert_eq!(plan.tracked, vec![(1000, 3000)]);
        // A boost of something tracked within the falloff is held back.
        let mut candidates = vec![(10, Some(1))];
        candidates.extend((0..(REBLOG_FALLOFF as i64 - 1)).map(|i| (100 + i, None)));
        candidates.push((500, Some(1)));
        let plan = aggregate(&candidates, true);
        assert_eq!(plan.held_back, vec![(1, 500)]);
    }
}
