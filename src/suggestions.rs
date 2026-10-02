//! Mastodon's `AccountSuggestions`: whom to suggest following, from
//! `config/settings`'s `bootstrap_timeline_accounts` (`featured`), from the
//! accounts followed by those one follows (`friends_of_friends`), and from the
//! server's most followed and most interacted-with accounts
//! (`global_follow_recommendations`, which the daily
//! `Scheduler::FollowRecommendationsScheduler` refreshes).
//! With the `fasp` feature on, the accounts a FASP recommended to the viewer
//! (`fasp`) join them. `SimilarProfilesSource` needs Elasticsearch, which
//! eunha does not have.

use std::collections::HashMap;
use std::time::Duration;

use crate::state::AppState;

/// `AccountSuggestions::BATCH_SIZE`: how many each source gives.
const BATCH_SIZE: i64 = 40;

/// How long a viewer's suggestions keep their order, as Mastodon caches the
/// shuffled list for fifteen minutes.
const ORDER_KEPT_FOR: i64 = 15 * 60;

/// `AccountSuggestions::Source#base_account_scope`, as SQL over `accounts a`
/// for the viewer `$1`: searchable, discoverable, not limited or a memorial,
/// not followed or requested, not blocked or muted either way, not from a
/// domain the viewer blocked, not the viewer, not dismissed.
const BASE_SCOPE: &str = r#"
    a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
    AND a.moved_to_account_id IS NULL
    AND (a.domain IS NOT NULL OR EXISTS (
          SELECT 1 FROM users u WHERE u.account_id = a.id
            AND u.approved AND u.confirmed_at IS NOT NULL))
    AND a.discoverable AND a.silenced_at IS NULL AND NOT a.memorial
    AND NOT EXISTS (SELECT 1 FROM follows f WHERE f.target_account_id = a.id AND f.account_id = $1)
    AND NOT EXISTS (SELECT 1 FROM follow_requests f WHERE f.target_account_id = a.id AND f.account_id = $1)
    AND NOT EXISTS (SELECT 1 FROM blocks b WHERE (b.account_id = $1 AND b.target_account_id = a.id)
                                            OR (b.account_id = a.id AND b.target_account_id = $1))
    AND NOT EXISTS (SELECT 1 FROM mutes m WHERE m.account_id = $1 AND m.target_account_id = a.id)
    AND (a.domain IS NULL OR a.domain NOT IN (
          SELECT domain FROM account_domain_blocks WHERE account_id = $1))
    AND a.id <> $1
    AND NOT EXISTS (SELECT 1 FROM follow_recommendation_mutes r
                    WHERE r.target_account_id = a.id AND r.account_id = $1)"#;

/// `AccountSuggestions::SettingSource`: the accounts the setting names, as
/// `username` or `username@domain`.
async fn featured(state: &AppState, viewer: i64) -> anyhow::Result<Vec<i64>> {
    let setting = crate::settings::string(state, "bootstrap_timeline_accounts").await;
    let mut usernames = vec![];
    let mut domains = vec![];
    for entry in setting.split(',') {
        let entry = entry.trim().trim_start_matches('@');
        let (username, domain) = match entry.split_once('@') {
            Some((u, d)) => (u, Some(d)),
            None => (entry, None),
        };
        if username.trim().is_empty() {
            continue;
        }
        // `TagManager#local_domain?`.
        let domain = domain.filter(|d| {
            !d.eq_ignore_ascii_case(&state.instance.domain)
                && !state
                    .instance
                    .aliases
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(d))
        });
        usernames.push(username.to_lowercase());
        domains.push(domain.map(str::to_lowercase).unwrap_or_default());
    }
    if usernames.is_empty() {
        return Ok(vec![]);
    }
    let sql = format!(
        "SELECT a.id FROM accounts a
         JOIN unnest($2::text[], $3::text[]) AS wanted(username, domain)
           ON lower(a.username) = wanted.username
          AND COALESCE(lower(a.domain), '') = wanted.domain
         WHERE {BASE_SCOPE}
         LIMIT $4"
    );
    Ok(sqlx::query_scalar(&sql)
        .bind(viewer)
        .bind(&usernames)
        .bind(&domains)
        .bind(BATCH_SIZE)
        .fetch_all(&state.db)
        .await?)
}

