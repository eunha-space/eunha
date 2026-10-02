//! Mastodon's `Export` and `ExportSummary`: the files on the data export
//! page, byte for byte, and the counts beside them.

use serde::Serialize;

use super::csv::{write_row, Field};
use super::{export_acct, status_uri, StatusUriParts};
use crate::error::AppResult;
use crate::state::AppState;

/// One of the files `Settings::Exports::*` serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    Follows,
    Blocks,
    Mutes,
    Lists,
    DomainBlocks,
    Bookmarks,
    CustomFilters,
}

impl ExportKind {
    /// The file a route names: upstream's `/settings/exports/follows.csv`
    /// and so on.
    pub fn from_path(file: &str) -> Option<Self> {
        Some(match file {
            "follows.csv" => Self::Follows,
            "blocks.csv" => Self::Blocks,
            "mutes.csv" => Self::Mutes,
            "lists.csv" => Self::Lists,
            "domain_blocks.csv" => Self::DomainBlocks,
            "bookmarks.csv" => Self::Bookmarks,
            "custom_filters.json" => Self::CustomFilters,
            _ => return None,
        })
    }

    /// `send_data`'s `filename`: the controller's name.
    pub fn filename(self) -> &'static str {
        match self {
            Self::Follows => "following_accounts.csv",
            Self::Blocks => "blocked_accounts.csv",
            Self::Mutes => "muted_accounts.csv",
            Self::Lists => "lists.csv",
            Self::DomainBlocks => "blocked_domains.csv",
            Self::Bookmarks => "bookmarks.csv",
            Self::CustomFilters => "custom_filters.json",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::CustomFilters => "application/json",
            _ => "text/csv",
        }
    }

    /// The scope a token needs to read it.
    pub fn scope(self) -> &'static str {
        match self {
            Self::Follows => "read:follows",
            Self::Blocks | Self::DomainBlocks => "read:blocks",
            Self::Mutes => "read:mutes",
            Self::Lists => "read:lists",
            Self::Bookmarks => "read:bookmarks",
            Self::CustomFilters => "read:filters",
        }
    }
}

/// The file's contents.
pub async fn generate(state: &AppState, account_id: i64, kind: ExportKind) -> AppResult<String> {
    match kind {
        ExportKind::Follows => following_accounts_csv(state, account_id).await,
        ExportKind::Blocks => blocked_accounts_csv(state, account_id).await,
        ExportKind::Mutes => muted_accounts_csv(state, account_id).await,
        ExportKind::Lists => lists_csv(state, account_id).await,
        ExportKind::DomainBlocks => blocked_domains_csv(state, account_id).await,
        ExportKind::Bookmarks => bookmarks_csv(state, account_id).await,
        ExportKind::CustomFilters => custom_filters_json(state, account_id).await,
    }
}

fn s(value: impl Into<String>) -> Field {
    Some(value.into())
}

fn ruby_bool(value: bool) -> Field {
    s(if value { "true" } else { "false" })
}

/// `Export#to_following_accounts_csv`: newest follow first.
async fn following_accounts_csv(state: &AppState, account_id: i64) -> AppResult<String> {
    let rows = sqlx::query!(
        r#"SELECT a.username, a.domain, f.show_reblogs, f.notify, f.languages
           FROM follows f JOIN accounts a ON a.id = f.target_account_id
           WHERE f.account_id = $1
           ORDER BY f.id DESC"#,
        account_id,
    )
    .fetch_all(&state.db)
    .await?;
    let mut out = String::new();
    write_row(
        &mut out,
        &[
            s("Account address"),
            s("Show boosts"),
            s("Notify on new posts"),
            s("Languages"),
        ],
    );
    for row in rows {
        write_row(
            &mut out,
            &[
                s(export_acct(state, &row.username, row.domain.as_deref())),
                ruby_bool(row.show_reblogs),
                ruby_bool(row.notify),
                // `follow.languages&.join(', ')`.
                row.languages.map(|languages| languages.join(", ")),
            ],
        );
    }
    Ok(out)
}

/// `Export#to_blocked_accounts_csv`: `account.blocking`, newest block first.
async fn blocked_accounts_csv(state: &AppState, account_id: i64) -> AppResult<String> {
    let rows = sqlx::query!(
        r#"SELECT a.username, a.domain
           FROM blocks b JOIN accounts a ON a.id = b.target_account_id
           WHERE b.account_id = $1
           ORDER BY b.id DESC"#,
        account_id,
    )
    .fetch_all(&state.db)
    .await?;
    let mut out = String::new();
    for row in rows {
        write_row(
            &mut out,
            &[s(export_acct(state, &row.username, row.domain.as_deref()))],
        );
    }
    Ok(out)
}

