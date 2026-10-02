//! The streaming half of Mastodon's status distribution: what
//! `FanOutOnWriteService` (through `FeedInsertWorker`, `FeedManager` and
//! `PushUpdateWorker`) and `RemoveStatusService` publish on the `timeline:*`
//! channels, and the other Rails publishers that render for one account —
//! notifications, conversations and announcements.
//!
//! Eunha's feeds are built elsewhere (`crate::feed`); this only decides, for
//! the accounts and lists somebody is streaming, what Mastodon would have
//! pushed to them. That is decided per subscriber rather than per follower,
//! which gives the same messages, since Mastodon only pushes to a timeline
//! with a subscriber (`push_update_required?`).

use serde_json::{json, Value};

use crate::db::models::{vis, Status as DbStatus};
use crate::state::AppState;

/// `Status::REAL_TIME_WINDOW`: a remote post older than this is stored but not
/// distributed.
const REAL_TIME_WINDOW_HOURS: i64 = 6;

/// `User::ACTIVE_DURATION` (`USER_ACTIVE_DAYS`, 7 days by default).
const ACTIVE_DAYS: i32 = 7;

/// What a status needs for the feed filters.
struct Subject {
    status: DbStatus,
    local: bool,
    author_silenced: bool,
    author_domain: Option<String>,
    /// The boosted post's author and domain; `None` for a boost whose post is
    /// gone as much as for a post that is no boost.
    reblog: Option<(i64, Option<String>)>,
    /// `crutches[:active_mentions]` for the status and the post it boosts.
    mentions: Vec<i64>,
    tag_names: Vec<String>,
    tag_ids: Vec<i64>,
}

async fn subject(state: &AppState, status_id: i64) -> anyhow::Result<Option<Subject>> {
    let Some(status) = sqlx::query_as!(DbStatus, "SELECT * FROM statuses WHERE id = $1", status_id)
        .fetch_optional(&state.db)
        .await?
    else {
        return Ok(None);
    };
    let author = sqlx::query!(
        r#"SELECT domain, silenced_at IS NOT NULL AS "silenced!",
                  suspended_at IS NOT NULL AS "suspended!"
           FROM accounts WHERE id = $1"#,
        status.account_id
    )
    .fetch_one(&state.db)
    .await?;
    let reblog = match status.reblog_of_id {
        Some(id) => sqlx::query!(
            r#"SELECT s.account_id, a.domain, a.suspended_at IS NOT NULL AS "suspended!"
               FROM statuses s JOIN accounts a ON a.id = s.account_id
               WHERE s.id = $1 AND s.deleted_at IS NULL"#,
            id
        )
        .fetch_optional(&state.db)
        .await?
        .map(|r| (r.account_id, r.domain, r.suspended)),
        None => None,
    };
    // `return if @status.proper.account.suspended?`
    let proper_suspended = match &reblog {
        Some((_, _, suspended)) => *suspended,
        None => author.suspended,
    };
    if proper_suspended {
        return Ok(None);
    }
    let mention_of: Vec<i64> = std::iter::once(status.id)
        .chain(status.reblog_of_id)
        .collect();
    let mentions = sqlx::query_scalar!(
        "SELECT account_id FROM mentions WHERE status_id = ANY($1) AND NOT silent",
        &mention_of
    )
    .fetch_all(&state.db)
    .await?;
    let tags = sqlx::query!(
        r#"SELECT t.id, t.name FROM statuses_tags st JOIN tags t ON t.id = st.tag_id
           WHERE st.status_id = $1"#,
        status.id
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Some(Subject {
        local: author.domain.is_none(),
        author_silenced: author.silenced,
        author_domain: author.domain,
        reblog: reblog.map(|(id, domain, _)| (id, domain)),
        mentions,
        tag_names: tags.iter().map(|t| t.name.clone()).collect(),
        tag_ids: tags.iter().map(|t| t.id).collect(),
        status,
    }))
}

/// The status rendered for `viewer`, as the REST API renders it, which is
/// what `StatusCacheHydrator#hydrate` reproduces; or for nobody, as
/// `InlineRenderer.render(status, nil, :status)` renders it.
async fn render(state: &AppState, status: &DbStatus, viewer: Option<i64>) -> Option<Value> {
    let rendered = crate::api::mastodon::timelines::build_status_list_with_filters(
        state,
        vec![status.clone()],
        viewer,
    )
    .await
    .ok()?;
    serde_json::to_value(rendered.into_iter().next()?).ok()
}

