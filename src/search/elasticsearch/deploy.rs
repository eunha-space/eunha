//! `eunha search deploy`, which is `tootctl search deploy`: create the
//! indexes or upgrade those whose specification changed, then import every
//! row from the database and delete what the database no longer has.
//!
//! Chewy notices a changed specification by a digest it keeps in an index
//! of its own; eunha compares what the cluster has — the mapping and the
//! analysis settings — with what it would create. An index created by
//! Mastodon with the same specification is kept as it is, and Mastodon finds
//! eunha's indexes as it would have made them.
//!
//! Each index is imported in batches of `--batch-size`, `--concurrency` of
//! them at a time. Beyond what tootctl does, the last batch written is kept
//! in Redis, so that `--resume` carries on an interrupted import from there
//! instead of starting over.

use futures::future::try_join_all;
use serde_json::{json, Value};

use super::documents::{bulk_lines, documents};
use super::{Client, Index};
use crate::state::AppState;

#[derive(Debug, Clone)]
pub struct Options {
    /// `--concurrency`: batches in flight at once.
    pub concurrency: usize,
    /// `--batch-size`: rows in each batch.
    pub batch_size: i64,
    /// `--only`: these indexes, or every one when empty.
    pub only: Vec<Index>,
    /// `--no-import` turns this off.
    pub import: bool,
    /// `--no-clean` turns this off.
    pub clean: bool,
    /// `--only-mapping`: update a changed specification in place, without
    /// re-creating the index or importing anything.
    pub only_mapping: bool,
    /// `--resume`: carry on from the last batch an earlier run wrote.
    pub resume: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            concurrency: 5,
            batch_size: 100,
            only: vec![],
            import: true,
            clean: true,
            only_mapping: false,
            resume: false,
        }
    }
}

/// What a deploy did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub indexed: u64,
    pub deleted: u64,
}

/// The first value of an answer keyed by index name: the concrete index's,
/// when the name asked for is an alias.
fn first_value(answer: &Value) -> Value {
    answer
        .as_object()
        .and_then(|m| m.values().next())
        .cloned()
        .unwrap_or(Value::Null)
}