/// `Export#to_muted_accounts_csv`: newest mute first.
async fn muted_accounts_csv(state: &AppState, account_id: i64) -> AppResult<String> {
    let rows = sqlx::query!(
        r#"SELECT a.username, a.domain, m.hide_notifications
           FROM mutes m JOIN accounts a ON a.id = m.target_account_id
           WHERE m.account_id = $1
           ORDER BY m.id DESC"#,
        account_id,
    )
    .fetch_all(&state.db)
    .await?;
    let mut out = String::new();
    write_row(&mut out, &[s("Account address"), s("Hide notifications")]);
    for row in rows {
        write_row(
            &mut out,
            &[
                s(export_acct(state, &row.username, row.domain.as_deref())),
                ruby_bool(row.hide_notifications),
            ],
        );
    }
    Ok(out)
}

/// `Export#to_lists_csv`: each owned list's members, a row each. Upstream
/// orders neither, so they come in the order the rows were made.
async fn lists_csv(state: &AppState, account_id: i64) -> AppResult<String> {
    let rows = sqlx::query!(
        r#"SELECT l.title, a.username, a.domain
           FROM lists l
           JOIN list_accounts la ON la.list_id = l.id
           JOIN accounts a ON a.id = la.account_id
           WHERE l.account_id = $1
           ORDER BY l.id, la.id"#,
        account_id,
    )
    .fetch_all(&state.db)
    .await?;
    let mut out = String::new();
    for row in rows {
        write_row(
            &mut out,
            &[
                s(row.title),
                s(export_acct(state, &row.username, row.domain.as_deref())),
            ],
        );
    }
    Ok(out)
}

/// `Export#to_blocked_domains_csv`.
async fn blocked_domains_csv(state: &AppState, account_id: i64) -> AppResult<String> {
    let domains: Vec<String> = sqlx::query_scalar!(
        "SELECT domain FROM account_domain_blocks WHERE account_id = $1 ORDER BY id",
        account_id,
    )
    .fetch_all(&state.db)
    .await?;
    let mut out = String::new();
    for domain in domains {
        write_row(&mut out, &[s(domain)]);
    }
    Ok(out)
}

/// `Export#to_bookmarks_csv`: newest bookmark first, skipping those whose
/// status is gone.
async fn bookmarks_csv(state: &AppState, account_id: i64) -> AppResult<String> {
    let rows = sqlx::query!(
        r#"SELECT s.id, s.uri, (s.reblog_of_id IS NOT NULL) AS "reblog!",
                  a.id AS account_id, a.id_scheme, a.username, a.domain
           FROM bookmarks b
           JOIN statuses s ON s.id = b.status_id AND s.deleted_at IS NULL
           JOIN accounts a ON a.id = s.account_id
           WHERE b.account_id = $1
           ORDER BY b.id DESC"#,
        account_id,
    )
    .fetch_all(&state.db)
    .await?;
    let mut out = String::new();
    for row in rows {
        let uri = status_uri(
            state,
            StatusUriParts {
                status_id: row.id,
                uri: row.uri.as_deref(),
                reblog: row.reblog,
                account_id: row.account_id,
                account_id_scheme: row.id_scheme,
                account_username: &row.username,
                account_domain: row.domain.as_deref(),
            },
        );
        write_row(&mut out, &[s(uri)]);
    }
    Ok(out)
}

#[derive(Serialize)]
struct FiltersFile {
    custom_filters: Vec<FilterEntry>,
}

#[derive(Serialize)]
struct FilterEntry {
    title: String,
    expires_at: Option<String>,
    context: Vec<String>,
    action: &'static str,
    keywords_attributes: Vec<KeywordEntry>,
    statuses: Vec<String>,
}

#[derive(Serialize)]
struct KeywordEntry {
    keyword: String,
    whole_word: bool,
}