fn status_event(update: bool) -> &'static str {
    if update {
        "status.update"
    } else {
        "update"
    }
}

/// `DistributionWorker` → `FanOutOnWriteService#call`, for its streaming
/// messages: the author's own home, followers' homes and lists, followers of
/// its hashtags, the mentioned accounts on an edit, and the public and
/// hashtag streams. Call it after the feeds were written.
pub async fn distribute(state: &AppState, status_id: i64, update: bool) {
    if let Err(error) = try_distribute(state, status_id, update).await {
        tracing::warn!(%error, status_id, "could not stream a status");
    }
}

async fn try_distribute(state: &AppState, status_id: i64, update: bool) -> anyhow::Result<()> {
    let Some(s) = subject(state, status_id).await? else {
        return Ok(());
    };
    if s.status.deleted_at.is_some() {
        return Ok(());
    }
    // `ActivityPub::Activity#distribute`: only a post that arrived in real
    // time is distributed.
    if !s.local
        && !update
        && chrono::Utc::now().naive_utc() - s.status.created_at
            > chrono::Duration::hours(REAL_TIME_WINDOW_HOURS)
    {
        return Ok(());
    }
    let bus = &state.streaming;
    let event = status_event(update);

    // Home timelines: `deliver_to_self!`, `deliver_to_all_followers!`,
    // `deliver_to_mentioned_followers!` and `deliver_to_hashtag_followers!`,
    // each a `FeedInsertWorker` that filters and then `push_to_home`s.
    let broadcastable =
        s.status.visibility == vis::PUBLIC && s.status.reblog_of_id.is_none() && !s.author_silenced;
    // Who is local, signed in recently, and the author, a follower or a
    // follower of one of its hashtags; then, as `push_update_required?`
    // asks, who among them is streaming their home.
    let candidates = sqlx::query_scalar!(
        r#"SELECT u.account_id FROM users u JOIN accounts a ON a.id = u.account_id
           WHERE a.domain IS NULL
             AND u.current_sign_in_at >= now() - make_interval(days => $4)
             AND (u.account_id = $1
                  OR EXISTS (SELECT 1 FROM follows f
                             WHERE f.account_id = u.account_id AND f.target_account_id = $1)
                  OR ($2 AND EXISTS (SELECT 1 FROM tag_follows tf
                                     WHERE tf.account_id = u.account_id AND tf.tag_id = ANY($3))))"#,
        s.status.account_id,
        broadcastable,
        &s.tag_ids,
        ACTIVE_DAYS,
    )
    .fetch_all(&state.db)
    .await?;
    let receivers = bus.subscribed_ids("timeline:", candidates).await;
    for receiver in receivers {
        for kind in home_deliveries(state, &s, receiver, broadcastable).await? {
            let pushed = match kind {
                Delivery::Self_ => true,
                Delivery::Follower => filter_from_home(state, &s, receiver, None).await?.is_none(),
                Delivery::Tags => !filter_from_tags(state, &s, receiver).await?,
            };
            if pushed && push_to_home(state, receiver, s.status.id).await? {
                if let Some(payload) = render(state, &s.status, Some(receiver)).await {
                    bus.publish(
                        &format!("timeline:{receiver}"),
                        json!({"event": event, "payload": payload}),
                    )
                    .await;
                }
            }
        }
    }

    // `deliver_to_lists!`.
    if matches!(
        s.status.visibility,
        vis::PUBLIC | vis::UNLISTED | vis::PRIVATE
    ) {
        let lists = sqlx::query_scalar!(
            "SELECT DISTINCT list_id FROM list_accounts WHERE account_id = $1",
            s.status.account_id
        )
        .fetch_all(&state.db)
        .await?;
        for list_id in bus.subscribed_ids("timeline:list:", lists).await {
            let Some(list) = distributing_list(state, list_id, s.status.account_id).await? else {
                continue;
            };
            if filter_from_list(state, &s, &list).await? {
                continue;
            }
            if filter_from_home(state, &s, list.account_id, Some(&list))
                .await?
                .is_some()
            {
                continue;
            }
            if !crate::feed::list_would_hold(
                &mut state.redis.clone(),
                &state.redis_keys,
                list_id,
                s.status.id,
            )
            .await
            {
                continue;
            }
            if let Some(payload) = render(state, &s.status, Some(list.account_id)).await {
                bus.publish(
                    &format!("timeline:list:{list_id}"),
                    json!({"event": event, "payload": payload}),
                )
                .await;
            }
        }
    }

    // `notify_mentioned_accounts!` on an edit: the edit reaches the mentioned
    // accounts' notification streams, whether or not it reached their homes.
    if update {
        let mentioned = sqlx::query_scalar!(
            r#"SELECT m.account_id FROM mentions m JOIN accounts a ON a.id = m.account_id
               WHERE m.status_id = $1 AND NOT m.silent AND a.domain IS NULL"#,
            s.status.id
        )
        .fetch_all(&state.db)
        .await?;
        for account_id in mentioned {
            if !bus.is_online(account_id).await {
                continue;
            }
            if let Some(payload) = render(state, &s.status, Some(account_id)).await {
                bus.publish(
                    &format!("timeline:{account_id}:notifications"),
                    json!({"event": "status.update", "payload": payload}),
                )
                .await;
            }
        }
    }

    // `fan_out_to_public_streams! if broadcastable?`.
    if broadcastable {
        let mut channels: Vec<String> = vec![];
        for name in &s.tag_names {
            let name = name.to_lowercase();
            channels.push(format!("timeline:hashtag:{name}"));
            if s.local {
                channels.push(format!("timeline:hashtag:{name}:local"));
            }
        }
        // `return if @status.reply? && @status.in_reply_to_account_id != @account.id`
        let reply_to_other = (s.status.reply || s.status.in_reply_to_id.is_some())
            && s.status.in_reply_to_account_id != Some(s.status.account_id);
        let mut media_channels = false;
        if !reply_to_other {
            channels.push("timeline:public".into());
            channels.push(if s.local {
                "timeline:public:local".into()
            } else {
                "timeline:public:remote".into()
            });
            media_channels = true;
        }
        if bus.subscribed(&channels).await.into_iter().any(|yes| yes)
            || (media_channels && any_media_subscriber(state).await)
        {
            if let Some(payload) = render(state, &s.status, None).await {
                let with_media = payload["media_attachments"]
                    .as_array()
                    .is_some_and(|m| !m.is_empty());
                if media_channels && with_media {
                    channels.push("timeline:public:media".into());
                    channels.push(if s.local {
                        "timeline:public:local:media".into()
                    } else {
                        "timeline:public:remote:media".into()
                    });
                }
                let message = json!({"event": event, "payload": payload});
                for channel in channels {
                    bus.publish(&channel, message.clone()).await;
                }
            }
        }
    }
    Ok(())
}

