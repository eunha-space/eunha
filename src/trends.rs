//! `Trends`: what is trending, scored as Mastodon scores it.
//!
//! Uses are registered as they happen: a tag's and a link's distinct users
//! per day go into their `Trends::History` in Redis, and every tag, link and
//! post used today is remembered in a `trending_{type}:used:{day}` set.
//! Every five minutes [`refresh`] scores whatever trended before or was used
//! today, writes those above the decay threshold to `tag_trends`,
//! `preview_card_trends` and `status_trends`, and ranks them. Every six
//! hours [`request_review`] mails staff about trends waiting on a review.
//!
//! The Redis writes are best-effort, like the histories': a Redis user
//! without the commands loses the use, not the request.

use std::collections::{BTreeSet, HashSet};
use std::time::Duration;

use chrono::{NaiveDateTime, Utc};

use crate::{moderation::history, state::AppState};

/// `Scheduler::Trends::RefreshScheduler`'s every five minutes.
pub const REFRESH_EVERY: Duration = Duration::from_secs(5 * 60);
/// The four minutes *config/sidekiq.yml* gives the refresh as `first_in`.
pub const REFRESH_FIRST_IN: Duration = Duration::from_secs(4 * 60);
/// `Scheduler::Trends::ReviewNotificationsScheduler`'s every six hours.
pub const REVIEW_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// `default_options[:threshold]`, the same for all three.
const THRESHOLD: f64 = 5.0;
/// `default_options[:review_threshold]`.
const REVIEW_THRESHOLD: i32 = 3;
/// `max_score_cooldown`, two days, for tags and links.
const MAX_SCORE_COOLDOWN: i64 = 2 * 24 * 60 * 60;
/// `max_score_halflife` and `score_halflife`, in seconds.
const TAGS_HALFLIFE: f64 = 4.0 * 3600.0;
const LINKS_HALFLIFE: f64 = 8.0 * 3600.0;
const STATUSES_HALFLIFE: f64 = 3600.0;
/// `decay_threshold`.
const TAGS_DECAY_THRESHOLD: f64 = 1.0;
const LINKS_DECAY_THRESHOLD: f64 = 1.0;
const STATUSES_DECAY_THRESHOLD: f64 = 0.3;

#[derive(Debug, Clone, Copy)]
enum Kind {
    Tags,
    Links,
    Statuses,
}

impl Kind {
    /// `PREFIX`.
    fn prefix(self) -> &'static str {
        match self {
            Self::Tags => "trending_tags",
            Self::Links => "trending_links",
            Self::Statuses => "trending_statuses",
        }
    }
}

fn used_key(state: &AppState, kind: Kind) -> String {
    state
        .redis_keys
        .key(format!("{}:used:{}", kind.prefix(), history::today()))
}

/// `Trends::Base#record_used_id`.
async fn record_used_id(state: &AppState, kind: Kind, id: i64) {
    let key = used_key(state, kind);
    let mut redis = state.redis.clone();
    let result: redis::RedisResult<()> = redis::pipe()
        .cmd("SADD")
        .arg(&key)
        .arg(id)
        .ignore()
        .cmd("EXPIRE")
        .arg(&key)
        .arg(24 * 60 * 60)
        .ignore()
        .query_async(&mut redis)
        .await;
    if let Err(error) = result {
        tracing::debug!(%error, kind = kind.prefix(), id, "could not record a trend use");
    }
}

/// `Trends::Base#recently_used_ids`.
async fn recently_used_ids(state: &AppState, kind: Kind) -> Vec<i64> {
    let mut redis = state.redis.clone();
    redis::cmd("SMEMBERS")
        .arg(used_key(state, kind))
        .query_async::<Vec<String>>(&mut redis)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|id| id.parse().ok())
        .collect()
}

/// `((observed - expected)**2) / expected`, or nothing below the threshold or
/// expectation.
fn raw_score(expected: f64, observed: f64) -> f64 {
    if expected > observed || observed < THRESHOLD {
        0.0
    } else {
        (observed - expected).powi(2) / expected
    }
}