/// How `JSON.generate` writes a time: `TimeWithZone#to_s`.
fn ruby_time(time: chrono::NaiveDateTime) -> String {
    time.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

/// `Export#to_custom_filters_json`: filters by title, each with its keywords
/// and the statuses it hides.
async fn custom_filters_json(state: &AppState, account_id: i64) -> AppResult<String> {
    let filters = sqlx::query!(
        r#"SELECT id, phrase, expires_at, context, action FROM custom_filters
           WHERE account_id = $1 ORDER BY phrase, id"#,
        account_id,
    )
    .fetch_all(&state.db)
    .await?;
    let ids: Vec<i64> = filters.iter().map(|f| f.id).collect();
    let keywords = sqlx::query!(
        r#"SELECT custom_filter_id, keyword, whole_word FROM custom_filter_keywords
           WHERE custom_filter_id = ANY($1) ORDER BY id"#,
        &ids,
    )
    .fetch_all(&state.db)
    .await?;
    let statuses = sqlx::query!(
        r#"SELECT cfs.custom_filter_id, s.id, s.uri, (s.reblog_of_id IS NOT NULL) AS "reblog!",
                  a.id AS account_id, a.id_scheme, a.username, a.domain
           FROM custom_filter_statuses cfs
           JOIN statuses s ON s.id = cfs.status_id AND s.deleted_at IS NULL
           JOIN accounts a ON a.id = s.account_id
           WHERE cfs.custom_filter_id = ANY($1) ORDER BY cfs.id"#,
        &ids,
    )
    .fetch_all(&state.db)
    .await?;

    let custom_filters = filters
        .into_iter()
        .map(|filter| FilterEntry {
            title: filter.phrase,
            expires_at: filter.expires_at.map(ruby_time),
            context: filter.context,
            action: crate::db::models::filter_action::to_str(filter.action),
            keywords_attributes: keywords
                .iter()
                .filter(|k| k.custom_filter_id == filter.id)
                .map(|k| KeywordEntry {
                    keyword: k.keyword.clone(),
                    whole_word: k.whole_word,
                })
                .collect(),
            statuses: statuses
                .iter()
                .filter(|s| s.custom_filter_id == filter.id)
                .map(|row| {
                    status_uri(
                        state,
                        StatusUriParts {
                            status_id: row.id,
                            uri: row.uri.as_deref(),
                            reblog: row.reblog,
                            account_id: row.account_id,
                            account_id_scheme: row.id_scheme,
                            account_username: &row.username,
                            account_domain: row.domain.as_deref(),
                        },
                    )
                })
                .collect(),
        })
        .collect();
    Ok(serde_json::to_string(&FiltersFile { custom_filters }).map_err(anyhow::Error::from)?)
}

/// `ExportSummary`: what the export page counts.
#[derive(Debug, Serialize)]
pub struct Summary {
    /// Bytes of uploaded media.
    pub storage: i64,
    pub statuses: i64,
    pub follows: i64,
    pub followers: i64,
    pub lists: i64,
    pub mutes: i64,
    pub blocks: i64,
    pub domain_blocks: i64,
    pub bookmarks: i64,
    pub custom_filters: i64,
}

pub async fn summary(state: &AppState, account_id: i64) -> AppResult<Summary> {
    let row = sqlx::query!(
        r#"SELECT
             (SELECT COALESCE(SUM(file_file_size), 0)::bigint FROM media_attachments WHERE account_id = $1) AS "storage!",
             (SELECT COALESCE(statuses_count, 0) FROM account_stats WHERE account_id = $1) AS "statuses",
             (SELECT COALESCE(following_count, 0) FROM account_stats WHERE account_id = $1) AS "follows",
             (SELECT COALESCE(followers_count, 0) FROM account_stats WHERE account_id = $1) AS "followers",
             (SELECT count(*) FROM lists WHERE account_id = $1) AS "lists!",
             (SELECT count(*) FROM mutes m JOIN accounts a ON a.id = m.target_account_id WHERE m.account_id = $1) AS "mutes!",
             (SELECT count(*) FROM blocks b JOIN accounts a ON a.id = b.target_account_id WHERE b.account_id = $1) AS "blocks!",
             (SELECT count(*) FROM account_domain_blocks WHERE account_id = $1) AS "domain_blocks!",
             (SELECT count(*) FROM bookmarks WHERE account_id = $1) AS "bookmarks!",
             (SELECT count(*) FROM custom_filters WHERE account_id = $1) AS "custom_filters!""#,
        account_id,
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Summary {
        storage: row.storage,
        statuses: row.statuses.unwrap_or(0),
        follows: row.follows.unwrap_or(0),
        followers: row.followers.unwrap_or(0),
        lists: row.lists,
        mutes: row.mutes,
        blocks: row.blocks,
        domain_blocks: row.domain_blocks,
        bookmarks: row.bookmarks,
        custom_filters: row.custom_filters,
    })
}