/// `AccountSuggestions::FriendsOfFriendsSource#source_query`.
async fn friends_of_friends(state: &AppState, viewer: i64) -> anyhow::Result<Vec<i64>> {
    Ok(sqlx::query_scalar!(
        r#"WITH first_degree AS (
             SELECT target_account_id FROM follows
             JOIN accounts AS target_accounts ON follows.target_account_id = target_accounts.id
             WHERE account_id = $1 AND NOT target_accounts.hide_collections
           )
           SELECT accounts.id
           FROM accounts
           JOIN follows ON follows.target_account_id = accounts.id
           JOIN account_stats ON account_stats.account_id = accounts.id
           LEFT OUTER JOIN follow_recommendation_mutes
             ON follow_recommendation_mutes.target_account_id = accounts.id
            AND follow_recommendation_mutes.account_id = $1
           WHERE follows.account_id IN (SELECT * FROM first_degree)
             AND NOT EXISTS (SELECT 1 FROM blocks b WHERE b.target_account_id = follows.target_account_id AND b.account_id = $1)
             AND NOT EXISTS (SELECT 1 FROM blocks b WHERE b.target_account_id = $1 AND b.account_id = follows.target_account_id)
             AND NOT EXISTS (SELECT 1 FROM mutes m WHERE m.target_account_id = follows.target_account_id AND m.account_id = $1)
             AND (accounts.domain IS NULL OR NOT EXISTS (SELECT 1 FROM account_domain_blocks b WHERE b.account_id = $1 AND b.domain = accounts.domain))
             AND NOT EXISTS (SELECT 1 FROM follows f WHERE f.target_account_id = follows.target_account_id AND f.account_id = $1)
             AND NOT EXISTS (SELECT 1 FROM follow_requests f WHERE f.target_account_id = follows.target_account_id AND f.account_id = $1)
             AND follows.target_account_id <> $1
             AND accounts.discoverable
             AND accounts.suspended_at IS NULL
             AND accounts.silenced_at IS NULL
             AND accounts.moved_to_account_id IS NULL
             AND accounts.memorial = FALSE
             AND follow_recommendation_mutes.target_account_id IS NULL
           GROUP BY accounts.id, account_stats.id
           ORDER BY COUNT(*) DESC, account_stats.followers_count ASC
           LIMIT $2"#,
        viewer,
        BATCH_SIZE,
    )
    .fetch_all(&state.db)
    .await?)
}

/// `AccountSuggestions::GlobalSource`: the recommendations, those whose
/// accounts mostly post in the viewer's language first, by rank, with their
/// reasons.
async fn global(
    state: &AppState,
    viewer: i64,
    locale: &str,
) -> anyhow::Result<Vec<(i64, Vec<String>)>> {
    let sql = format!(
        "SELECT g.account_id, g.reason FROM global_follow_recommendations g
         JOIN account_summaries s ON s.account_id = g.account_id
         JOIN accounts a ON a.id = g.account_id
         WHERE NOT EXISTS (SELECT 1 FROM follow_recommendation_suppressions x
                           WHERE x.account_id = g.account_id)
           AND {BASE_SCOPE}
         ORDER BY (s.language IS NOT DISTINCT FROM $2) DESC, g.rank DESC
         LIMIT $3"
    );
    Ok(sqlx::query_as(&sql)
        .bind(viewer)
        .bind(locale)
        .bind(BATCH_SIZE)
        .fetch_all(&state.db)
        .await?)
}

/// `AccountSuggestions::FaspSource`: the accounts providers recommended to
/// the viewer, while the `fasp` feature is on.
async fn fasp(state: &AppState, viewer: i64) -> anyhow::Result<Vec<i64>> {
    if !crate::fasp::enabled(state) {
        return Ok(vec![]);
    }
    let sql = format!(
        "SELECT a.id FROM accounts a
         WHERE a.id IN (SELECT r.recommended_account_id FROM fasp_follow_recommendations r
                         WHERE r.requesting_account_id = $1)
           AND {BASE_SCOPE}
         LIMIT $2"
    );
    Ok(sqlx::query_scalar(&sql)
        .bind(viewer)
        .bind(BATCH_SIZE)
        .fetch_all(&state.db)
        .await?)
}