async fn any_media_subscriber(state: &AppState) -> bool {
    state
        .streaming
        .subscribed(&[
            "timeline:public:media".into(),
            "timeline:public:local:media".into(),
            "timeline:public:remote:media".into(),
        ])
        .await
        .into_iter()
        .any(|yes| yes)
}

enum Delivery {
    /// `deliver_to_self!`: no filter.
    Self_,
    /// A `home` `FeedInsertWorker`: `FeedManager#filter_from_home`.
    Follower,
    /// A `tags` `FeedInsertWorker`: `FeedManager#filter_from_tags?`.
    Tags,
}

/// The `FeedInsertWorker`s `FanOutOnWriteService` queues for `receiver`, a
/// local account signed in recently. One account can get two — a follower
/// who also follows one of the post's hashtags — and Mastodon then pushes the
/// update twice.
async fn home_deliveries(
    state: &AppState,
    s: &Subject,
    receiver: i64,
    broadcastable: bool,
) -> anyhow::Result<Vec<Delivery>> {
    let mut deliveries = vec![];
    if receiver == s.status.account_id {
        // `deliver_to_self!` (`if @account.local?`; the receiver is local).
        deliveries.push(Delivery::Self_);
    } else {
        let follows = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2) AS "e!""#,
            receiver,
            s.status.account_id
        )
        .fetch_one(&state.db)
        .await?;
        let to_followers = match s.status.visibility {
            vis::PUBLIC | vis::UNLISTED | vis::PRIVATE => follows,
            // `deliver_to_mentioned_followers!`.
            _ => {
                follows
                    && sqlx::query_scalar!(
                        r#"SELECT EXISTS (SELECT 1 FROM mentions WHERE status_id = $1 AND account_id = $2) AS "e!""#,
                        s.status.id,
                        receiver
                    )
                    .fetch_one(&state.db)
                    .await?
            }
        };
        if to_followers {
            deliveries.push(Delivery::Follower);
        }
    }
    // `deliver_to_hashtag_followers!`, the author included.
    if broadcastable && !s.tag_ids.is_empty() {
        let follows_tag = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM tag_follows WHERE account_id = $1 AND tag_id = ANY($2)) AS "e!""#,
            receiver,
            &s.tag_ids
        )
        .fetch_one(&state.db)
        .await?;
        if follows_tag {
            deliveries.push(Delivery::Tags);
        }
    }
    Ok(deliveries)
}