/// `index.specification.changed?`: whether what the cluster has differs
/// from what eunha would create, or there is nothing there yet.
pub async fn specification_changed(client: &Client, index: Index) -> anyhow::Result<bool> {
    let name = client.index_name(index.base_name());
    if !client.exists(&name).await? {
        return Ok(true);
    }
    let mapping = first_value(
        &client
            .send(reqwest::Method::GET, &format!("{name}/_mapping"), None)
            .await?,
    );
    if super::indexes::stringify(&mapping["mappings"])
        != super::indexes::stringify(&index.mappings())
    {
        return Ok(true);
    }
    let settings = first_value(
        &client
            .send(reqwest::Method::GET, &format!("{name}/_settings"), None)
            .await?,
    );
    let live = &settings["settings"]["index"];
    let analysis = index.analysis().map(|a| super::indexes::stringify(&a));
    if live.get("analysis").cloned() != analysis {
        return Ok(true);
    }
    if let Some(expected) = index.index_settings(client.preset()) {
        if let Some(shards) = expected.get("number_of_shards") {
            if live["number_of_shards"] != super::indexes::stringify(shards) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// `index.purge`: delete the index if it is there and create it afresh.
pub async fn purge(client: &Client, index: Index) -> anyhow::Result<()> {
    let name = client.index_name(index.base_name());
    if client.exists(&name).await? {
        client.send(reqwest::Method::DELETE, &name, None).await?;
    }
    let body = json!({
        "settings": index.settings(client.preset()),
        "mappings": index.mappings(),
    });
    client
        .send(reqwest::Method::PUT, &name, Some(&body))
        .await?;
    Ok(())
}

/// `update_specification`: close the index, put the analysis settings and
/// the mapping, and open it again.
async fn update_specification(client: &Client, index: Index) -> anyhow::Result<()> {
    let name = client.index_name(index.base_name());
    client
        .send(reqwest::Method::POST, &format!("{name}/_close"), None)
        .await?;
    let result = async {
        if let Some(analysis) = index.analysis() {
            client
                .send(
                    reqwest::Method::PUT,
                    &format!("{name}/_settings"),
                    Some(&json!({ "settings": { "analysis": analysis } })),
                )
                .await?;
        }
        client
            .send(
                reqwest::Method::PUT,
                &format!("{name}/_mapping"),
                Some(&index.mappings()),
            )
            .await
    }
    .await;
    client
        .send(reqwest::Method::POST, &format!("{name}/_open"), None)
        .await?;
    result.map(|_| ())
}

async fn set_refresh_interval(
    client: &Client,
    index: Index,
    interval: Value,
) -> anyhow::Result<()> {
    client
        .send(
            reqwest::Method::PUT,
            &format!("{}/_settings", client.index_name(index.base_name())),
            Some(&json!({ "index": { "refresh_interval": interval } })),
        )
        .await
        .map(|_| ())
}

/// Every document id in `index`, by scrolling through it.
pub async fn indexed_ids(client: &Client, index: &str) -> anyhow::Result<Vec<String>> {
    let mut ids = vec![];
    let mut scroll = Scroll::new(client, index, 1000);
    let result = async {
        while let Some(page) = scroll.next().await? {
            ids.extend(page);
        }
        anyhow::Ok(())
    }
    .await;
    scroll.close().await;
    result.map(|()| ids)
}

/// A scroll through an index's ids, a page at a time.
struct Scroll<'a> {
    client: &'a Client,
    index: String,
    size: i64,
    scroll_id: Option<String>,
    started: bool,
}

impl<'a> Scroll<'a> {
    fn new(client: &'a Client, index: &str, size: i64) -> Self {
        Self {
            client,
            index: index.to_string(),
            size,
            scroll_id: None,
            started: false,
        }
    }

    async fn next(&mut self) -> anyhow::Result<Option<Vec<String>>> {
        let answer = if !self.started {
            self.started = true;
            self.client
                .send(
                    reqwest::Method::POST,
                    &format!("{}/_search?scroll=5m", self.index),
                    Some(&json!({ "size": self.size, "_source": false, "sort": ["_doc"] })),
                )
                .await?
        } else {
            let Some(id) = self.scroll_id.clone() else {
                return Ok(None);
            };
            self.client
                .send(
                    reqwest::Method::POST,
                    "_search/scroll",
                    Some(&json!({ "scroll": "5m", "scroll_id": id })),
                )
                .await?
        };
        if let Some(id) = answer["_scroll_id"].as_str() {
            self.scroll_id = Some(id.to_string());
        }
        let page: Vec<String> = answer["hits"]["hits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|hit| hit["_id"].as_str().map(str::to_owned))
            .collect();
        Ok((!page.is_empty()).then_some(page))
    }

    async fn close(self) {
        if let Some(id) = self.scroll_id {
            let _ = self
                .client
                .send(
                    reqwest::Method::DELETE,
                    "_search/scroll",
                    Some(&json!({ "scroll_id": [id] })),
                )
                .await;
        }
    }
}

/// The rows each index is imported from, as `(cursor, status or row id)`
/// pages: `StatusesIndexImporter` comes at the index from the interactions
/// that make a post searchable rather than from every post.
fn scopes(index: Index) -> &'static [&'static str] {
    match index {
        Index::Accounts => &["SELECT a.id, a.id FROM accounts a LEFT JOIN users u ON u.account_id = a.id \
             WHERE a.suspended_at IS NULL AND a.moved_to_account_id IS NULL \
               AND (a.domain IS NOT NULL OR (u.approved AND u.confirmed_at IS NOT NULL)) \
               AND a.id > $1 ORDER BY a.id LIMIT $2"],
        Index::Tags => &["SELECT id, id FROM tags WHERE (listable = TRUE OR listable IS NULL) \
             AND id > $1 ORDER BY id LIMIT $2"],
        Index::PublicStatuses => &["SELECT s.id, s.id FROM statuses s JOIN accounts a ON a.id = s.account_id \
             WHERE s.deleted_at IS NULL AND s.reblog_of_id IS NULL AND s.visibility = 0 AND a.indexable \
               AND s.id > $1 ORDER BY s.id LIMIT $2"],
        Index::Statuses => &[
            // `local_statuses_scope`.
            "SELECT id, coalesce(reblog_of_id, id) FROM statuses \
             WHERE (local OR uri IS NULL) AND deleted_at IS NULL AND id > $1 ORDER BY id LIMIT $2",
            // `local_mentions_scope`.
            "SELECT m.id, m.status_id FROM mentions m JOIN accounts a ON a.id = m.account_id \
             WHERE a.domain IS NULL AND NOT m.silent AND m.id > $1 ORDER BY m.id LIMIT $2",
            // `local_favourites_scope`.
            "SELECT f.id, f.status_id FROM favourites f JOIN accounts a ON a.id = f.account_id \
             WHERE a.domain IS NULL AND f.id > $1 ORDER BY f.id LIMIT $2",
            // `local_votes_scope`.
            "SELECT DISTINCT p.id, p.status_id FROM polls p JOIN poll_votes v ON v.poll_id = p.id \
             JOIN accounts a ON a.id = v.account_id \
             WHERE a.domain IS NULL AND p.id > $1 ORDER BY p.id LIMIT $2",
            // `local_bookmarks_scope`.
            "SELECT id, status_id FROM bookmarks WHERE id > $1 ORDER BY id LIMIT $2",
        ],
        Index::Instances => &[],
    }
}

fn progress_key(state: &AppState, index: Index) -> String {
    state
        .redis_keys
        .key(format!("search:deploy:{}", index.base_name()))
}

/// Where `--resume` starts: the scope and the cursor after the last batch
/// an earlier run wrote.
async fn saved_progress(state: &AppState, index: Index) -> (usize, i64) {
    let mut redis = state.redis.clone();
    let saved: Option<String> = redis::cmd("GET")
        .arg(progress_key(state, index))
        .query_async(&mut redis)
        .await
        .unwrap_or(None);
    saved
        .and_then(|s| {
            let (scope, cursor) = s.split_once(':')?;
            Some((scope.parse().ok()?, cursor.parse().ok()?))
        })
        .unwrap_or((0, 0))
}

async fn save_progress(state: &AppState, index: Index, scope: usize, cursor: i64) {
    let mut redis = state.redis.clone();
    let _: redis::RedisResult<()> = redis::cmd("SET")
        .arg(progress_key(state, index))
        .arg(format!("{scope}:{cursor}"))
        .query_async(&mut redis)
        .await;
}

async fn clear_progress(state: &AppState, index: Index) {
    let mut redis = state.redis.clone();
    let _: redis::RedisResult<()> = redis::cmd("DEL")
        .arg(progress_key(state, index))
        .query_async(&mut redis)
        .await;
}

/// One batch of ids, documented and written; how many were indexed and
/// deleted.
async fn write_batch(
    state: &AppState,
    client: &Client,
    index: Index,
    ids: Vec<i64>,
) -> anyhow::Result<(u64, u64)> {
    let docs = documents(state, index, &ids).await?;
    let indexed = docs.iter().filter(|(_, d)| d.is_some()).count() as u64;
    let deleted = docs.len() as u64 - indexed;
    client
        .bulk(&client.index_name(index.base_name()), &bulk_lines(docs))
        .await?;
    Ok((indexed, deleted))
}

/// `importer.import!`.
async fn import(
    state: &AppState,
    client: &Client,
    index: Index,
    options: &Options,
    report: &mut Report,
) -> anyhow::Result<()> {
    if index == Index::Instances {
        let docs = super::documents::instances(state).await?;
        for chunk in docs.chunks(options.batch_size.max(1) as usize) {
            client
                .bulk(
                    &client.index_name(index.base_name()),
                    &bulk_lines(chunk.to_vec()),
                )
                .await?;
            report.indexed += chunk.len() as u64;
        }
        return Ok(());
    }
    let (start_scope, start_cursor) = if options.resume {
        saved_progress(state, index).await
    } else {
        clear_progress(state, index).await;
        (0, 0)
    };
    for (scope_no, sql) in scopes(index).iter().enumerate() {
        if scope_no < start_scope {
            continue;
        }
        let mut cursor = if scope_no == start_scope {
            start_cursor
        } else {
            0
        };
        loop {
            // A wave of `concurrency` batches, read in order and written at
            // once; the cursor is saved once the whole wave is written.
            let mut wave = vec![];
            for _ in 0..options.concurrency {
                let rows: Vec<(i64, i64)> = sqlx::query_as(sql)
                    .bind(cursor)
                    .bind(options.batch_size)
                    .fetch_all(&state.db)
                    .await?;
                let Some(&(last, _)) = rows.last() else { break };
                cursor = last;
                let mut ids: Vec<i64> = rows.into_iter().map(|(_, id)| id).collect();
                ids.sort_unstable();
                ids.dedup();
                wave.push(write_batch(state, client, index, ids));
            }
            if wave.is_empty() {
                break;
            }
            for (indexed, deleted) in try_join_all(wave).await? {
                report.indexed += indexed;
                report.deleted += deleted;
            }
            save_progress(state, index, scope_no, cursor).await;
        }
    }
    clear_progress(state, index).await;
    Ok(())
}

/// `importer.clean_up!`: delete the documents whose row is gone.
async fn clean_up(
    state: &AppState,
    client: &Client,
    index: Index,
    options: &Options,
    report: &mut Report,
) -> anyhow::Result<()> {
    let name = client.index_name(index.base_name());
    let known_instances: std::collections::HashSet<String> = if index == Index::Instances {
        super::documents::instances(state)
            .await?
            .into_iter()
            .map(|(domain, _)| domain)
            .collect()
    } else {
        Default::default()
    };
    let mut scroll = Scroll::new(client, &name, options.batch_size);
    let result = async {
        while let Some(page) = scroll.next().await? {
            let existing: std::collections::HashSet<String> = if index == Index::Instances {
                page.iter()
                    .filter(|d| known_instances.contains(*d))
                    .cloned()
                    .collect()
            } else {
                let ids: Vec<i64> = page.iter().filter_map(|id| id.parse().ok()).collect();
                // `Status` has `kept` as its default scope.
                let extra = if index.table() == "statuses" {
                    " AND deleted_at IS NULL"
                } else {
                    ""
                };
                let sql = format!(
                    "SELECT id::text FROM {} WHERE id = ANY($1){extra}",
                    index.table()
                );
                sqlx::query_scalar::<_, String>(&sql)
                    .bind(&ids)
                    .fetch_all(&state.db)
                    .await?
                    .into_iter()
                    .collect()
            };
            let gone: Vec<(String, Option<Value>)> = page
                .into_iter()
                .filter(|id| !existing.contains(id))
                .map(|id| (id, None))
                .collect();
            if !gone.is_empty() {
                report.deleted += gone.len() as u64;
                client.bulk(&name, &bulk_lines(gone)).await?;
            }
        }
        anyhow::Ok(())
    }
    .await;
    scroll.close().await;
    result
}

/// `Mastodon::CLI::Search#deploy`. `say` reports progress.
pub async fn deploy(
    state: &AppState,
    options: &Options,
    say: impl Fn(&str),
) -> anyhow::Result<Report> {
    anyhow::ensure!(
        options.concurrency >= 1,
        "Cannot run with this concurrency setting, must be at least 1"
    );
    anyhow::ensure!(
        options.batch_size >= 1,
        "Cannot run with this batch_size setting, must be at least 1"
    );
    let client = state
        .search
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Elasticsearch is not enabled for this instance ([instance.elasticsearch] enabled = true, or ES_ENABLED=true)"))?;
    let indexes: Vec<Index> = if options.only.is_empty() {
        Index::ALL.to_vec()
    } else {
        Index::ALL
            .into_iter()
            .filter(|i| options.only.contains(i))
            .collect()
    };

    let mut changed = vec![];
    for &index in &indexes {
        if specification_changed(client, index).await? {
            changed.push(index);
        }
    }

    if options.only_mapping {
        for &index in &changed {
            say(&format!("Updating mapping for {}", index.class_name()));
            update_specification(client, index).await?;
        }
        say("Updated index mappings");
        return Ok(Report::default());
    }

    // First every index is created with the right structure, so that live
    // updates can already be written.
    for &index in &changed {
        say(&format!("Upgrading {}", index.class_name()));
        purge(client, index).await?;
        clear_progress(state, index).await;
    }

    let mut report = Report::default();
    for &index in &indexes {
        set_refresh_interval(client, index, json!(-1)).await?;
        let result = async {
            if options.import {
                say(&format!("Importing {}", index.class_name()));
                import(state, client, index, options, &mut report).await?;
            }
            if options.clean {
                say(&format!("Cleaning {}", index.class_name()));
                clean_up(state, client, index, options, &mut report).await?;
            }
            anyhow::Ok(())
        }
        .await;
        set_refresh_interval(client, index, json!(index.refresh_interval())).await?;
        result?;
    }
    say(&format!(
        "Indexed {} records, de-indexed {}",
        report.indexed, report.deleted
    ));
    Ok(report)
}