/// A shuffle that keeps its order for a viewer for [`ORDER_KEPT_FOR`]
/// seconds, so paging through suggestions sees one list, as Mastodon's cached
/// one is.
fn shuffle<T>(items: &mut [T], viewer: i64) {
    use rand::{seq::SliceRandom, SeedableRng};
    let window = chrono::Utc::now().timestamp() / ORDER_KEPT_FOR;
    let seed = (viewer as u64) ^ (window as u64).rotate_left(32);
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    items.shuffle(&mut rng);
}

/// `AccountSuggestions#get(limit, offset)`: each account with the sources
/// that suggested it.
pub async fn get(
    state: &AppState,
    viewer: i64,
    limit: usize,
    offset: usize,
) -> anyhow::Result<Vec<(i64, Vec<String>)>> {
    // `I18n.locale`, the user's own, as the content locale.
    let locale = sqlx::query_scalar!("SELECT locale FROM users WHERE account_id = $1", viewer)
        .fetch_optional(&state.db)
        .await?
        .flatten()
        .unwrap_or_else(|| "en".to_owned());
    let locale = locale.split(['_', '-']).next().unwrap_or("en").to_owned();

    let mut order: Vec<i64> = vec![];
    let mut sources: HashMap<i64, Vec<String>> = HashMap::new();
    let mut add = |id: i64, source: String| {
        let entry = sources.entry(id).or_insert_with(|| {
            order.push(id);
            vec![]
        });
        if !entry.contains(&source) {
            entry.push(source);
        }
    };
    for id in featured(state, viewer).await? {
        add(id, "featured".into());
    }
    for id in friends_of_friends(state, viewer).await? {
        add(id, "friends_of_friends".into());
    }
    for (id, reasons) in global(state, viewer, &locale).await? {
        for reason in reasons {
            add(id, reason);
        }
    }
    for id in fasp(state, viewer).await? {
        add(id, "fasp".into());
    }
    let mut all: Vec<(i64, Vec<String>)> = order
        .into_iter()
        .map(|id| {
            let found = sources.remove(&id).unwrap_or_default();
            (id, found)
        })
        .collect();
    shuffle(&mut all, viewer);
    Ok(all.into_iter().skip(offset).take(limit).collect())
}

/// `REST::SuggestionSerializer::LEGACY_SOURCE_TYPE_MAP` of the first source.
pub fn legacy_source(sources: &[String]) -> Option<&'static str> {
    match sources.first().map(String::as_str) {
        Some("featured") => Some("staff"),
        Some("most_followed" | "most_interactions") => Some("global"),
        Some("similar_to_recently_followed" | "friends_of_friends") => Some("past_interactions"),
        _ => None,
    }
}