/// `FeedManager#push_to_home`'s `add_to_feed`, read after `crate::feed` wrote
/// the feed: the home feed holds the status, or was never built and so had
/// nothing to keep it out.
async fn push_to_home(state: &AppState, receiver: i64, status_id: i64) -> anyhow::Result<bool> {
    Ok(crate::feed::home_would_hold(
        &mut state.redis.clone(),
        &state.redis_keys,
        receiver,
        status_id,
    )
    .await)
}

struct DistributingList {
    id: i64,
    account_id: i64,
    /// `enum :replies_policy, { list: 0, followed: 1, none: 2 }`.
    replies_policy: i32,
}

/// The list, when it is among the author's `lists_for_local_distribution`:
/// it holds the author through a follow, or belongs to the author, and its
/// owner signed in recently.
async fn distributing_list(
    state: &AppState,
    list_id: i64,
    author_id: i64,
) -> anyhow::Result<Option<DistributingList>> {
    Ok(sqlx::query_as!(
        DistributingList,
        r#"SELECT l.id, l.account_id, l.replies_policy
           FROM lists l
           JOIN list_accounts la ON la.list_id = l.id
           JOIN users u ON u.account_id = l.account_id
           WHERE l.id = $1 AND la.account_id = $2
             AND (la.follow_id IS NOT NULL OR l.account_id = $2)
             AND u.current_sign_in_at >= now() - make_interval(days => $3)
           LIMIT 1"#,
        list_id,
        author_id,
        ACTIVE_DAYS,
    )
    .fetch_optional(&state.db)
    .await?)
}

/// `FeedManager#filter_from_list?`.
async fn filter_from_list(
    state: &AppState,
    s: &Subject,
    list: &DistributingList,
) -> anyhow::Result<bool> {
    let st = &s.status;
    let is_reply = st.reply || st.in_reply_to_id.is_some();
    if is_reply && st.in_reply_to_account_id != Some(st.account_id) {
        let mut should_filter = st.in_reply_to_account_id != Some(list.account_id);
        should_filter &= list.replies_policy != 1;
        if should_filter && list.replies_policy == 0 {
            let listed = match st.in_reply_to_account_id {
                Some(target) => sqlx::query_scalar!(
                    r#"SELECT EXISTS (SELECT 1 FROM list_accounts WHERE list_id = $1 AND account_id = $2) AS "e!""#,
                    list.id,
                    target
                )
                .fetch_one(&state.db)
                .await?,
                None => false,
            };
            should_filter &= !listed;
        }
        return Ok(should_filter);
    }
    Ok(false)
}

#[derive(Debug, PartialEq, Eq)]
enum Filtered {
    Filter,
    SkipHome,
}

async fn blocks_or_mutes_any(
    state: &AppState,
    receiver: i64,
    targets: &[i64],
) -> anyhow::Result<bool> {
    // `crutches[:blocking]` and `crutches[:muting]`: every mute, expired or
    // not, as the crutches read them.
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = ANY($2)
             UNION ALL
             SELECT 1 FROM mutes WHERE account_id = $1 AND target_account_id = ANY($2)
           ) AS "e!""#,
        receiver,
        targets
    )
    .fetch_one(&state.db)
    .await?)
}

async fn blocked_by(state: &AppState, receiver: i64, account_id: i64) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = $2) AS "e!""#,
        account_id,
        receiver
    )
    .fetch_one(&state.db)
    .await?)
}

