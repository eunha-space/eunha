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
    /// Whose feed this is, for the log: an account's home feed or a list's.
    owner: FeedOwner,
}

#[derive(Clone, Copy)]
enum FeedOwner {
    Home(i64),
    List(i64),
}

impl Timeline {
    fn home(keys: &RedisKeyspace, account_id: i64) -> Self {
        Self {
            key: feed_key(keys, account_id),
            reblogs: keys.key(format!("feed:home:{account_id}:reblogs")),
            owner: FeedOwner::Home(account_id),
        }
    }

    fn list(keys: &RedisKeyspace, list_id: i64) -> Self {
        Self {
            key: list_feed_key(keys, list_id),
            reblogs: keys.key(format!("feed:list:{list_id}:reblogs")),
            owner: FeedOwner::List(list_id),
        }
    }

    fn reblog_set(&self, reblog_of_id: i64) -> String {
        format!("{}:{reblog_of_id}", self.reblogs)
    }

    /// Log that `entries` left this feed at once, and why: what makes a feed
    /// that lost its history attributable afterwards. Only the operations
    /// that delete, empty or take many entries out of a feed say so, once
    /// each; a post coming or going does not.
    fn log_removal(&self, reason: &str, entries: u64, target_account_id: Option<i64>) {
        match self.owner {
            FeedOwner::Home(account_id) => tracing::info!(
                account_id,
                reason,
                entries,
                target_account_id,
                "home feed entries removed"
            ),
            FeedOwner::List(list_id) => tracing::info!(
                list_id,
                reason,
                entries,
                target_account_id,
                "list feed entries removed"
            ),
        }
    }
}

/// Why a feed is deleted, emptied, or has many entries taken out at once,
/// as its log line says: named after the Mastodon worker or service that
/// does the same.
pub mod reason {
    /// `DeleteAccountService#purge_feeds!`, and `SuspendAccountService`'s
    /// unmerges, for an account suspended or deleted.
    pub const ACCOUNT_REMOVED: &str = "account suspended or deleted";
    /// `Vacuum::FeedsVacuum`: the user has not signed in for a week.
    pub const INACTIVE: &str = "FeedsVacuum";
    /// `UnmergeWorker`: an unfollow, a domain block, a list member removed.
    pub const UNMERGE: &str = "UnmergeWorker";
    /// `MuteWorker` and `BlockWorker` (`FeedManager#clear_from_home`).
    pub const MUTE_OR_BLOCK: &str = "MuteWorker or BlockWorker";
    /// `TagUnmergeWorker`: a hashtag unfollowed.
    pub const TAG_UNFOLLOWED: &str = "TagUnmergeWorker";
    /// `List#clean_feed_manager`: the list was deleted.
    pub const LIST_DELETED: &str = "list deleted";
    /// A lists import in overwrite mode deleted the list.
    pub const LIST_IMPORT: &str = "lists import";
    /// `tootctl feeds clear`.
    pub const CLEAR: &str = "feeds clear";
    /// `tootctl feeds vacuum`.
    pub const VACUUM_COMMAND: &str = "feeds vacuum";
    /// `RegenerationWorker`: a user back after a week away.
    pub const REGENERATION: &str = "RegenerationWorker";
    /// `tootctl feeds build`.
    pub const BUILD_COMMAND: &str = "feeds build";
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
/// boost standing in for others, put the oldest boost held back in its place;
/// whether the feed held it.
async fn remove_from_feed(
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
                .arg(status_id)
                .query_async(redis)
                .await?;
            if rank.is_none() {
                return Ok(false);
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
            let (removed,): (i64,) = pipe
                .zrem(&timeline.key, status_id)
                .query_async(redis)
                .await?;
            Ok(removed > 0)
        }
        None => {
            let mut pipe = redis::pipe();
            pipe.del(timeline.reblog_set(status_id))
                .ignore()
                .zrem(&timeline.reblogs, status_id)
                .ignore()
                .zrem(&timeline.key, status_id);
            let (removed,): (i64,) = pipe.query_async(redis).await?;
            Ok(removed > 0)
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
    /// Whether the receiver follows the author with `notify` (not one of
    /// Mastodon's crutches: `FeedInsertWorker#notify?` reads it per follow).
    notifying: bool,
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
            notifying: false,
        })
    }
}

