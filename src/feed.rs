//! Mastodon's `FeedManager`: the home and list feeds kept in Redis.
//!
//! The keys are Mastodon's, under the instance's key prefix, so that a feed
//! one process built is the feed the other reads:
//!
//!  -  `feed:home:<account id>` and `feed:list:<list id>`, sorted sets of
//!     status ids scored by the id;
//!  -  `<feed>:reblogs`, the boosted posts with a boost in the feed, scored by
//!     that boost;
//!  -  `<feed>:reblogs:<boosted id>`, a set of the other boosts of one post
//!     held back while the first is in the feed.
//!
//! A feed exists only for a user who signed in recently
//! (`User.signed_in_recently`): the fan-out and the merges skip everyone
//! else, the daily vacuum removes their feeds, and the feed is rebuilt only
//! by `RegenerationWorker` ([`crate::home_feed`]), which `User#regenerate_feed!`
//! queues for a returning user. An empty feed is an empty feed: reading one
//! never fills it.

use crate::redis_keys::RedisKeyspace;
use redis::{aio::ConnectionManager, AsyncCommands};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

/// `FeedManager::MAX_ITEMS`.
const FEED_MAX_ITEMS: isize = 800;

// When true, fanout/populate/merge run inline (no task, no queued job).
// Set by integration tests to eliminate timing races.
static SYNC_FANOUT: AtomicBool = AtomicBool::new(false);

pub fn enable_sync_fanout() {
    SYNC_FANOUT.store(true, Ordering::Relaxed);
}

pub fn sync_fanout() -> bool {
    SYNC_FANOUT.load(Ordering::Relaxed)
}

fn feed_key(keys: &RedisKeyspace, account_id: i64) -> String {
    keys.key(format!("feed:home:{account_id}"))
}