async fn domain_blocking(
    state: &AppState,
    receiver: i64,
    domain: Option<&str>,
) -> anyhow::Result<bool> {
    let Some(domain) = domain else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM account_domain_blocks WHERE account_id = $1 AND domain = $2) AS "e!""#,
        receiver,
        domain
    )
    .fetch_one(&state.db)
    .await?)
}

/// `FeedManager#filter_from_home`, with the crutches `build_crutches` makes
/// for one status (`list` for a list feed).
async fn filter_from_home(
    state: &AppState,
    s: &Subject,
    receiver: i64,
    list: Option<&DistributingList>,
) -> anyhow::Result<Option<Filtered>> {
    let st = &s.status;
    if receiver == st.account_id {
        return Ok(None);
    }
    let is_reply = st.reply || st.in_reply_to_id.is_some();
    if is_reply && (st.in_reply_to_id.is_none() || st.in_reply_to_account_id.is_none()) {
        return Ok(Some(Filtered::Filter));
    }
    if list.is_none() {
        // `crutches[:exclusive_list_users]`.
        let exclusive = sqlx::query_scalar!(
            r#"SELECT EXISTS (
                 SELECT 1 FROM list_accounts la JOIN lists l ON l.id = la.list_id
                 WHERE l.account_id = $1 AND l.exclusive AND la.account_id = $2
               ) AS "e!""#,
            receiver,
            st.account_id
        )
        .fetch_one(&state.db)
        .await?;
        if exclusive {
            return Ok(Some(Filtered::SkipHome));
        }
    }
    // `crutches[:languages]`.
    if let Some(language) = st.language.as_deref().filter(|l| !l.is_empty()) {
        let languages = sqlx::query_scalar!(
            "SELECT languages FROM follows WHERE account_id = $1 AND target_account_id = $2",
            receiver,
            st.account_id
        )
        .fetch_optional(&state.db)
        .await?
        .flatten()
        .unwrap_or_default();
        if !languages.is_empty() && !languages.iter().any(|l| l == language) {
            return Ok(Some(Filtered::Filter));
        }
    }
    if st.reblog_of_id.is_some() && s.reblog.is_none() {
        return Ok(Some(Filtered::Filter));
    }
    let mut check_for_blocks = s.mentions.clone();
    check_for_blocks.push(st.account_id);
    if let Some((reblog_author, _)) = &s.reblog {
        check_for_blocks.push(*reblog_author);
    }
    if blocks_or_mutes_any(state, receiver, &check_for_blocks).await? {
        return Ok(Some(Filtered::Filter));
    }
    if blocked_by(state, receiver, st.account_id).await? {
        return Ok(Some(Filtered::Filter));
    }
    let should_filter = if let (true, Some(reply_to)) = (is_reply, st.in_reply_to_account_id) {
        // `crutches[:following]`.
        let following = match list {
            Some(list) if list.replies_policy == 0 => {
                sqlx::query_scalar!(
                    r#"SELECT EXISTS (SELECT 1 FROM list_accounts WHERE list_id = $1 AND account_id = $2) AS "e!""#,
                    list.id,
                    reply_to
                )
                .fetch_one(&state.db)
                .await?
            }
            Some(list) if list.replies_policy != 1 => false,
            _ => {
                sqlx::query_scalar!(
                    r#"SELECT EXISTS (SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2) AS "e!""#,
                    receiver,
                    reply_to
                )
                .fetch_one(&state.db)
                .await?
            }
        };
        !following && receiver != reply_to && st.account_id != reply_to
    } else if let Some((reblog_author, reblog_domain)) = &s.reblog {
        let hiding_reblogs = sqlx::query_scalar!(
            r#"SELECT EXISTS (
                 SELECT 1 FROM follows
                 WHERE account_id = $1 AND target_account_id = $2 AND NOT show_reblogs
               ) AS "e!""#,
            receiver,
            st.account_id
        )
        .fetch_one(&state.db)
        .await?;
        hiding_reblogs
            || blocked_by(state, receiver, *reblog_author).await?
            || domain_blocking(state, receiver, reblog_domain.as_deref()).await?
    } else {
        false
    };
    Ok(should_filter.then_some(Filtered::Filter))
}