/// What `FeedManager#filter_from_home` answers: `nil`, `:filter`, or
/// `:skip_home` for a post an exclusive list keeps off the home feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filtered {
    Filter,
    SkipHome,
}

/// `FeedManager#filter_from_home`: whether `status` stays out of the
/// receiver's home feed, or out of one of its lists.
fn filter_from_home(
    status: &Candidate,
    receiver_id: i64,
    crutches: &Crutches,
    receiver: Receiver,
) -> bool {
    filter_result(status, receiver_id, crutches, receiver).is_some()
}

/// [`filter_from_home`], saying why.
fn filter_result(
    status: &Candidate,
    receiver_id: i64,
    crutches: &Crutches,
    receiver: Receiver,
) -> Option<Filtered> {
    if receiver_id == status.account_id {
        return None;
    }
    if status.reply && (status.in_reply_to_id.is_none() || status.in_reply_to_account_id.is_none())
    {
        return Some(Filtered::Filter);
    }
    if matches!(receiver, Receiver::Home)
        && crutches.exclusive_list_users.contains(&status.account_id)
    {
        return Some(Filtered::SkipHome);
    }
    filtered_after_lists(status, receiver_id, crutches).then_some(Filtered::Filter)
}

/// The rest of `FeedManager#filter_from_home`, after its exclusive list test.
fn filtered_after_lists(status: &Candidate, receiver_id: i64, crutches: &Crutches) -> bool {
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
    /// The followers `FeedInsertWorker#notify?` would notify of the post.
    pub notify: Vec<i64>,
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

/// `FanOutOnWriteService` for a status already stored, its feed work: the
/// home feeds and the lists, as `DistributionWorker` runs it, for an edit
/// when `update`.
pub async fn fanout_status(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    status_id: i64,
    update: bool,
) -> Pushed {
    let (homes, notify) = fan_out_home(redis, keys, db, status_id, update).await;
    let lists = fan_out_lists(redis, keys, db, status_id, update).await;
    Pushed {
        homes,
        lists,
        notify,
    }
}

/// `DistributionWorker#perform`: [`fanout_status`], the bell notifications
/// it asks for (`LocalNotificationWorker` with a `status` notification),
/// then the streaming messages.
pub async fn distribute(state: &crate::state::AppState, status_id: i64, update: bool) {
    let mut redis = state.redis.clone();
    let pushed = fanout_status(&mut redis, &state.redis_keys, &state.db, status_id, update).await;
    if !pushed.notify.is_empty() {
        notify_followers(state, status_id, &pushed.notify).await;
    }
    crate::streaming::fan_out::distribute(state, status_id, update, &pushed).await;
    // `deliver_to_conversation!`: a direct message, not an edit, into its
    // author's conversations, whoever the author is.
    if !update {
        let author = sqlx::query_scalar!(
            r#"SELECT s.account_id FROM statuses s JOIN accounts a ON a.id = s.account_id
               WHERE s.id = $1 AND s.visibility = $2 AND s.deleted_at IS NULL
                 AND a.suspended_at IS NULL"#,
            status_id,
            crate::db::models::vis::DIRECT,
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
        if let Some(author) = author {
            crate::api::mastodon::conversations::add_status(state, author, status_id).await;
        }
    }
}

/// `DistributionWorker.perform_async(status_id)`: [`distribute`] in a task
/// of its own, or at once when the tests ask for background work inline.
pub async fn distribute_later(state: &crate::state::AppState, status_id: i64) {
    if sync_fanout() {
        distribute(state, status_id, false).await;
    } else {
        let state = state.clone();
        crate::tenants::spawn(async move { distribute(&state, status_id, false).await });
    }
}

/// `FeedInsertWorker.perform_async(status_id, account_id, 'home')`: the
/// status into one account's home feed, where `filter_from_home` lets it in,
/// and streamed to it.
pub async fn insert_into_home(state: &crate::state::AppState, status_id: i64, account_id: i64) {
    let db = &state.db;
    let Some(d) = distributed(db, status_id).await else {
        return;
    };
    let status = &d.status;
    let crutches =
        match crutches_for(db, status, d.author_domain.as_deref(), &[account_id], true).await {
            Ok(crutches) => crutches,
            Err(error) => {
                tracing::warn!(%error, status_id, "could not read what filters a home insert");
                return;
            }
        };
    let Some(c) = crutches.get(&account_id) else {
        return;
    };
    if filter_result(status, account_id, c, Receiver::Home).is_some() {
        return;
    }
    let aggregate = status.reblog_of_id.is_none()
        || !not_aggregating(db, &[account_id])
            .await
            .contains(&account_id);
    let mut redis = state.redis.clone();
    let timeline = Timeline::home(&state.redis_keys, account_id);
    if push(
        &mut redis,
        &timeline,
        status_id,
        status.reblog_of_id,
        aggregate,
    )
    .await
    {
        crate::streaming::fan_out::home_inserted(state, status_id, account_id).await;
    }
}

/// `LocalNotificationWorker.perform_async(follower, status, 'Status',
/// 'status')` for each of `followers`.
async fn notify_followers(state: &crate::state::AppState, status_id: i64, followers: &[i64]) {
    let Ok(Some(author)) =
        sqlx::query_scalar!("SELECT account_id FROM statuses WHERE id = $1", status_id)
            .fetch_optional(&state.db)
            .await
    else {
        return;
    };
    let Ok(account) = crate::api::mastodon::accounts::fetch_account(state, author).await else {
        return;
    };
    for &follower in followers {
        crate::push::create_and_push(state, follower, account.id, "status", Some(status_id)).await;
    }
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
        "SELECT account_id, languages, show_reblogs, notify FROM follows
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
            c.notifying = f.notify;
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
    fan_out_home(redis, keys, db, status_id, false).await.0
}

/// [`fanout_new_status`], for an edit when `update`: a receiver the filters
/// now keep it from has it taken out (`FeedInsertWorker#perform_unpush`,
/// which streams no `delete`). Also the followers to notify
/// ([`Pushed::notify`]).
async fn fan_out_home(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    status_id: i64,
    update: bool,
) -> (HashMap<i64, bool>, Vec<i64>) {
    let mut pushed = HashMap::new();
    let mut notify = Vec::new();
    let Some(d) = distributed(db, status_id).await else {
        return (pushed, notify);
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
                return (pushed, notify);
            }
        };
    // `FeedInsertWorker#notify?`, for a `home` worker: no boost, no reply to
    // someone else, no edit, and nothing `:filter`ed (an exclusive list's
    // `:skip_home` still notifies).
    let notifies = status.reblog_of_id.is_none()
        && !(status.reply && status.in_reply_to_account_id != Some(author))
        && !update;
    // Each `FeedInsertWorker` in turn: `push_to_home` where the filter lets
    // the status in; for an edit, `unpush_from_home` where it does not.
    let mut unpushes: Vec<i64> = Vec::new();
    for follower in followers {
        if let Some(c) = crutches.get(&follower) {
            let result = filter_result(status, follower, c, Receiver::Home);
            match result {
                None => deliveries.push(follower),
                Some(_) if update => unpushes.push(follower),
                Some(_) => {}
            }
            if notifies && result != Some(Filtered::Filter) && c.notifying {
                notify.push(follower);
            }
        }
    }
    for follower in tag_followers {
        if let Some(c) = crutches.get(&follower) {
            if !filter_from_tags(status, d.author_domain.as_deref(), follower, c) {
                deliveries.push(follower);
            } else if update {
                unpushes.push(follower);
            }
        }
    }
    if deliveries.is_empty() && unpushes.is_empty() {
        return (pushed, notify);
    }

    let affected: Vec<i64> = deliveries.iter().chain(&unpushes).copied().collect();
    let separate = if status.reblog_of_id.is_some() {
        not_aggregating(db, &affected).await
    } else {
        Default::default()
    };
    for id in unpushes {
        let timeline = Timeline::home(keys, id);
        if let Err(error) = remove_from_feed(
            redis,
            &timeline,
            status_id,
            status.reblog_of_id,
            !separate.contains(&id),
        )
        .await
        {
            tracing::warn!(%error, account_id = id, "could not remove a status from a home feed");
        }
    }
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
    (pushed, notify)
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
    fan_out_lists(redis, keys, db, status_id, false).await
}

/// [`fanout_to_lists`], for an edit when `update`: a list the filters now
/// keep it from has it taken out (`unpush_from_list`).
async fn fan_out_lists(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    status_id: i64,
    update: bool,
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
        let Some(owner) = owner_crutches.get(&owner_id) else {
            continue;
        };
        let timeline = Timeline::list(keys, list_id);
        let aggregate = !separate.contains(&owner_id);
        // `FeedManager#filter_from_list?`
        let filtered_from_list = status.reply
            && in_reply_to != Some(author)
            && in_reply_to != Some(owner_id)
            && replies_policy != crate::db::models::replies::FOLLOWED
            && !(replies_policy == crate::db::models::replies::LIST && listed);
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
            notifying: false,
        };
        let receiver = Receiver::List {
            id: list_id,
            replies_policy,
        };
        if filtered_from_list || filter_from_home(status, owner_id, &crutches, receiver) {
            if update {
                if let Err(error) =
                    remove_from_feed(redis, &timeline, status_id, status.reblog_of_id, aggregate)
                        .await
                {
                    tracing::warn!(%error, list_id, "could not remove a status from a list feed");
                }
            }
            continue;
        }
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

/// `FeedManager#unmerge_from_home` for a suspended or deleted account: take
/// `from_account_id`'s statuses out of `into_account_id`'s home feed.
pub async fn unmerge_from_home(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    from_account_id: i64,
    into_account_id: i64,
) {
    unmerge_from_home_because(
        redis,
        keys,
        db,
        from_account_id,
        into_account_id,
        reason::ACCOUNT_REMOVED,
    )
    .await;
}

/// `FeedManager#unmerge_from_home`: take `from_account_id`'s statuses out of
/// `into_account_id`'s home feed, as an unfollow, a block or a suspension
/// does, for the `reason` the log gives.
pub async fn unmerge_from_home_because(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    from_account_id: i64,
    into_account_id: i64,
    reason: &'static str,
) {
    unmerge_account(
        redis,
        db,
        from_account_id,
        into_account_id,
        &Timeline::home(keys, into_account_id),
        reason,
    )
    .await;
}

/// `FeedManager#unmerge_from_list` for a suspended or deleted account.
pub async fn unmerge_from_list(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    from_account_id: i64,
    list_id: i64,
) {
    unmerge_from_list_because(
        redis,
        keys,
        db,
        from_account_id,
        list_id,
        reason::ACCOUNT_REMOVED,
    )
    .await;
}

/// `FeedManager#unmerge_from_list`, for the `reason` the log gives.
pub async fn unmerge_from_list_because(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    from_account_id: i64,
    list_id: i64,
    reason: &'static str,
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
        reason,
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
    reason: &'static str,
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
    let removed = unmerge(redis, db, owner_id, timeline, &ids).await;
    if removed > 0 {
        timeline.log_removal(reason, removed, Some(from_account_id));
    }
}

/// `FeedManager#unmerge_tag_from_home`: take out of `into_account_id`'s home
/// feed the posts it holds tagged with `tag_id`, unless they are the
/// account's own, from an account it follows, or tagged with another hashtag
/// it still follows.
pub async fn unmerge_tag_from_home(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
    tag_id: i64,
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
           JOIN statuses_tags st ON st.status_id = s.id AND st.tag_id = $1
           WHERE s.id = ANY($2::bigint[]) AND s.deleted_at IS NULL
             AND s.account_id <> $3
             AND s.account_id NOT IN (SELECT target_account_id FROM follows WHERE account_id = $3)
             AND NOT EXISTS (
               SELECT 1 FROM statuses_tags forbidden
               WHERE forbidden.status_id = s.id
                 AND forbidden.tag_id IN (SELECT tag_id FROM tag_follows WHERE account_id = $3)
             )"#,
        tag_id,
        &members,
        into_account_id,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let removed = unmerge(redis, db, into_account_id, &timeline, &ids).await;
    if removed > 0 {
        timeline.log_removal(reason::TAG_UNFOLLOWED, removed, None);
    }
}

/// [`remove_from_feed`] for each of `ids`, as `unmerge_from_home` runs it:
/// how many the feed held.
async fn unmerge(
    redis: &mut ConnectionManager,
    db: &PgPool,
    owner_id: i64,
    timeline: &Timeline,
    ids: &[i64],
) -> u64 {
    if ids.is_empty() {
        return 0;
    }
    let reblogs = reblogs_of(db, ids).await;
    let aggregate = aggregates(db, owner_id).await;
    let mut removed = 0;
    for &id in ids {
        let reblog_of_id = reblogs.get(&id).copied();
        match remove_from_feed(redis, timeline, id, reblog_of_id, aggregate).await {
            Ok(held) => removed += u64::from(held),
            Err(error) => {
                tracing::warn!(%error, key = %timeline.key, "could not remove a status from a feed");
                break;
            }
        }
    }
    removed
}

/// `FeedManager#clear_from_home` and `#clear_from_list`: take out of the
/// feed every status it holds that `target_account_id` wrote, that boosts
/// one it wrote, or that mentions it (or boosts one that does), each by
/// `unpush_from_home` / `unpush_from_list`, which streams a `delete` on
/// `channel` for each status the feed held.
async fn clear_from(
    state: &crate::state::AppState,
    timeline: &Timeline,
    owner_id: i64,
    target_account_id: i64,
    channel: &str,
) {
    let mut redis = state.redis.clone();
    let db = &state.db;
    let members: Vec<i64> = redis
        .zrange::<_, Vec<i64>>(&timeline.key, 0, -1)
        .await
        .unwrap_or_default();
    if members.is_empty() {
        return;
    }
    let statuses = sqlx::query!(
        "SELECT id, reblog_of_id, account_id FROM statuses
         WHERE id = ANY($1) AND deleted_at IS NULL",
        &members,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let reblog_of_ids: Vec<i64> = statuses.iter().filter_map(|s| s.reblog_of_id).collect();
    let reblogged: HashSet<i64> = sqlx::query_scalar!(
        "SELECT id FROM statuses
         WHERE id = ANY($1) AND account_id = $2 AND deleted_at IS NULL",
        &reblog_of_ids,
        target_account_id,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .collect();
    let mentioning_ids: Vec<i64> = statuses
        .iter()
        .flat_map(|s| std::iter::once(s.id).chain(s.reblog_of_id))
        .collect();
    let with_mentions: HashSet<i64> = sqlx::query_scalar!(
        "SELECT status_id FROM mentions
         WHERE status_id = ANY($1) AND account_id = $2 AND NOT silent",
        &mentioning_ids,
        target_account_id,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .collect();
    let aggregate = aggregates(db, owner_id).await;
    let mut removed = 0;
    for status in statuses {
        let boosts_target = status
            .reblog_of_id
            .is_some_and(|r| reblogged.contains(&r) || with_mentions.contains(&r));
        if status.account_id != target_account_id
            && !boosts_target
            && !with_mentions.contains(&status.id)
        {
            continue;
        }
        match remove_from_feed(
            &mut redis,
            timeline,
            status.id,
            status.reblog_of_id,
            aggregate,
        )
        .await
        {
            Ok(true) => {
                removed += 1;
                state.streaming.delete(channel, status.id).await;
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(%error, key = %timeline.key, "could not clear a feed");
                break;
            }
        }
    }
    if removed > 0 {
        timeline.log_removal(reason::MUTE_OR_BLOCK, removed, Some(target_account_id));
    }
}

/// `FeedManager#clear_from_home`.
pub async fn clear_from_home(
    state: &crate::state::AppState,
    account_id: i64,
    target_account_id: i64,
) {
    clear_from(
        state,
        &Timeline::home(&state.redis_keys, account_id),
        account_id,
        target_account_id,
        &format!("timeline:{account_id}"),
    )
    .await;
}

/// `FeedManager#clear_from_lists`: [`clear_from`] for each of the account's
/// lists.
pub async fn clear_from_lists(
    state: &crate::state::AppState,
    account_id: i64,
    target_account_id: i64,
) {
    let lists: Vec<i64> =
        sqlx::query_scalar!("SELECT id FROM lists WHERE account_id = $1", account_id)
            .fetch_all(&state.db)
            .await
            .unwrap_or_default();
    for list_id in lists {
        clear_from(
            state,
            &Timeline::list(&state.redis_keys, list_id),
            account_id,
            target_account_id,
            &format!("timeline:list:{list_id}"),
        )
        .await;
    }
}

// ── Cleaning ──────────────────────────────────────────────────────────────

/// `FeedManager#clean_feeds!` for one feed: the feed, its tracked boosts, and
/// every set of boosts held back. How many entries the feed held, none when
/// there was no feed (Redis keeps no empty sorted set).
async fn clean_feed(redis: &mut ConnectionManager, timeline: &Timeline) -> u64 {
    let tracked: Vec<i64> = redis
        .zrange(&timeline.reblogs, 0, -1)
        .await
        .unwrap_or_default();
    let mut pipe = redis::pipe();
    pipe.zcard(&timeline.key)
        .del(&timeline.key)
        .ignore()
        .del(&timeline.reblogs)
        .ignore();
    for boosted in tracked {
        pipe.del(timeline.reblog_set(boosted)).ignore();
    }
    match pipe.query_async::<(u64,)>(redis).await {
        Ok((entries,)) => entries,
        Err(error) => {
            tracing::warn!(%error, key = %timeline.key, "could not delete a feed");
            0
        }
    }
}

/// [`clean_feed`], logged with `reason` when there was a feed to delete.
async fn clean_feed_because(
    redis: &mut ConnectionManager,
    timeline: &Timeline,
    reason: &'static str,
) -> u64 {
    let entries = clean_feed(redis, timeline).await;
    if entries > 0 {
        timeline.log_removal(reason, entries, None);
    }
    entries
}

/// `FeedManager#clean_feeds!(:home, [account_id])`, as
/// `DeleteAccountService#purge_feeds!` runs it.
pub async fn delete_home_feed(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    account_id: i64,
) {
    clean_feed_because(
        redis,
        &Timeline::home(keys, account_id),
        reason::ACCOUNT_REMOVED,
    )
    .await;
}

/// `FeedManager#clean_feeds!(:list, [list_id])`, as
/// `DeleteAccountService#purge_feeds!` runs it.
pub async fn delete_list_feed(redis: &mut ConnectionManager, keys: &RedisKeyspace, list_id: i64) {
    delete_list_feed_because(redis, keys, list_id, reason::ACCOUNT_REMOVED).await;
}

/// `FeedManager#clean_feeds!(:list, [list_id])`, for the `reason` the log
/// gives.
pub async fn delete_list_feed_because(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    list_id: i64,
    reason: &'static str,
) {
    clean_feed_because(redis, &Timeline::list(keys, list_id), reason).await;
}

/// `FeedManager::MAX_ITEMS`, for those outside deciding whether a feed is
/// full.
pub const MAX_ITEMS: u64 = FEED_MAX_ITEMS as u64;

/// `FeedManager#timeline_size(:home, account_id)`: how many entries the home
/// feed holds, none when Redis cannot say.
pub async fn home_size(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    account_id: i64,
) -> u64 {
    redis.zcard(feed_key(keys, account_id)).await.unwrap_or(0)
}

/// `FeedManager#timeline_size(:list, list_id)`.
pub async fn list_size(redis: &mut ConnectionManager, keys: &RedisKeyspace, list_id: i64) -> u64 {
    redis.zcard(list_feed_key(keys, list_id)).await.unwrap_or(0)
}

/// `Vacuum::FeedsVacuum`: remove the home feeds and list feeds of confirmed
/// users who have not signed in within [`crate::home_feed::ACTIVE_DAYS`]
/// (`User.confirmed.not_signed_in_recently`), so that they are regenerated
/// when those users return.
///
/// Each feed removed is logged with the user row that put it on the list and
/// its `current_sign_in_at`. One whose account has another user row that
/// signed in recently is logged as a warning: the fan-out counts such an
/// account as active, so its feed is removed from under a member who uses
/// it, as Mastodon's vacuum removes it too.
pub async fn vacuum_inactive_feeds(
    redis: &mut ConnectionManager,
    keys: &RedisKeyspace,
    db: &PgPool,
) -> sqlx::Result<()> {
    const BATCH: i64 = 1000;
    let mut after = 0i64;
    let (mut home_feeds, mut list_feeds) = (0u64, 0u64);
    loop {
        let users = sqlx::query!(
            r#"SELECT u.id, u.account_id, u.current_sign_in_at,
                      EXISTS (SELECT 1 FROM users other
                              WHERE other.account_id = u.account_id AND other.id <> u.id
                                AND other.current_sign_in_at >= now() - make_interval(days => $1))
                        AS "active_elsewhere!"
               FROM users u
               WHERE u.confirmed_at IS NOT NULL
                 AND u.current_sign_in_at < now() - make_interval(days => $1)
                 AND u.id > $2
               ORDER BY u.id LIMIT $3"#,
            crate::home_feed::ACTIVE_DAYS,
            after,
            BATCH,
        )
        .fetch_all(db)
        .await?;
        let Some(last) = users.last() else {
            tracing::info!(home_feeds, list_feeds, "feeds vacuum finished");
            return Ok(());
        };
        after = last.id;
        for user in &users {
            let entries = clean_feed(redis, &Timeline::home(keys, user.account_id)).await;
            if entries == 0 {
                continue;
            }
            home_feeds += 1;
            if user.active_elsewhere {
                tracing::warn!(
                    account_id = user.account_id,
                    user_id = user.id,
                    current_sign_in_at = ?user.current_sign_in_at,
                    entries,
                    reason = reason::INACTIVE,
                    "home feed of an account another user keeps active removed as inactive"
                );
            } else {
                tracing::info!(
                    account_id = user.account_id,
                    user_id = user.id,
                    current_sign_in_at = ?user.current_sign_in_at,
                    entries,
                    reason = reason::INACTIVE,
                    "home feed entries removed"
                );
            }
        }
        let account_ids: Vec<i64> = users.iter().map(|u| u.account_id).collect();
        let lists = sqlx::query!(
            "SELECT id, account_id FROM lists WHERE account_id = ANY($1)",
            &account_ids,
        )
        .fetch_all(db)
        .await?;
        for list in lists {
            let entries = clean_feed(redis, &Timeline::list(keys, list.id)).await;
            if entries > 0 {
                list_feeds += 1;
                tracing::info!(
                    list_id = list.id,
                    account_id = list.account_id,
                    entries,
                    reason = reason::INACTIVE,
                    "list feed entries removed"
                );
            }
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