/// `0.5**(elapsed / halflife)`.
fn decay(elapsed: f64, halflife: f64) -> f64 {
    0.5f64.powf(elapsed / halflife)
}

fn seconds(at: NaiveDateTime) -> f64 {
    at.and_utc().timestamp_millis() as f64 / 1000.0
}

// ── Registering uses ──────────────────────────────────────────────────────

/// `Trends.register!(status)`: a boost counts towards all three.
pub async fn register(state: &AppState, status_id: i64) {
    register_links(state, status_id).await;
    register_tags(state, status_id).await;
    register_status(state, status_id).await;
}

/// `Trends.tags.register(status)`: each usable tag of a public original post
/// by an account not limited.
pub async fn register_tags(state: &AppState, status_id: i64) {
    let result: anyhow::Result<()> = async {
        let Some(status) = sqlx::query!(
            r#"SELECT s.account_id,
                      (s.reblog_of_id IS NULL AND s.visibility = 0 AND a.silenced_at IS NULL) AS "counts!"
               FROM statuses s JOIN accounts a ON a.id = s.account_id
               WHERE s.id = $1"#,
            status_id,
        )
        .fetch_optional(&state.db)
        .await?
        else {
            return Ok(());
        };
        if !status.counts {
            return Ok(());
        }
        let tags = sqlx::query_scalar!(
            r#"SELECT t.id FROM statuses_tags st JOIN tags t ON t.id = st.tag_id
               WHERE st.status_id = $1 AND COALESCE(t.usable, true)"#,
            status_id,
        )
        .fetch_all(&state.db)
        .await?;
        for tag in tags {
            history::add(state, "tags", tag, &status.account_id.to_string()).await;
            record_used_id(state, Kind::Tags, tag).await;
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::debug!(%error, status_id, "could not register tag uses");
    }
}

/// `Trends.statuses.register(status)`: the post, or the one it boosts, if it
/// is eligible.
pub async fn register_status(state: &AppState, status_id: i64) {
    let result: anyhow::Result<()> = async {
        let Some(proper) = sqlx::query_scalar!(
            "SELECT COALESCE(reblog_of_id, id) AS \"id!\" FROM statuses WHERE id = $1",
            status_id,
        )
        .fetch_optional(&state.db)
        .await?
        else {
            return Ok(());
        };
        if eligible_statuses(state, &[proper]).await?.contains(&proper) {
            record_used_id(state, Kind::Statuses, proper).await;
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::debug!(%error, status_id, "could not register a post use");
    }
}

/// `Trends.links.register(status)`: the preview card of a public post, or of
/// the one it boosts, neither by a limited account nor behind a warning.
pub async fn register_links(state: &AppState, status_id: i64) {
    let result: anyhow::Result<()> = async {
        let Some(row) = sqlx::query!(
            r#"SELECT s.account_id,
                      (o.visibility = 0 AND s.visibility = 0
                       AND oa.silenced_at IS NULL AND a.silenced_at IS NULL
                       AND o.spoiler_text = '' AND NOT o.sensitive) AS "counts!",
                      (SELECT pc.id FROM preview_cards_statuses pcs
                       JOIN preview_cards pc ON pc.id = pcs.preview_card_id
                       WHERE pcs.status_id = o.id
                         -- `PreviewCard#appropriate_for_trends?`
                         AND pc.type = 0 AND pc.link_type = 1 AND pc.title <> ''
                         AND pc.description <> '' AND pc.image_file_name IS NOT NULL
                         AND COALESCE(pc.provider_name, '') <> ''
                       LIMIT 1) AS card_id
               FROM statuses s
               JOIN accounts a ON a.id = s.account_id
               JOIN statuses o ON o.id = COALESCE(s.reblog_of_id, s.id)
               JOIN accounts oa ON oa.id = o.account_id
               WHERE s.id = $1"#,
            status_id,
        )
        .fetch_optional(&state.db)
        .await?
        else {
            return Ok(());
        };
        if let (true, Some(card)) = (row.counts, row.card_id) {
            history::add(state, "links", card, &row.account_id.to_string()).await;
            record_used_id(state, Kind::Links, card).await;
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::debug!(%error, status_id, "could not register a link use");
    }
}

/// `Trends::Statuses#eligible?` for each of `ids`: public, by a discoverable
/// account neither limited nor marked sensitive, not behind a warning, not a
/// reply, in a known language, and quoting nothing or an acceptable quote of
/// a post that would be eligible itself. Deleted posts are left out, as
/// `Status`'s default scope leaves them out.
async fn eligible_statuses(state: &AppState, ids: &[i64]) -> sqlx::Result<HashSet<i64>> {
    let rows = sqlx::query!(
        r#"SELECT s.id, s.language
           FROM statuses s
           JOIN accounts a ON a.id = s.account_id
           LEFT JOIN quotes q ON q.status_id = s.id
           LEFT JOIN statuses qs ON qs.id = q.quoted_status_id AND qs.deleted_at IS NULL
           LEFT JOIN accounts qa ON qa.id = qs.account_id
           WHERE s.id = ANY($1) AND s.deleted_at IS NULL
             AND s.created_at <= now()
             AND s.visibility = 0 AND a.discoverable IS TRUE AND a.silenced_at IS NULL
             AND a.sensitized_at IS NULL AND s.spoiler_text = '' AND NOT s.sensitive
             AND s.in_reply_to_id IS NULL AND NOT COALESCE(s.reply, false)
             AND (q.id IS NULL OR (
                   (q.state = 1 OR NOT q.legacy) AND qs.id IS NOT NULL
                   AND qs.visibility = 0 AND qa.discoverable IS TRUE AND qa.silenced_at IS NULL
                   AND qa.sensitized_at IS NULL AND qs.spoiler_text = '' AND NOT qs.sensitive))"#,
        ids,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|r| crate::languages::valid_locale(r.language.as_deref()))
        .map(|r| r.id)
        .collect())
}

// ── Refreshing scores ─────────────────────────────────────────────────────

/// `Trends.refresh!`.
pub async fn refresh(state: &AppState) -> anyhow::Result<()> {
    let at = Utc::now().naive_utc();
    refresh_links(state, at).await?;
    refresh_tags(state, at).await?;
    refresh_statuses(state, at).await?;
    Ok(())
}

/// What trended before, and what was used today.
async fn candidates(state: &AppState, kind: Kind, trending: Vec<i64>) -> Vec<i64> {
    let mut ids: BTreeSet<i64> = trending.into_iter().collect();
    ids.extend(recently_used_ids(state, kind).await);
    ids.into_iter().collect()
}

/// `RankedTrend.recalculate_ordered_rank`.
async fn recalculate_ordered_rank(state: &AppState, table: &str) -> sqlx::Result<()> {
    // `table` is one of three constants, never input.
    sqlx::query(&format!(
        "UPDATE {table} SET rank = inner_ordered.calculated_rank
         FROM (SELECT id, row_number() OVER w AS calculated_rank FROM {table}
               WINDOW w AS (PARTITION BY language ORDER BY score DESC)) inner_ordered
         WHERE {table}.id = inner_ordered.id"
    ))
    .execute(&state.db)
    .await?;
    Ok(())
}

/// The peak-and-decay score tags and links share: today's distinct users
/// against yesterday's, the peak kept for two days and decayed from.
/// Returns the score and the new peak, if this is one.
async fn peak_score(
    state: &AppState,
    history_prefix: &str,
    id: i64,
    max_score: Option<f64>,
    max_score_at: Option<NaiveDateTime>,
    at: NaiveDateTime,
    halflife: f64,
) -> (f64, Option<f64>) {
    let mut expected = history::accounts(state, history_prefix, id, 1).await as f64;
    if expected == 0.0 {
        expected = 1.0;
    }
    let observed = history::accounts(state, history_prefix, id, 0).await as f64;
    let cooled =
        max_score_at.is_none_or(|t| t < at - chrono::Duration::seconds(MAX_SCORE_COOLDOWN));
    let mut max_score = if cooled {
        0.0
    } else {
        max_score.unwrap_or(0.0)
    };
    let mut max_time = max_score_at;
    let score = raw_score(expected, observed);
    let mut peak = None;
    if score > max_score {
        max_score = score;
        max_time = Some(at);
        peak = Some(score);
    }
    let decaying = match max_time {
        Some(t) if max_score != 0.0 => max_score * decay(seconds(at) - seconds(t), halflife),
        _ => 0.0,
    };
    (decaying, peak)
}

/// `Trends::Tags#refresh`.
async fn refresh_tags(state: &AppState, at: NaiveDateTime) -> anyhow::Result<()> {
    let trending = sqlx::query_scalar!("SELECT tag_id FROM tag_trends")
        .fetch_all(&state.db)
        .await?;
    let ids = candidates(state, Kind::Tags, trending).await;
    let trendable_by_default = crate::settings::boolean(state, "trendable_by_default").await;
    for batch in ids.chunks(100) {
        let tags = sqlx::query!(
            r#"SELECT id, max_score, max_score_at, COALESCE(trendable, $2) AS "trendable!"
               FROM tags WHERE id = ANY($1)"#,
            batch,
            trendable_by_default,
        )
        .fetch_all(&state.db)
        .await?;
        let (mut insert, mut delete) = (vec![], vec![]);
        for tag in tags {
            let (score, peak) = peak_score(
                state,
                "tags",
                tag.id,
                tag.max_score,
                tag.max_score_at,
                at,
                TAGS_HALFLIFE,
            )
            .await;
            if let Some(peak) = peak {
                sqlx::query!(
                    "UPDATE tags SET max_score = $2, max_score_at = $3 WHERE id = $1",
                    tag.id,
                    peak,
                    at
                )
                .execute(&state.db)
                .await?;
            }
            if score >= TAGS_DECAY_THRESHOLD {
                insert.push((tag.id, score, tag.trendable));
            } else {
                delete.push(tag.id);
            }
        }
        for (id, score, allowed) in insert {
            sqlx::query!(
                r#"INSERT INTO tag_trends (tag_id, score, language, allowed) VALUES ($1, $2, '', $3)
                   ON CONFLICT (tag_id, language) DO UPDATE SET score = $2, allowed = $3"#,
                id,
                score,
                allowed,
            )
            .execute(&state.db)
            .await?;
        }
        if !delete.is_empty() {
            sqlx::query!("DELETE FROM tag_trends WHERE tag_id = ANY($1)", &delete)
                .execute(&state.db)
                .await?;
        }
    }
    recalculate_ordered_rank(state, "tag_trends").await?;
    Ok(())
}

/// `Trends::Links#refresh`.
async fn refresh_links(state: &AppState, at: NaiveDateTime) -> anyhow::Result<()> {
    let trending = sqlx::query_scalar!("SELECT preview_card_id FROM preview_card_trends")
        .fetch_all(&state.db)
        .await?;
    let ids = candidates(state, Kind::Links, trending).await;
    for batch in ids.chunks(100) {
        let cards = sqlx::query!(
            r#"SELECT pc.id, pc.max_score, pc.max_score_at, pc.language,
                      -- `PreviewCard#trendable?`: its own say, else its provider's.
                      COALESCE(pc.trendable, (
                        SELECT p.trendable FROM preview_card_providers p
                        WHERE p.domain = lower(substring(pc.url from '^[a-zA-Z]+://([^/:?#]+)'))
                           OR lower(substring(pc.url from '^[a-zA-Z]+://([^/:?#]+)')) LIKE '%.' || p.domain
                        ORDER BY char_length(p.domain) DESC LIMIT 1), false) AS "trendable!"
               FROM preview_cards pc WHERE pc.id = ANY($1)"#,
            batch,
        )
        .fetch_all(&state.db)
        .await?;
        let (mut insert, mut delete) = (vec![], vec![]);
        for card in cards {
            let (score, peak) = peak_score(
                state,
                "links",
                card.id,
                card.max_score,
                card.max_score_at,
                at,
                LINKS_HALFLIFE,
            )
            .await;
            if let Some(peak) = peak {
                sqlx::query!(
                    "UPDATE preview_cards SET max_score = $2, max_score_at = $3 WHERE id = $1",
                    card.id,
                    peak,
                    at
                )
                .execute(&state.db)
                .await?;
            }
            // A link in no known language never trends.
            let score = if crate::languages::valid_locale(card.language.as_deref()) {
                score
            } else {
                0.0
            };
            if score >= LINKS_DECAY_THRESHOLD {
                insert.push((card.id, score, card.language, card.trendable));
            } else {
                delete.push(card.id);
            }
        }
        for (id, score, language, allowed) in insert {
            sqlx::query!(
                r#"INSERT INTO preview_card_trends (preview_card_id, score, language, allowed)
                   VALUES ($1, $2, $3, $4)
                   ON CONFLICT (preview_card_id) DO UPDATE SET score = $2, language = $3, allowed = $4"#,
                id,
                score,
                language,
                allowed,
            )
            .execute(&state.db)
            .await?;
        }
        if !delete.is_empty() {
            sqlx::query!(
                "DELETE FROM preview_card_trends WHERE preview_card_id = ANY($1)",
                &delete
            )
            .execute(&state.db)
            .await?;
        }
    }
    recalculate_ordered_rank(state, "preview_card_trends").await?;
    Ok(())
}

/// `Trends::Statuses#refresh`: boosts and favourites past the threshold,
/// halved for every hour since the post was made.
async fn refresh_statuses(state: &AppState, at: NaiveDateTime) -> anyhow::Result<()> {
    let trending = sqlx::query_scalar!("SELECT status_id FROM status_trends")
        .fetch_all(&state.db)
        .await?;
    let ids = candidates(state, Kind::Statuses, trending).await;
    let trendable_by_default = crate::settings::boolean(state, "trendable_by_default").await;
    for batch in ids.chunks(100) {
        let statuses = sqlx::query!(
            r#"SELECT s.id, s.account_id, s.language, s.created_at,
                      (GREATEST(COALESCE(ss.reblogs_count, 0), 0)
                       + GREATEST(COALESCE(ss.favourites_count, 0), 0)) AS "interactions!",
                      COALESCE(s.trendable, a.trendable, $2) AS "trendable!"
               FROM statuses s
               JOIN accounts a ON a.id = s.account_id
               LEFT JOIN status_stats ss ON ss.status_id = s.id
               WHERE s.id = ANY($1) AND s.deleted_at IS NULL"#,
            batch,
            trendable_by_default,
        )
        .fetch_all(&state.db)
        .await?;
        let eligible = eligible_statuses(state, batch).await?;
        let (mut insert, mut delete) = (vec![], vec![]);
        for status in statuses {
            let score = raw_score(1.0, status.interactions as f64);
            let score = if score == 0.0 || !eligible.contains(&status.id) {
                0.0
            } else {
                score * decay(seconds(at) - seconds(status.created_at), STATUSES_HALFLIFE)
            };
            if score >= STATUSES_DECAY_THRESHOLD {
                insert.push(StatusRow {
                    status_id: status.id,
                    account_id: status.account_id,
                    score,
                    language: status.language,
                    allowed: status.trendable,
                });
            } else {
                delete.push(status.id);
            }
        }
        for row in insert {
            sqlx::query!(
                r#"INSERT INTO status_trends (status_id, account_id, score, language, allowed)
                   VALUES ($1, $2, $3, $4, $5)
                   ON CONFLICT (status_id) DO UPDATE
                     SET account_id = $2, score = $3, language = $4, allowed = $5"#,
                row.status_id,
                row.account_id,
                row.score,
                row.language,
                row.allowed,
            )
            .execute(&state.db)
            .await?;
        }
        if !delete.is_empty() {
            sqlx::query!(
                "DELETE FROM status_trends WHERE status_id = ANY($1)",
                &delete
            )
            .execute(&state.db)
            .await?;
        }
    }
    recalculate_ordered_rank(state, "status_trends").await?;
    Ok(())
}

struct StatusRow {
    status_id: i64,
    account_id: i64,
    score: f64,
    language: Option<String>,
    allowed: bool,
}

// ── Requesting reviews ────────────────────────────────────────────────────

/// A trend staff are asked to review.
pub struct ReviewItem {
    pub label: String,
    pub detail: String,
}

/// What [`request_review`] found, by kind.
#[derive(Default)]
pub struct Requested {
    pub links: Vec<ReviewItem>,
    pub tags: Vec<ReviewItem>,
    pub statuses: Vec<ReviewItem>,
}

impl Requested {
    fn is_empty(&self) -> bool {
        self.links.is_empty() && self.tags.is_empty() && self.statuses.is_empty()
    }
}

/// `Trends.request_review!`: unless every trend is allowed by default, or
/// trends are off, mark what outscores the third allowed trend but awaits a
/// review as asked about, and mail each `manage_taxonomies` user who wants
/// it the list.
pub async fn request_review(state: &AppState) -> anyhow::Result<Requested> {
    if crate::settings::boolean(state, "trendable_by_default").await
        || !crate::settings::boolean(state, "trends").await
    {
        return Ok(Requested::default());
    }
    let requested = Requested {
        links: request_link_reviews(state).await?,
        tags: request_tag_reviews(state).await?,
        statuses: request_status_reviews(state).await?,
    };
    if requested.is_empty() {
        return Ok(requested);
    }
    let staff =
        crate::push::accounts_who_can(state, &[crate::moderation::role::flag::MANAGE_TAXONOMIES])
            .await?;
    for staff_id in staff {
        let Some(recipient) = sqlx::query!(
            "SELECT email, settings FROM users WHERE account_id = $1",
            staff_id
        )
        .fetch_optional(&state.db)
        .await?
        else {
            continue;
        };
        // `allows_trends_review_emails?`
        if !crate::accounts::user_setting_bool(
            recipient.settings.as_deref(),
            "notification_emails.trends",
            true,
        ) {
            continue;
        }
        if let Err(error) = state
            .mailer()
            .send_new_trends(&recipient.email, &state.instance.domain, &requested)
            .await
        {
            tracing::warn!(%error, "could not mail staff about trends to review");
        }
    }
    Ok(requested)
}

/// The score of the lowest-ranked allowed trend within the review threshold
/// (`allowed.by_rank.ranked_below(review_threshold).first&.score || 0`).
async fn score_at_threshold(
    state: &AppState,
    table: &str,
    language: Option<&str>,
) -> sqlx::Result<f64> {
    // `table` is one of three constants, never input.
    let score: Option<f64> = sqlx::query_scalar(&format!(
        "SELECT score FROM {table} WHERE allowed AND rank <= $1
           AND language IS NOT DISTINCT FROM $2
         ORDER BY rank DESC LIMIT 1"
    ))
    .bind(REVIEW_THRESHOLD)
    .bind(language)
    .fetch_optional(&state.db)
    .await?;
    Ok(score.unwrap_or(0.0))
}

/// `Trends::Tags#request_review`.
async fn request_tag_reviews(state: &AppState) -> anyhow::Result<Vec<ReviewItem>> {
    let threshold = score_at_threshold(state, "tag_trends", Some("")).await?;
    let tags = sqlx::query!(
        r#"UPDATE tags t SET requested_review_at = now(), updated_at = now()
           FROM tag_trends tt
           WHERE tt.tag_id = t.id AND NOT tt.allowed AND tt.score > $1
             AND t.trendable IS NOT TRUE
             -- `requires_review_notification?`
             AND t.reviewed_at IS NULL AND t.requested_review_at IS NULL
           RETURNING t.id, COALESCE(t.display_name, t.name) AS "name!", tt.score"#,
        threshold,
    )
    .fetch_all(&state.db)
    .await?;
    let mut items = vec![];
    for tag in tags {
        let today = history::accounts(state, "tags", tag.id, 0).await;
        let yesterday = history::accounts(state, "tags", tag.id, 1).await;
        items.push(ReviewItem {
            label: format!("#{}", tag.name),
            detail: format!(
                "{today} people today ({yesterday} yesterday) · Score: {:.2}",
                tag.score
            ),
        });
    }
    Ok(items)
}

/// The languages a trend table holds (`RankedTrend.locales`).
async fn locales(state: &AppState, table: &str) -> sqlx::Result<Vec<Option<String>>> {
    // `table` is one of three constants, never input.
    sqlx::query_scalar(&format!("SELECT DISTINCT language FROM {table}"))
        .fetch_all(&state.db)
        .await
}

/// `Trends::Statuses#request_review`, which marks the authors as asked about.
async fn request_status_reviews(state: &AppState) -> anyhow::Result<Vec<ReviewItem>> {
    let mut items = vec![];
    for language in locales(state, "status_trends").await? {
        let threshold = score_at_threshold(state, "status_trends", language.as_deref()).await?;
        let statuses = sqlx::query!(
            r#"SELECT s.id, s.uri, s.url, s.local, a.username, st.score, s.account_id
               FROM status_trends st
               JOIN statuses s ON s.id = st.status_id AND s.deleted_at IS NULL
               JOIN accounts a ON a.id = s.account_id
               WHERE st.language IS NOT DISTINCT FROM $1 AND NOT st.allowed AND st.score > $2
                 AND NOT COALESCE(s.trendable, a.trendable, false)
                 AND s.trendable IS NULL AND a.reviewed_at IS NULL AND a.requested_review_at IS NULL"#,
            language.as_deref(),
            threshold,
        )
        .fetch_all(&state.db)
        .await?;
        let accounts: Vec<i64> = statuses.iter().map(|s| s.account_id).collect();
        sqlx::query!(
            "UPDATE accounts SET requested_review_at = now(), updated_at = now() WHERE id = ANY($1)",
            &accounts
        )
        .execute(&state.db)
        .await?;
        // `status.account.touch(:requested_review_at)`, whose
        // `after_update_commit` a local account's webhook is.
        let mut touched = accounts.clone();
        touched.sort_unstable();
        touched.dedup();
        for account_id in touched {
            crate::moderation::webhooks::account_updated(state, account_id).await;
        }
        for status in statuses {
            // `ActivityPub::TagManager#url_for`.
            let url = if status.local.unwrap_or(false) {
                format!(
                    "https://{}/@{}/{}",
                    state.urls.local_domain, status.username, status.id
                )
            } else {
                status.url.or(status.uri).unwrap_or_default()
            };
            items.push(ReviewItem {
                label: url,
                detail: format!(
                    "{} · Score: {:.2}",
                    language_name(language.as_deref()),
                    status.score
                ),
            });
        }
    }
    Ok(items)
}

/// `Trends::Links#request_review`, which marks the providers as asked about,
/// creating one for a domain that has none.
async fn request_link_reviews(state: &AppState) -> anyhow::Result<Vec<ReviewItem>> {
    let mut items = vec![];
    for language in locales(state, "preview_card_trends").await? {
        let threshold =
            score_at_threshold(state, "preview_card_trends", language.as_deref()).await?;
        let cards = sqlx::query!(
            r#"WITH cards AS (
                 SELECT pc.*, lower(substring(pc.url from '^[a-zA-Z]+://([^/:?#]+)')) AS domain,
                        (SELECT p.id FROM preview_card_providers p
                         WHERE lower(substring(pc.url from '^[a-zA-Z]+://([^/:?#]+)')) = p.domain
                            OR lower(substring(pc.url from '^[a-zA-Z]+://([^/:?#]+)')) LIKE '%.' || p.domain
                         ORDER BY char_length(p.domain) DESC LIMIT 1) AS provider_id
                 FROM preview_cards pc
               )
               SELECT c.id, c.url, c.title, c.domain, c.provider_id, t.score
               FROM preview_card_trends t
               JOIN cards c ON c.id = t.preview_card_id
               LEFT JOIN preview_card_providers p ON p.id = c.provider_id
               WHERE t.language IS NOT DISTINCT FROM $1 AND NOT t.allowed AND t.score > $2
                 AND NOT COALESCE(c.trendable, p.trendable, false)
                 AND c.trendable IS NULL
                 AND (p.id IS NULL OR (p.reviewed_at IS NULL AND p.requested_review_at IS NULL))"#,
            language.as_deref(),
            threshold,
        )
        .fetch_all(&state.db)
        .await?;
        for card in cards {
            match card.provider_id {
                Some(provider) => {
                    sqlx::query!(
                        "UPDATE preview_card_providers SET requested_review_at = now(), updated_at = now() WHERE id = $1",
                        provider
                    )
                    .execute(&state.db)
                    .await?;
                }
                None => {
                    if let Some(domain) = card.domain.as_deref() {
                        sqlx::query!(
                            r#"INSERT INTO preview_card_providers (domain, requested_review_at, created_at, updated_at)
                               VALUES ($1, now(), now(), now()) ON CONFLICT DO NOTHING"#,
                            domain
                        )
                        .execute(&state.db)
                        .await?;
                    }
                }
            }
            let today = history::accounts(state, "links", card.id, 0).await;
            let yesterday = history::accounts(state, "links", card.id, 1).await;
            items.push(ReviewItem {
                label: format!("{} · {}", card.title, card.url),
                detail: format!(
                    "{} · {today} people today ({yesterday} yesterday) · Score: {:.2}",
                    language_name(language.as_deref()),
                    card.score
                ),
            });
        }
    }
    Ok(items)
}

/// `standard_locale_name`.
fn language_name(code: Option<&str>) -> String {
    code.and_then(|code| {
        crate::languages::SUPPORTED_LOCALES
            .iter()
            .find(|(c, _, _)| *c == code)
            .map(|(_, name, _)| (*name).to_owned())
    })
    .unwrap_or_default()
}

// ── Reading trends ────────────────────────────────────────────────────────

/// `Trends::Query#preferred_languages`: the viewer's chosen languages, else
/// the request's locale (`content_locale`).
pub async fn preferred_languages(
    state: &AppState,
    viewer: Option<i64>,
    headers: &axum::http::HeaderMap,
) -> Vec<String> {
    let mut locale = None;
    if let Some(viewer) = viewer {
        let row = sqlx::query!(
            "SELECT chosen_languages, locale FROM users WHERE account_id = $1",
            viewer
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
        if let Some(row) = row {
            if let Some(chosen) = row.chosen_languages.filter(|c| !c.is_empty()) {
                return chosen;
            }
            locale = row.locale.filter(|l| !l.is_empty());
        }
    }
    let locale = locale
        .or_else(|| accept_language(headers))
        .unwrap_or_else(|| state.instance.default_locale().to_owned());
    // `I18n.locale.to_s.split(/[_-]/).first`
    vec![locale
        .split(['-', '_'])
        .next()
        .unwrap_or_default()
        .to_owned()]
}

/// The first `Accept-Language` tag Mastodon knows the language of.
fn accept_language(headers: &axum::http::HeaderMap) -> Option<String> {
    let header = headers
        .get(axum::http::header::ACCEPT_LANGUAGE)?
        .to_str()
        .ok()?;
    let mut tags: Vec<(f32, &str)> = header
        .split(',')
        .filter_map(|part| {
            let mut pieces = part.trim().split(';');
            let tag = pieces.next()?.trim();
            let quality = pieces
                .find_map(|p| p.trim().strip_prefix("q="))
                .and_then(|q| q.parse().ok())
                .unwrap_or(1.0);
            (!tag.is_empty() && tag != "*").then_some((quality, tag))
        })
        .collect();
    tags.sort_by(|a, b| b.0.total_cmp(&a.0));
    tags.into_iter()
        .map(|(_, tag)| tag.split(['-', '_']).next().unwrap_or(tag).to_lowercase())
        .find(|code| crate::languages::valid_locale(Some(code)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_like_mastodon() {
        // Below the threshold, or fewer than expected: nothing.
        assert_eq!(raw_score(1.0, 4.0), 0.0);
        assert_eq!(raw_score(10.0, 8.0), 0.0);
        // `((observed - expected)**2) / expected`
        assert_eq!(raw_score(1.0, 5.0), 16.0);
        assert_eq!(raw_score(2.0, 6.0), 8.0);
        assert!((decay(3600.0, 3600.0) - 0.5).abs() < 1e-12);
        assert!((decay(4.0 * 3600.0, TAGS_HALFLIFE) - 0.5).abs() < 1e-12);
    }
}