fn list_feed_key(keys: &RedisKeyspace, list_id: i64) -> String {
    keys.key(format!("feed:list:{list_id}"))
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
async fn not_aggregating(db: &PgPool, account_ids: &[i64]) -> HashSet<i64> {
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

/// `account.user&.aggregates_reblogs?` for one account.
async fn aggregates(db: &PgPool, account_id: i64) -> bool {
    !not_aggregating(db, &[account_id])
        .await
        .contains(&account_id)
}

/// The `reblog_of_id` of each of `ids` that is a boost.
async fn reblogs_of(db: &PgPool, ids: &[i64]) -> HashMap<i64, i64> {
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
                redis
                    .zadd::<_, _, _, ()>(&timeline.key, status_id, status_id as f64)
                    .await?;
                Ok(true)
            } else {
                let set = timeline.reblog_set(reblog_of_id);
                redis.sadd::<_, _, ()>(&set, status_id).await?;
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

/// `redis.zcard(timeline_key)`.
async fn timeline_size(redis: &mut ConnectionManager, timeline: &Timeline) -> i64 {
    redis.zcard(&timeline.key).await.unwrap_or(0)
}

/// `redis.zrange(timeline_key, 0, 0, with_scores: true).first.last.to_i`:
/// the score of the oldest entry, as Mastodon reads it (a float, truncated).
async fn oldest_score(redis: &mut ConnectionManager, timeline: &Timeline) -> Option<i64> {
    let oldest: Vec<(i64, f64)> = redis
        .zrange_withscores(&timeline.key, 0, 0)
        .await
        .unwrap_or_default();
    oldest.first().map(|(_, score)| *score as i64)
}

/// `Mastodon::Snowflake.id_at(timestamp, with_random: false)`; a missing
/// timestamp is nought, as `nil.to_i` is.
fn id_at(timestamp: Option<chrono::NaiveDateTime>) -> i64 {
    (timestamp.map_or(0, |t| t.and_utc().timestamp()) * 1000) << 16
}

// ── Filtering (`FeedManager#filter_from_home`) ────────────────────────────

/// A status as `FeedManager#filter_from_home` reads it.
struct Candidate {
    id: i64,
    account_id: i64,
    reply: bool,
    in_reply_to_id: Option<i64>,
    in_reply_to_account_id: Option<i64>,
    language: Option<String>,
    reblog_of_id: Option<i64>,
    /// The boosted status, when it is kept: its author and their domain.
    reblog: Option<(i64, Option<String>)>,
}

/// Which feed a status is being filtered for: the home feed, or a list (its
/// id and `replies_policy`).
#[derive(Clone, Copy)]
enum Receiver {
    Home,
    List { id: i64, replies_policy: i32 },
}

/// `account.statuses.list_eligible_visibility.includes(reblog: :account)`,
/// newest first: `limit` of `account_id`'s kept public, unlisted and private
/// statuses, those newer than `after` when it is given.
async fn eligible_statuses(
    db: &PgPool,
    account_id: i64,
    after: Option<i64>,
    limit: i64,
) -> Vec<Candidate> {
    sqlx::query!(
        r#"SELECT s.id, s.account_id, s.reply, s.in_reply_to_id, s.in_reply_to_account_id,
                  s.language, s.reblog_of_id,
                  r.account_id AS "reblog_account_id?", ra.domain AS "reblog_domain?"
           FROM statuses s
           LEFT JOIN statuses r ON r.id = s.reblog_of_id AND r.deleted_at IS NULL
           LEFT JOIN accounts ra ON ra.id = r.account_id
           WHERE s.account_id = $1
             AND s.deleted_at IS NULL
             AND s.visibility IN (0, 1, 2)
             AND ($2::bigint IS NULL OR s.id > $2)
           ORDER BY s.id DESC
           LIMIT $3"#,
        account_id,
        after,
        limit,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|s| Candidate {
        id: s.id,
        account_id: s.account_id,
        reply: s.reply,
        in_reply_to_id: s.in_reply_to_id,
        in_reply_to_account_id: s.in_reply_to_account_id,
        language: s.language,
        reblog_of_id: s.reblog_of_id,
        reblog: s.reblog_account_id.map(|id| (id, s.reblog_domain)),
    })
    .collect()
}

/// `FeedManager#build_crutches`: what [`filter_from_home`] needs to know
/// about the receiver and the accounts of `statuses`, read at once.
#[derive(Default)]
struct Crutches {
    active_mentions: HashMap<i64, Vec<i64>>,
    following: HashSet<i64>,
    /// Only the follows that name languages.
    languages: HashMap<i64, Vec<String>>,
    hiding_reblogs: HashSet<i64>,
    blocking: HashSet<i64>,
    muting: HashSet<i64>,
    domain_blocking: HashSet<String>,
    blocked_by: HashSet<i64>,
    exclusive_list_users: HashSet<i64>,
}

impl Crutches {
    async fn build(
        db: &PgPool,
        receiver_id: i64,
        statuses: &[Candidate],
        receiver: Receiver,
    ) -> sqlx::Result<Self> {
        let mentioned_ids: Vec<i64> = statuses
            .iter()
            .flat_map(|s| std::iter::once(s.id).chain(s.reblog_of_id))
            .collect();
        let mut active_mentions: HashMap<i64, Vec<i64>> = HashMap::new();
        for m in sqlx::query!(
            "SELECT status_id, account_id FROM mentions WHERE status_id = ANY($1) AND NOT silent",
            &mentioned_ids,
        )
        .fetch_all(db)
        .await?
        {
            active_mentions
                .entry(m.status_id)
                .or_default()
                .push(m.account_id);
        }

        let mut check_for_blocks: Vec<i64> = Vec::new();
        for s in statuses {
            check_for_blocks.extend(active_mentions.get(&s.id).into_iter().flatten());
            check_for_blocks.push(s.account_id);
            if let (Some(reblog_of_id), Some((reblog_account_id, _))) = (s.reblog_of_id, &s.reblog)
            {
                check_for_blocks.push(*reblog_account_id);
                check_for_blocks.extend(active_mentions.get(&reblog_of_id).into_iter().flatten());
            }
        }
        let account_ids: Vec<i64> = statuses.iter().map(|s| s.account_id).collect();
        let reblogger_ids: Vec<i64> = statuses
            .iter()
            .filter(|s| s.reblog_of_id.is_some())
            .map(|s| s.account_id)
            .collect();
        let in_reply_to_ids: Vec<i64> = statuses
            .iter()
            .filter_map(|s| s.in_reply_to_account_id)
            .collect();
        let domains: Vec<String> = statuses
            .iter()
            .filter_map(|s| s.reblog.as_ref().and_then(|(_, d)| d.clone()))
            .collect();
        let authors_and_boosted: Vec<i64> = statuses
            .iter()
            .flat_map(|s| std::iter::once(s.account_id).chain(s.reblog.as_ref().map(|r| r.0)))
            .collect();

        let following: HashSet<i64> = match receiver {
            Receiver::Home
            | Receiver::List {
                replies_policy: crate::db::models::replies::FOLLOWED,
                ..
            } => sqlx::query_scalar!(
                "SELECT target_account_id FROM follows
                 WHERE account_id = $1 AND target_account_id = ANY($2)",
                receiver_id,
                &in_reply_to_ids,
            )
            .fetch_all(db)
            .await?
            .into_iter()
            .collect(),
            Receiver::List {
                id,
                replies_policy: crate::db::models::replies::LIST,
            } => sqlx::query_scalar!(
                "SELECT account_id FROM list_accounts WHERE list_id = $1 AND account_id = ANY($2)",
                id,
                &in_reply_to_ids,
            )
            .fetch_all(db)
            .await?
            .into_iter()
            .collect(),
            Receiver::List { .. } => HashSet::new(),
        };

        let languages = sqlx::query!(
            "SELECT target_account_id, languages FROM follows
             WHERE account_id = $1 AND target_account_id = ANY($2)",
            receiver_id,
            &account_ids,
        )
        .fetch_all(db)
        .await?
        .into_iter()
        .filter_map(|f| {
            f.languages
                .filter(|l| !l.is_empty())
                .map(|l| (f.target_account_id, l))
        })
        .collect();

        let hiding_reblogs = sqlx::query_scalar!(
            "SELECT target_account_id FROM follows
             WHERE account_id = $1 AND target_account_id = ANY($2) AND NOT show_reblogs",
            receiver_id,
            &reblogger_ids,
        )
        .fetch_all(db)
        .await?
        .into_iter()
        .collect();

        let blocking = sqlx::query_scalar!(
            "SELECT target_account_id FROM blocks
             WHERE account_id = $1 AND target_account_id = ANY($2)",
            receiver_id,
            &check_for_blocks,
        )
        .fetch_all(db)
        .await?
        .into_iter()
        .collect();

        let muting = sqlx::query_scalar!(
            "SELECT target_account_id FROM mutes
             WHERE account_id = $1 AND target_account_id = ANY($2)",
            receiver_id,
            &check_for_blocks,
        )
        .fetch_all(db)
        .await?
        .into_iter()
        .collect();

        let domain_blocking = sqlx::query_scalar!(
            "SELECT domain FROM account_domain_blocks
             WHERE account_id = $1 AND domain = ANY($2)",
            receiver_id,
            &domains,
        )
        .fetch_all(db)
        .await?
        .into_iter()
        .collect();

        let blocked_by = sqlx::query_scalar!(
            "SELECT account_id FROM blocks
             WHERE target_account_id = $1 AND account_id = ANY($2)",
            receiver_id,
            &authors_and_boosted,
        )
        .fetch_all(db)
        .await?
        .into_iter()
        .collect();

        let exclusive_list_users = match receiver {
            Receiver::Home => sqlx::query_scalar!(
                "SELECT la.account_id FROM list_accounts la
                 JOIN lists l ON l.id = la.list_id
                 WHERE l.account_id = $1 AND l.exclusive AND la.account_id = ANY($2)",
                receiver_id,
                &account_ids,
            )
            .fetch_all(db)
            .await?
            .into_iter()
            .collect(),
            Receiver::List { .. } => HashSet::new(),
        };

        Ok(Self {
            active_mentions,
            following,
            languages,
            hiding_reblogs,
            blocking,
            muting,
            domain_blocking,
            blocked_by,
            exclusive_list_users,
        })
    }
}

/// `FeedManager#filter_from_home`: whether `status` stays out of the
/// receiver's home feed, or out of one of its lists.
fn filter_from_home(
    status: &Candidate,
    receiver_id: i64,
    crutches: &Crutches,
    receiver: Receiver,
) -> bool {
    if receiver_id == status.account_id {
        return false;
    }
    if status.reply && (status.in_reply_to_id.is_none() || status.in_reply_to_account_id.is_none())
    {
        return true;
    }
    // `:skip_home`
    if matches!(receiver, Receiver::Home)
        && crutches.exclusive_list_users.contains(&status.account_id)
    {
        return true;
    }
    if let (Some(languages), Some(language)) = (
        crutches.languages.get(&status.account_id),
        status.language.as_deref().filter(|l| !l.is_empty()),
    ) {
        if !languages.iter().any(|l| l == language) {
            return true;
        }
    }
    if status.reblog_of_id.is_some() && status.reblog.is_none() {
        return true;
    }

    let mut check_for_blocks: Vec<i64> = crutches
        .active_mentions
        .get(&status.id)
        .cloned()
        .unwrap_or_default();
    check_for_blocks.push(status.account_id);
    if let (Some(reblog_of_id), Some((reblog_account_id, _))) =
        (status.reblog_of_id, &status.reblog)
    {
        check_for_blocks.push(*reblog_account_id);
        check_for_blocks.extend(
            crutches
                .active_mentions
                .get(&reblog_of_id)
                .into_iter()
                .flatten(),
        );
    }
    if check_for_blocks
        .iter()
        .any(|id| crutches.blocking.contains(id) || crutches.muting.contains(id))
    {
        return true;
    }
    if crutches.blocked_by.contains(&status.account_id) {
        return true;
    }

    match (status.in_reply_to_account_id, &status.reblog) {
        (Some(in_reply_to), _) if status.reply => {
            !crutches.following.contains(&in_reply_to)
                && receiver_id != in_reply_to
                && status.account_id != in_reply_to
        }
        (_, Some((reblog_account_id, reblog_domain))) => {
            crutches.hiding_reblogs.contains(&status.account_id)
                || crutches.blocked_by.contains(reblog_account_id)
                || reblog_domain
                    .as_ref()
                    .is_some_and(|d| crutches.domain_blocking.contains(d))
        }
        _ => false,
    }
}

/// The statuses of `statuses` that [`filter_from_home`] lets in, in order.
async fn unfiltered(
    db: &PgPool,
    receiver_id: i64,
    statuses: Vec<Candidate>,
    receiver: Receiver,
) -> Vec<Candidate> {
    if statuses.is_empty() {
        return statuses;
    }
    match Crutches::build(db, receiver_id, &statuses, receiver).await {
        Ok(crutches) => statuses
            .into_iter()
            .filter(|s| !filter_from_home(s, receiver_id, &crutches, receiver))
            .collect(),
        Err(error) => {
            tracing::warn!(%error, receiver_id, "could not read what filters a feed");
            Vec::new()
        }
    }
}

/// `User#signed_in_recently?` for the user of `account_id`.
async fn is_signed_in_recently(db: &PgPool, account_id: i64) -> bool {
    !signed_in_recently(db, &[account_id]).await.is_empty()
}

/// Those of `account_ids` whose user signed in within
/// [`crate::home_feed::ACTIVE_DAYS`] (`User.signed_in_recently`), in order.
async fn signed_in_recently(db: &PgPool, account_ids: &[i64]) -> Vec<i64> {
    if account_ids.is_empty() {
        return Vec::new();
    }
    let active: HashSet<i64> = sqlx::query_scalar!(
        r#"SELECT account_id FROM users
           WHERE account_id = ANY($1)
             AND current_sign_in_at >= now() - make_interval(days => $2)"#,
        account_ids,
        crate::home_feed::ACTIVE_DAYS,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .collect();
    account_ids
        .iter()
        .copied()
        .filter(|id| active.contains(id))
        .collect()
}

// ── Reading (`Feed#get`) ──────────────────────────────────────────────────

/// `Feed#from_redis`: up to `limit` ids, newest first below `max_id` and
/// above `since_id`, or, given `min_id`, the oldest above it (and below
/// `max_id`), oldest first.
async fn get(
    redis: &mut ConnectionManager,
    key: &str,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: isize,
) -> Vec<i64> {
    let max = max_id.map_or_else(|| "+inf".to_owned(), |id| id.to_string());
    let read = match min_id {
        Some(min_id) => {
            redis::cmd("ZRANGEBYSCORE")
                .arg(key)
                .arg(format!("({min_id}"))
                .arg(format!("({max}"))
                .arg("LIMIT")
                .arg(0i64)
                .arg(limit)
                .query_async(redis)
                .await
        }
        None => {
            let since = since_id.map_or_else(|| "-inf".to_owned(), |id| id.to_string());
            redis::cmd("ZREVRANGEBYSCORE")
                .arg(key)
                .arg(format!("({max}"))
                .arg(format!("({since}"))
                .arg("LIMIT")
                .arg(0i64)
                .arg(limit)
                .query_async(redis)
                .await
        }
    };
    read.unwrap_or_else(|error| {
        tracing::warn!(%error, key, "could not read a feed");
        Vec::new()
    })
}

/// `HomeFeed#get`.
pub async fn feed_get(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: isize,
) -> Vec<i64> {
    get(
        redis,
        &feed_key(keys, account_id),
        max_id,
        since_id,
        min_id,
        limit,
    )
    .await
}

/// `ListFeed#get`.
pub async fn list_feed_get(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    list_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: isize,
) -> Vec<i64> {
    get(
        redis,
        &list_feed_key(keys, list_id),
        max_id,
        since_id,
        min_id,
        limit,
    )
    .await
}

/// What `FeedManager#remove_from_feed` will answer for the status, asked
/// before the feed lets go of it: whether the home feed holds it.
pub async fn home_holds(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    account_id: i64,
    status_id: i64,
) -> bool {
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
    redis
        .zscore::<_, _, Option<f64>>(list_feed_key(keys, list_id), status_id)
        .await
        .ok()
        .flatten()
        .is_some()
}

// ── Fan-out ───────────────────────────────────────────────────────────────

/// What `FeedManager#add_to_feed` answered for a status in each home feed
/// (by account) and list feed (by list) the fan-out delivered it to, which is
/// what decides whether `push_to_home` and `push_to_list` stream it.
#[derive(Debug, Default, Clone)]
pub struct Pushed {
    pub homes: HashMap<i64, bool>,
    pub lists: HashMap<i64, bool>,
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

/// `FanOutOnWriteService` for a status already stored: [`fanout_new_status`]
/// and [`fanout_to_lists`], as `DistributionWorker` runs them.
pub async fn fanout_status(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    status_id: i64,
) -> Pushed {
    let homes = fanout_new_status(redis, keys, db, status_id).await;
    let lists = fanout_to_lists(redis, keys, db, status_id).await;
    Pushed { homes, lists }
}

/// A status as `FanOutOnWriteService` distributes it.
struct Distributed {
    status: Candidate,
    visibility: i32,
    author_domain: Option<String>,
    author_silenced: bool,
}

impl Distributed {
    /// `broadcastable?`: public, no boost, and not by a silenced account.
    fn broadcastable(&self) -> bool {
        self.visibility == crate::db::models::vis::PUBLIC
            && self.status.reblog_of_id.is_none()
            && !self.author_silenced
    }
}

/// The status, unless it is gone or `@status.proper.account.suspended?`.
async fn distributed(db: &PgPool, status_id: i64) -> Option<Distributed> {
    let row = sqlx::query!(
        r#"SELECT s.id, s.account_id, s.reply, s.in_reply_to_id, s.in_reply_to_account_id,
                  s.language, s.reblog_of_id, s.visibility,
                  a.domain AS author_domain, a.silenced_at IS NOT NULL AS "author_silenced!",
                  a.suspended_at IS NOT NULL AS "author_suspended!",
                  r.account_id AS "reblog_account_id?", ra.domain AS "reblog_domain?",
                  ra.suspended_at IS NOT NULL AS "reblog_suspended?"
           FROM statuses s
           JOIN accounts a ON a.id = s.account_id
           LEFT JOIN statuses r ON r.id = s.reblog_of_id AND r.deleted_at IS NULL
           LEFT JOIN accounts ra ON ra.id = r.account_id
           WHERE s.id = $1 AND s.deleted_at IS NULL"#,
        status_id,
    )
    .fetch_optional(db)
    .await
    .ok()
    .flatten()?;
    let proper_suspended = match row.reblog_account_id {
        Some(_) => row.reblog_suspended.unwrap_or(false),
        None => row.author_suspended,
    };
    if proper_suspended {
        return None;
    }
    Some(Distributed {
        status: Candidate {
            id: row.id,
            account_id: row.account_id,
            reply: row.reply,
            in_reply_to_id: row.in_reply_to_id,
            in_reply_to_account_id: row.in_reply_to_account_id,
            language: row.language,
            reblog_of_id: row.reblog_of_id,
            reblog: row.reblog_account_id.map(|id| (id, row.reblog_domain)),
        },
        visibility: row.visibility,
        author_domain: row.author_domain,
        author_silenced: row.author_silenced,
    })
}

/// `FeedManager#build_crutches` for one status and many receivers at once:
/// the crutches each of `receivers` would get, read in a handful of
/// queries rather than a handful per receiver, as the fan-out's
/// `FeedInsertWorker`s build them one at a time. `home` adds
/// `crutches[:exclusive_list_users]`, which a list's crutches go without;
/// `crutches[:following]` is the follows, which a list replaces as its
/// `replies_policy` says.
async fn crutches_for(
    db: &PgPool,
    status: &Candidate,
    author_domain: Option<&str>,
    receivers: &[i64],
    home: bool,
) -> sqlx::Result<HashMap<i64, Crutches>> {
    let mut crutches: HashMap<i64, Crutches> = receivers
        .iter()
        .map(|&r| (r, Crutches::default()))
        .collect();
    if receivers.is_empty() {
        return Ok(crutches);
    }
    let author = status.account_id;
    let mentioned_ids: Vec<i64> = std::iter::once(status.id)
        .chain(status.reblog_of_id)
        .collect();
    let mut active_mentions: HashMap<i64, Vec<i64>> = HashMap::new();
    for m in sqlx::query!(
        "SELECT status_id, account_id FROM mentions WHERE status_id = ANY($1) AND NOT silent",
        &mentioned_ids,
    )
    .fetch_all(db)
    .await?
    {
        active_mentions
            .entry(m.status_id)
            .or_default()
            .push(m.account_id);
    }
    let mut check_for_blocks: Vec<i64> =
        active_mentions.get(&status.id).cloned().unwrap_or_default();
    check_for_blocks.push(author);
    let reblog_author = status.reblog.as_ref().map(|(id, _)| *id);
    if let (Some(reblog_of_id), Some(reblog_author)) = (status.reblog_of_id, reblog_author) {
        check_for_blocks.push(reblog_author);
        check_for_blocks.extend(active_mentions.get(&reblog_of_id).into_iter().flatten());
    }
    let domains: Vec<String> = author_domain
        .map(str::to_owned)
        .into_iter()
        .chain(status.reblog.as_ref().and_then(|(_, d)| d.clone()))
        .collect();
    let authors: Vec<i64> = std::iter::once(author).chain(reblog_author).collect();

    if let Some(in_reply_to) = status.in_reply_to_account_id {
        for receiver in sqlx::query_scalar!(
            "SELECT account_id FROM follows
             WHERE account_id = ANY($1) AND target_account_id = $2",
            receivers,
            in_reply_to,
        )
        .fetch_all(db)
        .await?
        {
            if let Some(c) = crutches.get_mut(&receiver) {
                c.following.insert(in_reply_to);
            }
        }
    }
    for f in sqlx::query!(
        "SELECT account_id, languages, show_reblogs FROM follows
         WHERE account_id = ANY($1) AND target_account_id = $2",
        receivers,
        author,
    )
    .fetch_all(db)
    .await?
    {
        if let Some(c) = crutches.get_mut(&f.account_id) {
            if let Some(languages) = f.languages.filter(|l| !l.is_empty()) {
                c.languages.insert(author, languages);
            }
            if status.reblog_of_id.is_some() && !f.show_reblogs {
                c.hiding_reblogs.insert(author);
            }
        }
    }
    for b in sqlx::query!(
        "SELECT account_id, target_account_id FROM blocks
         WHERE account_id = ANY($1) AND target_account_id = ANY($2)",
        receivers,
        &check_for_blocks,
    )
    .fetch_all(db)
    .await?
    {
        if let Some(c) = crutches.get_mut(&b.account_id) {
            c.blocking.insert(b.target_account_id);
        }
    }
    for m in sqlx::query!(
        "SELECT account_id, target_account_id FROM mutes
         WHERE account_id = ANY($1) AND target_account_id = ANY($2)",
        receivers,
        &check_for_blocks,
    )
    .fetch_all(db)
    .await?
    {
        if let Some(c) = crutches.get_mut(&m.account_id) {
            c.muting.insert(m.target_account_id);
        }
    }
    if !domains.is_empty() {
        for d in sqlx::query!(
            "SELECT account_id, domain FROM account_domain_blocks
             WHERE account_id = ANY($1) AND domain = ANY($2)",
            receivers,
            &domains,
        )
        .fetch_all(db)
        .await?
        {
            if let Some(c) = crutches.get_mut(&d.account_id) {
                c.domain_blocking.insert(d.domain);
            }
        }
    }
    for b in sqlx::query!(
        "SELECT account_id, target_account_id FROM blocks
         WHERE target_account_id = ANY($1) AND account_id = ANY($2)",
        receivers,
        &authors,
    )
    .fetch_all(db)
    .await?
    {
        if let Some(c) = crutches.get_mut(&b.target_account_id) {
            c.blocked_by.insert(b.account_id);
        }
    }
    if home {
        for receiver in sqlx::query_scalar!(
            "SELECT DISTINCT l.account_id FROM list_accounts la
             JOIN lists l ON l.id = la.list_id
             WHERE l.account_id = ANY($1) AND l.exclusive AND la.account_id = $2",
            receivers,
            author,
        )
        .fetch_all(db)
        .await?
        {
            if let Some(c) = crutches.get_mut(&receiver) {
                c.exclusive_list_users.insert(author);
            }
        }
    }
    for c in crutches.values_mut() {
        c.active_mentions = active_mentions.clone();
    }
    Ok(crutches)
}

/// `FeedManager#filter_from_tags?`: whether a followed hashtag's status
/// stays out of the receiver's home feed.
fn filter_from_tags(
    status: &Candidate,
    author_domain: Option<&str>,
    receiver_id: i64,
    crutches: &Crutches,
) -> bool {
    receiver_id == status.account_id
        || crutches
            .active_mentions
            .get(&status.id)
            .into_iter()
            .flatten()
            .chain(std::iter::once(&status.account_id))
            .any(|id| crutches.blocking.contains(id) || crutches.muting.contains(id))
        || crutches.blocked_by.contains(&status.account_id)
        || author_domain.is_some_and(|d| crutches.domain_blocking.contains(d))
}

/// `FanOutOnWriteService#fan_out_to_local_recipients!` and
/// `#fan_out_to_public_recipients!` for the home feeds: `deliver_to_self!`
/// unfiltered, then a `FeedInsertWorker` for each follower who signed in
/// recently (only those mentioned, for a direct or limited post), filtered
/// by `FeedManager#filter_from_home`, and for each such follower of one of
/// its hashtags when the post is `broadcastable?`, filtered by
/// `#filter_from_tags?`; each that passes is `push_to_home`d. For each home
/// pushed to, whether the status went in ([`Pushed::homes`]).
pub async fn fanout_new_status(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    status_id: i64,
) -> HashMap<i64, bool> {
    let mut pushed = HashMap::new();
    let Some(d) = distributed(db, status_id).await else {
        return pushed;
    };
    let status = &d.status;
    let author = status.account_id;

    // `deliver_to_self!` (`if @account.local?`; only a local account has a
    // user to have signed in).
    let mut deliveries: Vec<i64> = signed_in_recently(db, &[author]).await;

    let followers: Vec<i64> = match d.visibility {
        crate::db::models::vis::PUBLIC
        | crate::db::models::vis::UNLISTED
        | crate::db::models::vis::PRIVATE => followers_for_local_distribution(db, author).await,
        // `deliver_to_mentioned_followers!`
        _ => sqlx::query_scalar!(
            r#"SELECT DISTINCT u.account_id FROM mentions m
               JOIN follows f ON f.account_id = m.account_id AND f.target_account_id = $2
               JOIN users u ON u.account_id = m.account_id
               WHERE m.status_id = $1
                 AND u.current_sign_in_at >= now() - make_interval(days => $3)"#,
            status_id,
            author,
            crate::home_feed::ACTIVE_DAYS,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default(),
    };
    // `deliver_to_hashtag_followers!` (`TagFollow.for_local_distribution`).
    let tag_followers: Vec<i64> = if d.broadcastable() {
        sqlx::query_scalar!(
            r#"SELECT DISTINCT tf.account_id FROM tag_follows tf
               JOIN statuses_tags st ON st.tag_id = tf.tag_id
               JOIN users u ON u.account_id = tf.account_id
               WHERE st.status_id = $1
                 AND u.current_sign_in_at >= now() - make_interval(days => $2)"#,
            status_id,
            crate::home_feed::ACTIVE_DAYS,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default()
    } else {
        Vec::new()
    };

    let receivers: Vec<i64> = followers
        .iter()
        .chain(&tag_followers)
        .copied()
        .collect::<HashSet<i64>>()
        .into_iter()
        .collect();
    let crutches =
        match crutches_for(db, status, d.author_domain.as_deref(), &receivers, true).await {
            Ok(crutches) => crutches,
            Err(error) => {
                tracing::warn!(%error, status_id, "could not read what filters the fan-out");
                return pushed;
            }
        };
    for follower in followers {
        if let Some(c) = crutches.get(&follower) {
            if !filter_from_home(status, follower, c, Receiver::Home) {
                deliveries.push(follower);
            }
        }
    }
    for follower in tag_followers {
        if let Some(c) = crutches.get(&follower) {
            if !filter_from_tags(status, d.author_domain.as_deref(), follower, c) {
                deliveries.push(follower);
            }
        }
    }
    if deliveries.is_empty() {
        return pushed;
    }

    let separate = if status.reblog_of_id.is_some() {
        not_aggregating(db, &deliveries).await
    } else {
        Default::default()
    };
    // One account can be pushed to twice, as a follower and as a follower of
    // a hashtag, as Mastodon queues a `FeedInsertWorker` for each.
    for id in deliveries {
        let timeline = Timeline::home(keys, id);
        let added = push(
            redis,
            &timeline,
            status_id,
            status.reblog_of_id,
            !separate.contains(&id),
        )
        .await;
        *pushed.entry(id).or_insert(false) |= added;
    }
    pushed
}

/// Remove a deleted status from its author's and followers' home feeds. The
/// status row must still be there to say whether it was a boost; for one
/// already gone, use [`fanout_remove_boost`].
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

/// `RemoveStatusService#remove_from_self` and `#remove_from_followers`:
/// `FeedManager#unpush_from_home` for the author, when local, and each
/// follower who signed in recently (`followers_for_local_distribution`), for
/// a status that boosted `reblog_of_id`.
pub async fn fanout_remove_boost(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    author_id: i64,
    status_id: i64,
    reblog_of_id: Option<i64>,
) {
    let recipients: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT u.account_id FROM users u
           WHERE u.account_id = $1
              OR (u.current_sign_in_at >= now() - make_interval(days => $2)
                  AND EXISTS (SELECT 1 FROM follows f
                              WHERE f.account_id = u.account_id AND f.target_account_id = $1))"#,
        author_id,
        crate::home_feed::ACTIVE_DAYS,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let separate = if reblog_of_id.is_some() {
        not_aggregating(db, &recipients).await
    } else {
        Default::default()
    };
    for id in recipients {
        let timeline = Timeline::home(keys, id);
        let aggregate = !separate.contains(&id);
        if let Err(error) =
            remove_from_feed(redis, &timeline, status_id, reblog_of_id, aggregate).await
        {
            tracing::warn!(%error, account_id = id, "could not remove a status from a home feed");
        }
    }
}

/// `Account#followers_for_local_distribution`: the local followers of
/// `account_id` who signed in recently.
pub async fn followers_for_local_distribution(db: &PgPool, account_id: i64) -> Vec<i64> {
    sqlx::query_scalar!(
        r#"SELECT u.account_id FROM users u
           JOIN follows f ON f.account_id = u.account_id
           WHERE f.target_account_id = $1
             AND u.current_sign_in_at >= now() - make_interval(days => $2)"#,
        account_id,
        crate::home_feed::ACTIVE_DAYS,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
}

/// The ids of [`lists_for_local_distribution`].
pub async fn list_ids_for_local_distribution(db: &PgPool, account_id: i64) -> Vec<i64> {
    lists_for_local_distribution(db, account_id)
        .await
        .into_iter()
        .map(|(id, _, _)| id)
        .collect()
}

/// `Account#lists_for_local_distribution`: the lists holding `author_id`
/// whose owner signed in recently, and which follow it or are its own, with
/// their owners and reply policies.
async fn lists_for_local_distribution(db: &PgPool, author_id: i64) -> Vec<(i64, i64, i32)> {
    sqlx::query!(
        r#"SELECT DISTINCT l.id, l.account_id, l.replies_policy FROM lists l
           JOIN list_accounts la ON la.list_id = l.id
           JOIN users u ON u.account_id = l.account_id
           WHERE la.account_id = $1
             AND (la.follow_id IS NOT NULL OR l.account_id = $1)
             AND u.current_sign_in_at >= now() - make_interval(days => $2)"#,
        author_id,
        crate::home_feed::ACTIVE_DAYS,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|l| (l.id, l.account_id, l.replies_policy))
    .collect()
}

/// `FanOutOnWriteService#deliver_to_lists!`, for a public, unlisted or
/// private post: a `FeedInsertWorker` for each list of
/// [`lists_for_local_distribution`], which `push_to_list`s unless
/// `FeedManager#filter_from_list?` or `#filter_from_home` (with the list's
/// crutches) keeps it out; and whether it went in ([`Pushed::lists`]).
pub async fn fanout_to_lists(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    status_id: i64,
) -> HashMap<i64, bool> {
    let mut pushed = HashMap::new();
    let Some(d) = distributed(db, status_id).await else {
        return pushed;
    };
    if !matches!(
        d.visibility,
        crate::db::models::vis::PUBLIC
            | crate::db::models::vis::UNLISTED
            | crate::db::models::vis::PRIVATE
    ) {
        return pushed;
    }
    let status = &d.status;
    let author = status.account_id;
    let lists = lists_for_local_distribution(db, author).await;
    if lists.is_empty() {
        return pushed;
    }

    let owners: Vec<i64> = lists
        .iter()
        .map(|l| l.1)
        .collect::<HashSet<i64>>()
        .into_iter()
        .collect();
    let owner_crutches =
        match crutches_for(db, status, d.author_domain.as_deref(), &owners, false).await {
            Ok(crutches) => crutches,
            Err(error) => {
                tracing::warn!(%error, status_id, "could not read what filters the fan-out");
                return pushed;
            }
        };
    // The lists that hold the account replied to, for `show_list?`.
    let in_reply_to = status.in_reply_to_account_id;
    let listing_reply_target: HashSet<i64> = match in_reply_to {
        Some(target) => {
            let list_ids: Vec<i64> = lists.iter().map(|l| l.0).collect();
            sqlx::query_scalar!(
                "SELECT list_id FROM list_accounts WHERE list_id = ANY($1) AND account_id = $2",
                &list_ids,
                target,
            )
            .fetch_all(db)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect()
        }
        None => HashSet::new(),
    };

    let separate = if status.reblog_of_id.is_some() {
        not_aggregating(db, &owners).await
    } else {
        Default::default()
    };
    for (list_id, owner_id, replies_policy) in lists {
        let listed = listing_reply_target.contains(&list_id);
        // `FeedManager#filter_from_list?`
        if status.reply && in_reply_to != Some(author) {
            let filtered = in_reply_to != Some(owner_id)
                && replies_policy != crate::db::models::replies::FOLLOWED
                && !(replies_policy == crate::db::models::replies::LIST && listed);
            if filtered {
                continue;
            }
        }
        let Some(owner) = owner_crutches.get(&owner_id) else {
            continue;
        };
        // `crutches_following` for the list.
        let following: HashSet<i64> = match replies_policy {
            crate::db::models::replies::FOLLOWED => owner.following.clone(),
            crate::db::models::replies::LIST => {
                in_reply_to.filter(|_| listed).into_iter().collect()
            }
            _ => HashSet::new(),
        };
        let crutches = Crutches {
            active_mentions: owner.active_mentions.clone(),
            following,
            languages: owner.languages.clone(),
            hiding_reblogs: owner.hiding_reblogs.clone(),
            blocking: owner.blocking.clone(),
            muting: owner.muting.clone(),
            domain_blocking: owner.domain_blocking.clone(),
            blocked_by: owner.blocked_by.clone(),
            exclusive_list_users: HashSet::new(),
        };
        let receiver = Receiver::List {
            id: list_id,
            replies_policy,
        };
        if filter_from_home(status, owner_id, &crutches, receiver) {
            continue;
        }
        let timeline = Timeline::list(keys, list_id);
        let aggregate = !separate.contains(&owner_id);
        let added = push(redis, &timeline, status_id, status.reblog_of_id, aggregate).await;
        pushed.insert(list_id, added);
    }
    pushed
}

/// Remove a deleted status from the lists of
/// [`lists_for_local_distribution`]. As with [`fanout_remove_status`], the
/// row must still be there; for one already gone, use
/// [`fanout_remove_boost_from_lists`].
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

/// `RemoveStatusService#remove_from_lists`: `FeedManager#unpush_from_list`
/// for each list of [`lists_for_local_distribution`].
pub async fn fanout_remove_boost_from_lists(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    author_id: i64,
    status_id: i64,
    reblog_of_id: Option<i64>,
) {
    let lists = lists_for_local_distribution(db, author_id).await;
    if lists.is_empty() {
        return;
    }
    let owners: Vec<i64> = lists.iter().map(|l| l.1).collect();
    let separate = if reblog_of_id.is_some() {
        not_aggregating(db, &owners).await
    } else {
        Default::default()
    };
    for (list_id, owner_id, _) in lists {
        let timeline = Timeline::list(keys, list_id);
        let aggregate = !separate.contains(&owner_id);
        if let Err(error) =
            remove_from_feed(redis, &timeline, status_id, reblog_of_id, aggregate).await
        {
            tracing::warn!(%error, list_id, "could not remove a status from a list feed");
        }
    }
}

// ── Populating and merging ────────────────────────────────────────────────

/// A list as the feed work needs it: its owner and `replies_policy`.
async fn find_list(db: &PgPool, list_id: i64) -> Option<(i64, i32)> {
    sqlx::query!(
        "SELECT account_id, replies_policy FROM lists WHERE id = $1",
        list_id,
    )
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .map(|l| (l.account_id, l.replies_policy))
}

/// Add each of `statuses`, in order, then [`trim`] once: the loop
/// `populate_home`, `populate_list` and the merges share.
async fn add_all(
    redis: &mut ConnectionManager,
    timeline: &Timeline,
    statuses: &[Candidate],
    aggregate: bool,
) -> redis::RedisResult<()> {
    for status in statuses {
        add_to_feed(redis, timeline, status.id, status.reblog_of_id, aggregate).await?;
    }
    trim(redis, timeline).await
}

/// The followed accounts' part of `populate_home` and `populate_list`: for
/// each of `targets` (an account and its `last_status_at`), the newest
/// [`FEED_MAX_ITEMS`]` / 2` of its eligible statuses the filter lets in,
/// skipping, once the feed holds that many, an account that has posted
/// nothing newer than the feed's oldest entry, and reading only what is
/// newer than that.
async fn populate_from(
    redis: &mut ConnectionManager,
    db: &PgPool,
    timeline: &Timeline,
    receiver_id: i64,
    receiver: Receiver,
    targets: Vec<(i64, Option<chrono::NaiveDateTime>)>,
    aggregate: bool,
) -> redis::RedisResult<()> {
    let limit = (FEED_MAX_ITEMS / 2) as i64;
    let mut over_limit = false;
    for (target_id, last_status_at) in targets {
        over_limit = over_limit || timeline_size(redis, timeline).await >= limit;
        let mut after = None;
        if over_limit {
            if let Some(oldest) = oldest_score(redis, timeline).await {
                // None of its statuses would stay on the feed.
                if id_at(last_status_at) < oldest {
                    continue;
                }
                // `where(id: oldest_home_score...)`
                after = Some(oldest - 1);
            }
        }
        let statuses = eligible_statuses(db, target_id, after, limit).await;
        if statuses.is_empty() {
            continue;
        }
        let statuses = unfiltered(db, receiver_id, statuses, receiver).await;
        add_all(redis, timeline, &statuses, aggregate).await?;
    }
    Ok(())
}

/// `FeedManager#populate_home`: fill a home feed from scratch with the
/// account's own statuses and those of the accounts it follows.
pub async fn populate_home(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    account_id: i64,
) {
    let limit = (FEED_MAX_ITEMS / 2) as i64;
    let aggregate = aggregates(db, account_id).await;
    let timeline = Timeline::home(keys, account_id);
    let populated = async {
        // `account.statuses.limit(limit)`, unfiltered.
        let own = sqlx::query!(
            "SELECT id, reblog_of_id FROM statuses
             WHERE account_id = $1 AND deleted_at IS NULL
             ORDER BY id DESC LIMIT $2",
            account_id,
            limit,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default();
        for status in own {
            add_to_feed(redis, &timeline, status.id, status.reblog_of_id, aggregate).await?;
        }
        // `account.following.includes(:account_stat).reorder(nil).find_each`
        let targets = sqlx::query!(
            r#"SELECT a.id, st.last_status_at AS "last_status_at?" FROM follows f
               JOIN accounts a ON a.id = f.target_account_id
               LEFT JOIN account_stats st ON st.account_id = a.id
               WHERE f.account_id = $1
               ORDER BY a.id"#,
            account_id,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|t| (t.id, t.last_status_at))
        .collect();
        populate_from(
            redis,
            db,
            &timeline,
            account_id,
            Receiver::Home,
            targets,
            aggregate,
        )
        .await
    }
    .await;
    if let Err(error) = populated {
        tracing::warn!(%error, account_id, "could not populate a home feed");
    }
}

/// `FeedManager#populate_list`: fill a list feed from scratch with the
/// statuses of its active members (those followed).
pub async fn populate_list(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    list_id: i64,
) {
    let Some((owner_id, replies_policy)) = find_list(db, list_id).await else {
        return;
    };
    let aggregate = aggregates(db, owner_id).await;
    let timeline = Timeline::list(keys, list_id);
    // `list.active_accounts.includes(:account_stat).reorder(nil).find_each`
    let targets = sqlx::query!(
        r#"SELECT a.id, st.last_status_at AS "last_status_at?" FROM list_accounts la
           JOIN accounts a ON a.id = la.account_id
           LEFT JOIN account_stats st ON st.account_id = a.id
           WHERE la.list_id = $1 AND la.follow_id IS NOT NULL
           ORDER BY a.id"#,
        list_id,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|t| (t.id, t.last_status_at))
    .collect();
    let receiver = Receiver::List {
        id: list_id,
        replies_policy,
    };
    if let Err(error) =
        populate_from(redis, db, &timeline, owner_id, receiver, targets, aggregate).await
    {
        tracing::warn!(%error, list_id, "could not populate a list feed");
    }
}

/// The merges' part of `merge_into_home` and `merge_into_list`: the newest
/// [`FEED_MAX_ITEMS`]` / 4` of `from_account_id`'s eligible statuses, only
/// those newer than the feed's oldest entry once it holds that many, that the
/// filter lets in.
async fn merge(
    redis: &mut ConnectionManager,
    db: &PgPool,
    timeline: &Timeline,
    from_account_id: i64,
    receiver_id: i64,
    receiver: Receiver,
) {
    let limit = (FEED_MAX_ITEMS / 4) as i64;
    let aggregate = aggregates(db, receiver_id).await;
    let after = if timeline_size(redis, timeline).await >= limit {
        oldest_score(redis, timeline).await
    } else {
        None
    };
    let statuses = eligible_statuses(db, from_account_id, after, limit).await;
    let statuses = unfiltered(db, receiver_id, statuses, receiver).await;
    if let Err(error) = add_all(redis, timeline, &statuses, aggregate).await {
        tracing::warn!(%error, key = %timeline.key, "could not merge statuses into a feed");
    }
}

/// `FeedManager#merge_into_home`: fill `into_account_id`'s home feed with
/// `from_account_id`'s statuses, unless its user has not signed in recently.
pub async fn merge_into_home(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    from_account_id: i64,
    into_account_id: i64,
) {
    if !is_signed_in_recently(db, into_account_id).await {
        return;
    }
    merge(
        redis,
        db,
        &Timeline::home(keys, into_account_id),
        from_account_id,
        into_account_id,
        Receiver::Home,
    )
    .await;
}

/// `FeedManager#merge_into_list`: fill a list feed with `from_account_id`'s
/// statuses, unless the list's owner has not signed in recently.
pub async fn merge_into_list(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    from_account_id: i64,
    list_id: i64,
) {
    let Some((owner_id, replies_policy)) = find_list(db, list_id).await else {
        return;
    };
    if !is_signed_in_recently(db, owner_id).await {
        return;
    }
    merge(
        redis,
        db,
        &Timeline::list(keys, list_id),
        from_account_id,
        owner_id,
        Receiver::List {
            id: list_id,
            replies_policy,
        },
    )
    .await;
}

/// `FeedManager#unmerge_from_home`: take `from_account_id`'s statuses out of
/// `into_account_id`'s home feed, as an unfollow, a block or a suspension
/// does.
pub async fn unmerge_from_home(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    from_account_id: i64,
    into_account_id: i64,
) {
    unmerge_account(
        redis,
        db,
        from_account_id,
        into_account_id,
        &Timeline::home(keys, into_account_id),
    )
    .await;
}

/// `FeedManager#unmerge_from_list`.
pub async fn unmerge_from_list(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    from_account_id: i64,
    list_id: i64,
) {
    let Some((owner_id, _)) = find_list(db, list_id).await else {
        return;
    };
    unmerge_account(
        redis,
        db,
        from_account_id,
        owner_id,
        &Timeline::list(keys, list_id),
    )
    .await;
}

/// `from_account.statuses.where(id: timeline_status_ids)`, each removed.
async fn unmerge_account(
    redis: &mut ConnectionManager,
    db: &PgPool,
    from_account_id: i64,
    owner_id: i64,
    timeline: &Timeline,
) {
    // The feed's *members* are exact status ids (only the scores are lossy
    // f64s), so read the members and keep the ones the account wrote.
    let members: Vec<i64> = redis
        .zrange::<_, Vec<i64>>(&timeline.key, 0, -1)
        .await
        .unwrap_or_default();
    if members.is_empty() {
        return;
    }
    let ids: Vec<i64> = sqlx::query_scalar!(
        "SELECT id FROM statuses
         WHERE account_id = $1 AND id = ANY($2::bigint[]) AND deleted_at IS NULL",
        from_account_id,
        &members,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    unmerge(redis, db, owner_id, timeline, &ids).await;
}

/// Remove every home-feed entry authored by an account on `domain`, used when a
/// user blocks a domain.
pub async fn unmerge_domain_from_home(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    domain: &str,
    into_account_id: i64,
) {
    let timeline = Timeline::home(keys, into_account_id);
    let members: Vec<i64> = redis
        .zrange::<_, Vec<i64>>(&timeline.key, 0, -1)
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
    unmerge(redis, db, into_account_id, &timeline, &ids).await;
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
    let aggregate = aggregates(db, owner_id).await;
    for &id in ids {
        let reblog_of_id = reblogs.get(&id).copied();
        if let Err(error) = remove_from_feed(redis, timeline, id, reblog_of_id, aggregate).await {
            tracing::warn!(%error, key = %timeline.key, "could not remove a status from a feed");
            return;
        }
    }
}

// ── Cleaning ──────────────────────────────────────────────────────────────

/// `FeedManager#clean_feeds!` for one feed: the feed, its tracked boosts, and
/// every set of boosts held back.
async fn clean_feed(redis: &mut ConnectionManager, timeline: &Timeline) {
    let tracked: Vec<i64> = redis
        .zrange(&timeline.reblogs, 0, -1)
        .await
        .unwrap_or_default();
    let mut pipe = redis::pipe();
    pipe.del(&timeline.key)
        .ignore()
        .del(&timeline.reblogs)
        .ignore();
    for boosted in tracked {
        pipe.del(timeline.reblog_set(boosted)).ignore();
    }
    let _: redis::RedisResult<()> = pipe.query_async(redis).await;
}

/// `FeedManager#clean_feeds!(:home, [account_id])`.
pub async fn delete_home_feed(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    account_id: i64,
) {
    clean_feed(redis, &Timeline::home(keys, account_id)).await;
}

/// `FeedManager#clean_feeds!(:list, [list_id])`.
pub async fn delete_list_feed(redis: &mut ConnectionManager, keys: &RedisKeyspace, list_id: i64) {
    clean_feed(redis, &Timeline::list(keys, list_id)).await;
}

/// `Vacuum::FeedsVacuum`: remove the home feeds and list feeds of confirmed
/// users who have not signed in within [`crate::home_feed::ACTIVE_DAYS`]
/// (`User.confirmed.not_signed_in_recently`), so that they are regenerated
/// when those users return.
pub async fn vacuum_inactive_feeds(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
) -> sqlx::Result<()> {
    const BATCH: i64 = 1000;
    let mut after = 0i64;
    loop {
        let users = sqlx::query!(
            r#"SELECT id, account_id FROM users
               WHERE confirmed_at IS NOT NULL
                 AND current_sign_in_at < now() - make_interval(days => $1)
                 AND id > $2
               ORDER BY id LIMIT $3"#,
            crate::home_feed::ACTIVE_DAYS,
            after,
            BATCH,
        )
        .fetch_all(db)
        .await?;
        let Some(last) = users.last() else {
            return Ok(());
        };
        after = last.id;
        let account_ids: Vec<i64> = users.iter().map(|u| u.account_id).collect();
        for &account_id in &account_ids {
            delete_home_feed(redis, keys, account_id).await;
        }
        let lists: Vec<i64> = sqlx::query_scalar!(
            "SELECT id FROM lists WHERE account_id = ANY($1)",
            &account_ids,
        )
        .fetch_all(db)
        .await?;
        for list_id in lists {
            delete_list_feed(redis, keys, list_id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{filter_from_home, id_at, Candidate, Crutches, Receiver};

    fn post(id: i64, account_id: i64) -> Candidate {
        Candidate {
            id,
            account_id,
            reply: false,
            in_reply_to_id: None,
            in_reply_to_account_id: None,
            language: None,
            reblog_of_id: None,
            reblog: None,
        }
    }

    #[test]
    fn a_reply_to_someone_not_followed_stays_out() {
        let mut reply = post(10, 2);
        reply.reply = true;
        reply.in_reply_to_id = Some(9);
        reply.in_reply_to_account_id = Some(3);
        let crutches = Crutches::default();
        assert!(filter_from_home(&reply, 1, &crutches, Receiver::Home));
        let following = Crutches {
            following: [3].into(),
            ..Default::default()
        };
        assert!(!filter_from_home(&reply, 1, &following, Receiver::Home));
        // A reply to the receiver, and the receiver's own, go in.
        reply.in_reply_to_account_id = Some(1);
        assert!(!filter_from_home(&reply, 1, &crutches, Receiver::Home));
        // One whose parent is gone stays out.
        reply.in_reply_to_id = None;
        assert!(filter_from_home(&reply, 1, &crutches, Receiver::Home));
        assert!(!filter_from_home(&reply, 2, &crutches, Receiver::Home));
    }

    #[test]
    fn an_exclusive_list_keeps_its_members_off_home_only() {
        let crutches = Crutches {
            exclusive_list_users: [2].into(),
            ..Default::default()
        };
        assert!(filter_from_home(&post(10, 2), 1, &crutches, Receiver::Home));
        let list = Receiver::List {
            id: 5,
            replies_policy: 0,
        };
        assert!(!filter_from_home(&post(10, 2), 1, &crutches, list));
    }

    #[test]
    fn a_boost_is_filtered_by_its_original() {
        let mut boost = post(10, 2);
        boost.reblog_of_id = Some(9);
        // The boosted post is gone.
        assert!(filter_from_home(
            &boost,
            1,
            &Crutches::default(),
            Receiver::Home
        ));
        boost.reblog = Some((3, Some("example.com".into())));
        assert!(!filter_from_home(
            &boost,
            1,
            &Crutches::default(),
            Receiver::Home
        ));
        let muting = Crutches {
            muting: [3].into(),
            ..Default::default()
        };
        assert!(filter_from_home(&boost, 1, &muting, Receiver::Home));
        let domain = Crutches {
            domain_blocking: ["example.com".to_owned()].into(),
            ..Default::default()
        };
        assert!(filter_from_home(&boost, 1, &domain, Receiver::Home));
        let hiding = Crutches {
            hiding_reblogs: [2].into(),
            ..Default::default()
        };
        assert!(filter_from_home(&boost, 1, &hiding, Receiver::Home));
    }

    #[test]
    fn a_follow_limited_to_languages_filters_others() {
        let crutches = Crutches {
            languages: [(2, vec!["en".to_owned()])].into(),
            ..Default::default()
        };
        let mut status = post(10, 2);
        assert!(!filter_from_home(&status, 1, &crutches, Receiver::Home));
        status.language = Some("ko".into());
        assert!(filter_from_home(&status, 1, &crutches, Receiver::Home));
        status.language = Some("en".into());
        assert!(!filter_from_home(&status, 1, &crutches, Receiver::Home));
    }

    #[test]
    fn id_at_is_a_snowflake_without_its_random_part() {
        let at = chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .unwrap()
            .naive_utc();
        assert_eq!(id_at(Some(at)), 1_700_000_000_000 << 16);
        assert_eq!(id_at(None), 0);
    }
}