/// `FeedManager#filter_from_tags?`.
async fn filter_from_tags(state: &AppState, s: &Subject, receiver: i64) -> anyhow::Result<bool> {
    let st = &s.status;
    if receiver == st.account_id {
        return Ok(true);
    }
    let mut targets: Vec<i64> = s.mentions.clone();
    targets.push(st.account_id);
    Ok(blocks_or_mutes_any(state, receiver, &targets).await?
        || blocked_by(state, receiver, st.account_id).await?
        || domain_blocking(state, receiver, s.author_domain.as_deref()).await?)
}

/// `RemoveStatusService#call`, for its streaming messages: `delete` to the
/// homes and lists the status was pushed to, the accounts it mentions, and
/// the public and hashtag streams; then the same for each boost of it
/// discarded with it. Call it after the status was discarded and before the
/// feeds let go of it.
pub async fn remove(state: &AppState, status_id: i64) {
    if let Err(error) = try_remove(state, status_id, true).await {
        tracing::warn!(%error, status_id, "could not stream a deletion");
    }
}

/// `BatchedRemoveStatusService`'s streaming messages for one status: the
/// homes, lists and public and hashtag streams, but not the mentioned
/// accounts, and its boosts are removed by the caller.
pub async fn remove_batched(state: &AppState, status_id: i64) {
    if let Err(error) = try_remove(state, status_id, false).await {
        tracing::warn!(%error, status_id, "could not stream a deletion");
    }
}

/// [`remove`] for a boost already deleted from the database: `delete` to the
/// homes and lists it was pushed to. Call it before the feeds let go of it.
pub async fn remove_boost(state: &AppState, boost_id: i64, booster_id: i64) {
    let result = async {
        let local = sqlx::query_scalar!(
            r#"SELECT domain IS NULL AS "local!" FROM accounts WHERE id = $1"#,
            booster_id
        )
        .fetch_one(&state.db)
        .await?;
        unpush(state, boost_id, booster_id, local).await
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, boost_id, "could not stream a deletion");
    }
}

/// `remove_from_self if @account.local?`, `remove_from_followers` and
/// `remove_from_lists`: `FeedManager#unpush_from_home` and
/// `#unpush_from_list`, which publish `delete` where the status was in the
/// feed. A feed never built may have been shown it too.
async fn unpush(
    state: &AppState,
    status_id: i64,
    author_id: i64,
    local: bool,
) -> anyhow::Result<()> {
    let bus = &state.streaming;
    {
        let candidates = sqlx::query_scalar!(
            r#"SELECT u.account_id FROM users u
               WHERE (u.account_id = $1 AND $2)
                  OR (u.current_sign_in_at >= now() - make_interval(days => $3)
                      AND EXISTS (SELECT 1 FROM follows f
                                  WHERE f.account_id = u.account_id AND f.target_account_id = $1))"#,
            author_id,
            local,
            ACTIVE_DAYS,
        )
        .fetch_all(&state.db)
        .await?;
        for account_id in bus.subscribed_ids("timeline:", candidates).await {
            if crate::feed::home_would_hold(
                &mut state.redis.clone(),
                &state.redis_keys,
                account_id,
                status_id,
            )
            .await
            {
                bus.delete(&format!("timeline:{account_id}"), status_id)
                    .await;
            }
        }
    }

    {
        let candidates = sqlx::query_scalar!(
            r#"SELECT DISTINCT l.id FROM lists l
               JOIN list_accounts la ON la.list_id = l.id
               JOIN users u ON u.account_id = l.account_id
               WHERE la.account_id = $1
                 AND (la.follow_id IS NOT NULL OR l.account_id = $1)
                 AND u.current_sign_in_at >= now() - make_interval(days => $2)"#,
            author_id,
            ACTIVE_DAYS,
        )
        .fetch_all(&state.db)
        .await?;
        for list_id in bus.subscribed_ids("timeline:list:", candidates).await {
            if crate::feed::list_would_hold(
                &mut state.redis.clone(),
                &state.redis_keys,
                list_id,
                status_id,
            )
            .await
            {
                bus.delete(&format!("timeline:list:{list_id}"), status_id)
                    .await;
            }
        }
    }
    Ok(())
}