/// `AccountSummary.refresh`: each discoverable, unlocked, unrestricted
/// account's most common language and sensitivity over its latest posts.
pub async fn refresh_account_summaries(state: &AppState) -> anyhow::Result<()> {
    // Delete any record that is ineligible.
    sqlx::query!(
        r#"DELETE FROM account_summaries s USING accounts a
           WHERE a.id = s.account_id
             AND NOT (a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
                      AND a.silenced_at IS NULL AND a.moved_to_account_id IS NULL
                      AND a.discoverable AND NOT a.locked)"#
    )
    .execute(&state.db)
    .await?;
    // Unless the table is empty, only the accounts that posted this week.
    let populated =
        sqlx::query_scalar!(r#"SELECT EXISTS (SELECT 1 FROM account_summaries) AS "e!""#)
            .fetch_one(&state.db)
            .await?;
    sqlx::query!(
        r#"INSERT INTO account_summaries (account_id, language, sensitive)
           SELECT accounts.id AS account_id,
                  mode() WITHIN GROUP (ORDER BY language ASC) AS language,
                  mode() WITHIN GROUP (ORDER BY sensitive ASC) AS sensitive
           FROM accounts
           CROSS JOIN LATERAL (
             SELECT s.language, s.sensitive FROM (
               SELECT statuses.language, statuses.sensitive, statuses.reblog_of_id
               FROM statuses
               WHERE statuses.account_id = accounts.id AND statuses.deleted_at IS NULL
               ORDER BY statuses.id DESC LIMIT 1000
             ) s
             WHERE s.reblog_of_id IS NULL
             LIMIT 20
           ) t0
           WHERE accounts.suspended_at IS NULL AND accounts.requested_deletion_at IS NULL
             AND accounts.silenced_at IS NULL AND accounts.moved_to_account_id IS NULL
             AND accounts.discoverable AND NOT accounts.locked
             AND (NOT $1 OR EXISTS (
                   SELECT 1 FROM account_stats st WHERE st.account_id = accounts.id
                     AND st.last_status_at >= now() - interval '1 week'))
           GROUP BY accounts.id
           ON CONFLICT (account_id) DO UPDATE
             SET language = EXCLUDED.language, sensitive = EXCLUDED.sensitive"#,
        populated,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `FollowRecommendation.refresh`: the accounts most followed by recently
/// active users, and those whose posts drew the most interactions this month,
/// leaving out sensitive and suppressed ones.
pub async fn refresh_follow_recommendations(state: &AppState) -> anyhow::Result<()> {
    let mut tx = state.db.begin().await?;
    sqlx::query!("UPDATE global_follow_recommendations SET stale = true")
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        r#"INSERT INTO global_follow_recommendations (account_id, rank, reason)
           SELECT account_id, sum(rank) AS rank, array_agg(reason) AS reason
           FROM (
             SELECT account_summaries.account_id AS account_id,
                    count(follows.id) / (1.0 + count(follows.id)) AS rank,
                    'most_followed' AS reason
             FROM follows
             INNER JOIN account_summaries ON account_summaries.account_id = follows.target_account_id
             INNER JOIN users ON users.account_id = follows.account_id
             WHERE users.current_sign_in_at >= (now() - interval '30 days')
               AND account_summaries.sensitive = 'f'
               AND NOT EXISTS (SELECT 1 FROM follow_recommendation_suppressions
                               WHERE follow_recommendation_suppressions.account_id = follows.target_account_id)
             GROUP BY account_summaries.account_id
             HAVING count(follows.id) >= 5
             UNION ALL
             SELECT account_summaries.account_id AS account_id,
                    sum(status_stats.reblogs_count + status_stats.favourites_count)
                      / (1.0 + sum(status_stats.reblogs_count + status_stats.favourites_count)) AS rank,
                    'most_interactions' AS reason
             FROM status_stats
             INNER JOIN statuses ON statuses.id = status_stats.status_id
             INNER JOIN account_summaries ON account_summaries.account_id = statuses.account_id
             WHERE statuses.id >= ((date_part('epoch', now() - interval '30 days') * 1000)::bigint << 16)
               AND account_summaries.sensitive = 'f'
               AND NOT EXISTS (SELECT 1 FROM follow_recommendation_suppressions
                               WHERE follow_recommendation_suppressions.account_id = statuses.account_id)
             GROUP BY account_summaries.account_id
             HAVING sum(status_stats.reblogs_count + status_stats.favourites_count) >= 5
           ) t0
           GROUP BY account_id
           ORDER BY rank DESC
           ON CONFLICT (account_id) DO UPDATE
             SET rank = EXCLUDED.rank, reason = EXCLUDED.reason, stale = 'f'"#
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM global_follow_recommendations WHERE stale")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// `Scheduler::FollowRecommendationsScheduler#perform`.
pub async fn refresh(state: &AppState) -> anyhow::Result<()> {
    refresh_account_summaries(state).await?;
    refresh_follow_recommendations(state).await
}

/// The scheduler, daily, for as long as the instance runs. The first pass
/// waits a few minutes, so a starting instance is not slowed by it.
pub async fn run(state: AppState) {
    crate::background::rest(&state.stop, Duration::from_secs(10 * 60)).await;
    while !state.stop.is_cancelled() {
        if let Err(error) = refresh(&state).await {
            tracing::error!(%error, "follow recommendations refresh failed");
        }
        crate::background::rest(&state.stop, Duration::from_secs(24 * 60 * 60)).await;
    }
}