/// `whole` is a `RemoveStatusService` of the status itself, which also
/// streams to the mentioned accounts and removes the boosts.
async fn try_remove(state: &AppState, status_id: i64, whole: bool) -> anyhow::Result<()> {
    let Some(status) = sqlx::query!(
        r#"SELECT s.id, s.account_id, s.visibility, s.reblog_of_id, s.deleted_at,
                  a.domain IS NULL AS "local!",
                  (s.ordered_media_attachment_ids IS NOT NULL
                   AND cardinality(s.ordered_media_attachment_ids) > 0
                   OR EXISTS (SELECT 1 FROM media_attachments m WHERE m.status_id = s.id)) AS "with_media!"
           FROM statuses s JOIN accounts a ON a.id = s.account_id WHERE s.id = $1"#,
        status_id
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let bus = &state.streaming;
    unpush(state, status.id, status.account_id, status.local).await?;

    // A boost mentions nobody and carries no hashtags or media.
    if status.reblog_of_id.is_some() {
        return Ok(());
    }

    // `remove_from_mentions`.
    let mentioned = sqlx::query_scalar!(
        "SELECT account_id FROM mentions WHERE status_id = $1 AND NOT silent",
        status.id
    )
    .fetch_all(&state.db)
    .await?;
    if whole {
        for account_id in mentioned {
            bus.delete(&format!("timeline:{account_id}"), status.id)
                .await;
        }
    }

    // `remove_reblogs`: the boosts discarded with it, each its own removal,
    // which for a boost is only its homes and lists.
    if whole {
        let reblogs = sqlx::query_scalar!(
            r#"SELECT id FROM statuses
               WHERE reblog_of_id = $1 AND (deleted_at IS NULL OR deleted_at = $2)"#,
            status.id,
            status.deleted_at,
        )
        .fetch_all(&state.db)
        .await?;
        for reblog in reblogs {
            Box::pin(try_remove(state, reblog, false)).await?;
        }
    }

    if status.visibility == vis::PUBLIC {
        // `remove_from_hashtags`.
        let tags = sqlx::query_scalar!(
            r#"SELECT t.name FROM statuses_tags st JOIN tags t ON t.id = st.tag_id
               WHERE st.status_id = $1"#,
            status.id
        )
        .fetch_all(&state.db)
        .await?;
        for name in tags {
            let name = name.to_lowercase();
            bus.delete(&format!("timeline:hashtag:{name}"), status.id)
                .await;
            if status.local {
                bus.delete(&format!("timeline:hashtag:{name}:local"), status.id)
                    .await;
            }
        }
        // `remove_from_public` and `remove_from_media if @status.with_media?`.
        let scope = if status.local { "local" } else { "remote" };
        bus.delete("timeline:public", status.id).await;
        bus.delete(&format!("timeline:public:{scope}"), status.id)
            .await;
        if status.with_media {
            bus.delete("timeline:public:media", status.id).await;
            bus.delete(&format!("timeline:public:{scope}:media"), status.id)
                .await;
        }
    }
    Ok(())
}

/// `NotifyService#push_notification!`'s streaming half: the notification as
/// rendered for its recipient, when the recipient is streaming.
pub async fn notification(state: &AppState, recipient_id: i64, notification_id: i64) {
    if !state.streaming.is_online(recipient_id).await {
        return;
    }
    let Some(rendered) =
        crate::api::mastodon::notifications::render_notification(state, notification_id).await
    else {
        return;
    };
    if let Ok(payload) = serde_json::from_str::<Value>(&rendered) {
        state.streaming.notification(recipient_id, payload).await;
    }
}

/// The accounts `FeedManager#with_active_accounts` yields that are streaming
/// their home timeline: who an announcement message goes to.
async fn active_home_subscribers(state: &AppState) -> anyhow::Result<Vec<i64>> {
    let active = sqlx::query_scalar!(
        r#"SELECT account_id FROM users
           WHERE current_sign_in_at >= now() - make_interval(days => $1)"#,
        ACTIVE_DAYS,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(state.streaming.subscribed_ids("timeline:", active).await)
}

/// `PublishScheduledAnnouncementWorker`, `UnpublishAnnouncementWorker` and
/// `PublishAnnouncementReactionWorker`: one message, the same for everyone,
/// to each active account's `timeline:<id>` with a subscriber.
pub async fn to_active_accounts(state: &AppState, message: Value) {
    match active_home_subscribers(state).await {
        Ok(accounts) => {
            for account_id in accounts {
                state
                    .streaming
                    .publish(&format!("timeline:{account_id}"), message.clone())
                    .await;
            }
        }
        Err(error) => tracing::warn!(%error, "could not stream an announcement"),
    }
}
