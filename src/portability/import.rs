//! Mastodon's data import: `Form::Import` reads an uploaded file into a
//! `bulk_imports` row and its `bulk_import_rows`, the member confirms it, and
//! `BulkImportService` then `BulkImportRowService` carry it out, row by row.
//!
//! Both run on the job queue as upstream runs them on Sidekiq: confirming
//! queues a [`BulkImportWorker`], whose `BulkImportService` queues an
//! [`RowWorker`] (`Import::RowWorker`) for each row it leaves, retried six
//! times before it counts as processed and not imported.
//!
//! A row that imported is deleted, as upstream deletes it; one that did not
//! stays, which is what the failures file lists.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::csv::{self, Field};
use crate::api::mastodon::accounts as relationships;
use crate::db::models::Account;
use crate::error::{AppError, AppResult};
use crate::jobs::{Job, Options, Queue};
use crate::state::AppState;

/// `Form::Import::FILE_SIZE_LIMIT`.
pub const FILE_SIZE_LIMIT: usize = 20 * 1024 * 1024;
/// `Form::Import::ROWS_PROCESSING_LIMIT`.
pub const ROWS_PROCESSING_LIMIT: usize = 20_000;
/// `Settings::ImportsController::RECENT_IMPORTS_LIMIT`.
pub const RECENT_IMPORTS_LIMIT: i64 = 10;
/// `BulkImport::CONFIRM_PERIOD`, in minutes.
const CONFIRM_PERIOD_MINUTES: i32 = 10;
/// `BulkImport::ARCHIVE_PERIOD`, in days.
const ARCHIVE_PERIOD_DAYS: i32 = 7;
/// `FollowLimitValidator::LIMIT` and `RATIO`.
const FOLLOW_LIMIT: i64 = 7_500;
const FOLLOW_RATIO: f64 = 1.1;
/// `List::PER_ACCOUNT_LIMIT` and `TITLE_LENGTH_LIMIT`.
const LIST_PER_ACCOUNT_LIMIT: i64 = 50;
const LIST_TITLE_MAX: usize = 256;
/// `CustomFilterKeyword::KEYWORD_LENGTH_LIMIT`.
const KEYWORD_MAX: usize = 512;

/// `BulkImport#type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportType {
    Following,
    Blocking,
    Muting,
    DomainBlocking,
    Bookmarks,
    Lists,
    CustomFilters,
}

impl ImportType {
    pub fn from_i32(value: i32) -> Option<Self> {
        Some(match value {
            0 => Self::Following,
            1 => Self::Blocking,
            2 => Self::Muting,
            3 => Self::DomainBlocking,
            4 => Self::Bookmarks,
            5 => Self::Lists,
            6 => Self::CustomFilters,
            _ => return None,
        })
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "following" => Self::Following,
            "blocking" => Self::Blocking,
            "muting" => Self::Muting,
            "domain_blocking" => Self::DomainBlocking,
            "bookmarks" => Self::Bookmarks,
            "lists" => Self::Lists,
            "custom_filters" => Self::CustomFilters,
            _ => return None,
        })
    }

    pub fn as_i32(self) -> i32 {
        self as i32
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Following => "following",
            Self::Blocking => "blocking",
            Self::Muting => "muting",
            Self::DomainBlocking => "domain_blocking",
            Self::Bookmarks => "bookmarks",
            Self::Lists => "lists",
            Self::CustomFilters => "custom_filters",
        }
    }

    /// The scope a token needs to import it.
    pub fn scope(self) -> &'static str {
        match self {
            Self::Following => "write:follows",
            Self::Blocking | Self::DomainBlocking => "write:blocks",
            Self::Muting => "write:mutes",
            Self::Bookmarks => "write:bookmarks",
            Self::Lists => "write:lists",
            Self::CustomFilters => "write:filters",
        }
    }

    /// `Form::Import::EXPECTED_HEADERS_BY_TYPE`.
    fn expected_headers(self) -> &'static [&'static str] {
        match self {
            Self::Following => &[
                "Account address",
                "Show boosts",
                "Notify on new posts",
                "Languages",
            ],
            Self::Blocking => &["Account address"],
            Self::Muting => &["Account address", "Hide notifications"],
            Self::DomainBlocking => &["#domain"],
            Self::Bookmarks => &["#uri"],
            Self::Lists => &["List name", "Account address"],
            Self::CustomFilters => &[],
        }
    }

    /// `Form::Import#default_csv_headers`: what a file without a header row
    /// is read as.
    fn default_csv_headers(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Following | Self::Blocking | Self::Muting => Some(&["Account address"]),
            Self::DomainBlocking => Some(&["#domain"]),
            Self::Bookmarks => Some(&["#uri"]),
            Self::Lists => Some(&["List name", "Account address"]),
            Self::CustomFilters => None,
        }
    }

    /// `Settings::ImportsController::TYPE_TO_FILENAME_MAP`.
    fn failures_filename(self) -> &'static str {
        match self {
            Self::Following => "following_accounts_failures.csv",
            Self::Blocking => "blocked_accounts_failures.csv",
            Self::Muting => "muted_accounts_failures.csv",
            Self::DomainBlocking => "blocked_domains_failures.csv",
            Self::Bookmarks => "bookmarks_failures.csv",
            Self::Lists => "lists_failures.csv",
            Self::CustomFilters => "custom_filters_failures.json",
        }
    }
}

/// `BulkImport#state`.
pub mod import_state {
    pub const UNCONFIRMED: i32 = 0;
    pub const SCHEDULED: i32 = 1;
    pub const IN_PROGRESS: i32 = 2;
    pub const FINISHED: i32 = 3;

    pub fn to_str(state: i32) -> &'static str {
        match state {
            UNCONFIRMED => "unconfirmed",
            SCHEDULED => "scheduled",
            IN_PROGRESS => "in_progress",
            _ => "finished",
        }
    }
}

/// `Form::Import::KNOWN_FIRST_HEADERS`.
const KNOWN_FIRST_HEADERS: &[&str] = &["Account address", "#domain", "#uri", "List name"];

/// `Form::Import::ATTRIBUTE_BY_HEADER`.
fn attribute_for(header: &str) -> &'static str {
    match header {
        "Account address" => "acct",
        "Show boosts" => "show_reblogs",
        "Notify on new posts" => "notify",
        "Languages" => "languages",
        "Hide notifications" => "hide_notifications",
        "#domain" => "domain",
        "#uri" => "uri",
        _ => "list_name",
    }
}

/// An uploaded file.
#[derive(Debug, Default)]
pub struct Upload {
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

/// What the import form submits.
#[derive(Debug, Default)]
pub struct ImportForm {
    pub r#type: Option<String>,
    pub mode: Option<String>,
    pub data: Option<Upload>,
}

/// A `bulk_imports` row, as the API shows it.
#[derive(Debug, Serialize)]
pub struct BulkImport {
    pub id: String,
    pub r#type: &'static str,
    pub state: &'static str,
    pub overwrite: bool,
    pub original_filename: String,
    pub likely_mismatched: bool,
    pub missing_status: bool,
    pub total_items: i32,
    pub processed_items: i32,
    pub imported_items: i32,
    /// `BulkImport#failure_count`.
    pub failure_count: i32,
    pub created_at: String,
    pub finished_at: Option<String>,
}

fn timestamp(time: chrono::NaiveDateTime) -> String {
    time.and_utc()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

struct ImportRecord {
    id: i64,
    r#type: i32,
    state: i32,
    overwrite: bool,
    original_filename: String,
    likely_mismatched: bool,
    missing_status: bool,
    total_items: i32,
    processed_items: i32,
    imported_items: i32,
    account_id: i64,
    created_at: chrono::NaiveDateTime,
    finished_at: Option<chrono::NaiveDateTime>,
}

impl ImportRecord {
    fn import_type(&self) -> ImportType {
        ImportType::from_i32(self.r#type).unwrap_or(ImportType::Following)
    }

    fn into_api(self) -> BulkImport {
        BulkImport {
            id: self.id.to_string(),
            r#type: self.import_type().as_str(),
            state: import_state::to_str(self.state),
            overwrite: self.overwrite,
            original_filename: self.original_filename,
            likely_mismatched: self.likely_mismatched,
            missing_status: self.missing_status,
            total_items: self.total_items,
            processed_items: self.processed_items,
            imported_items: self.imported_items,
            failure_count: self.processed_items - self.imported_items,
            created_at: timestamp(self.created_at),
            finished_at: self.finished_at.map(timestamp),
        }
    }
}

async fn load_record(state: &AppState, id: i64) -> AppResult<Option<ImportRecord>> {
    Ok(sqlx::query_as!(
        ImportRecord,
        r#"SELECT id, type, state, overwrite, original_filename, likely_mismatched,
                  missing_status, total_items, processed_items, imported_items,
                  account_id, created_at, finished_at
           FROM bulk_imports WHERE id = $1"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?)
}

/// One of the account's imports.
pub async fn show(state: &AppState, account_id: i64, id: i64) -> AppResult<BulkImport> {
    load_record(state, id)
        .await?
        .filter(|record| record.account_id == account_id)
        .map(ImportRecord::into_api)
        .ok_or(AppError::NotFound)
}

/// `set_recent_imports`: the account's ten newest imports.
pub async fn recent(state: &AppState, account_id: i64) -> AppResult<Vec<BulkImport>> {
    let records = sqlx::query_as!(
        ImportRecord,
        r#"SELECT id, type, state, overwrite, original_filename, likely_mismatched,
                  missing_status, total_items, processed_items, imported_items,
                  account_id, created_at, finished_at
           FROM bulk_imports WHERE account_id = $1 ORDER BY id DESC LIMIT $2"#,
        account_id,
        RECENT_IMPORTS_LIMIT,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(records.into_iter().map(ImportRecord::into_api).collect())
}

// ── Form::Import ─────────────────────────────────────────────────────────

/// Ruby's `String#strip`.
fn ruby_strip(value: &str) -> &str {
    value
        .trim_start_matches(['\t', '\n', '\x0b', '\x0c', '\r', ' '])
        .trim_end_matches(['\t', '\n', '\x0b', '\x0c', '\r', ' ', '\0'])
}

/// `ActiveModel::Type::Boolean.new.cast(field.downcase)`.
fn ruby_boolean(field: &str) -> Option<bool> {
    let field = field.to_lowercase();
    if field.is_empty() {
        None
    } else {
        Some(!matches!(field.as_str(), "0" | "f" | "false" | "off"))
    }
}

/// `Form::Import#csv_data`'s converter, for a field that was not empty and
/// unquoted (Ruby's CSV hands those through as `nil` untouched).
fn convert(header: Option<&str>, field: &str) -> Value {
    match header {
        Some("Show boosts" | "Notify on new posts" | "Hide notifications") => {
            ruby_boolean(field).map_or(Value::Null, Value::Bool)
        }
        Some("Languages") => {
            // `field.split(',')` drops trailing empty pieces.
            let mut pieces: Vec<&str> = field.split(',').collect();
            while pieces.last() == Some(&"") {
                pieces.pop();
            }
            if pieces.is_empty() {
                Value::Null
            } else {
                Value::Array(
                    pieces
                        .into_iter()
                        .map(|piece| Value::String(ruby_strip(piece).to_owned()))
                        .collect(),
                )
            }
        }
        Some("Account address") => {
            let acct = ruby_strip(field);
            Value::String(acct.strip_prefix('@').unwrap_or(acct).to_owned())
        }
        Some("#domain") => Value::String(ruby_strip(field).to_lowercase()),
        Some("#uri" | "List name") => Value::String(ruby_strip(field).to_owned()),
        _ => Value::String(field.to_owned()),
    }
}

enum CsvError {
    Malformed(String),
    Empty,
}

/// The file read as upstream reads it: with its own header row when the
/// first field of the first row is one of `KNOWN_FIRST_HEADERS`, and
/// otherwise with the type's default headers and every row as data.
struct CsvData {
    headers: Vec<Option<String>>,
    rows: Vec<Vec<Field>>,
}

impl CsvData {
    fn read(bytes: &[u8], import_type: ImportType) -> Result<Self, CsvError> {
        let mut rows = csv::parse(bytes).map_err(|e| CsvError::Malformed(e.0))?;
        if rows.is_empty() {
            return Err(CsvError::Empty);
        }
        let first = rows[0].first().cloned().flatten();
        if first
            .as_deref()
            .is_some_and(|first| KNOWN_FIRST_HEADERS.contains(&first))
        {
            let headers = rows.remove(0);
            return Ok(Self { headers, rows });
        }
        let headers = import_type
            .default_csv_headers()
            .unwrap_or_default()
            .iter()
            .map(|h| Some((*h).to_owned()))
            .collect();
        Ok(Self { headers, rows })
    }

    fn has_header(&self, header: &str) -> bool {
        self.headers.iter().any(|h| h.as_deref() == Some(header))
    }

    /// `parsed_rows`: each row as `row.to_h.slice(*expected_headers)`, keyed
    /// by attribute.
    fn parsed_rows(&self, import_type: ImportType) -> Vec<Value> {
        let expected = import_type.expected_headers();
        self.rows
            .iter()
            .take(ROWS_PROCESSING_LIMIT + 1)
            .map(|fields| {
                // `CSV::Row#to_h`: every header, padded with nil; a repeated
                // header keeps its first field.
                let width = self.headers.len().max(fields.len());
                let mut seen: Vec<Option<&str>> = Vec::new();
                let mut hash: Vec<(Option<&str>, Value)> = Vec::new();
                for index in 0..width {
                    let header = self.headers.get(index).and_then(|h| h.as_deref());
                    if seen.contains(&header) {
                        continue;
                    }
                    seen.push(header);
                    let value = match fields.get(index).cloned().flatten() {
                        Some(field) => convert(header, &field),
                        None => Value::Null,
                    };
                    hash.push((header, value));
                }
                let mut data = Map::new();
                for wanted in expected {
                    if let Some((_, value)) = hash.iter().find(|(h, _)| *h == Some(*wanted)) {
                        data.insert(attribute_for(wanted).to_owned(), value.clone());
                    }
                }
                Value::Object(data)
            })
            .collect()
    }
}

/// `FollowLimitValidator.limit_for_account`.
async fn follow_limit(state: &AppState, account_id: i64) -> AppResult<(i64, i64)> {
    let stats = sqlx::query!(
        "SELECT following_count, followers_count FROM account_stats WHERE account_id = $1",
        account_id,
    )
    .fetch_optional(&state.db)
    .await?;
    let following = stats.as_ref().map_or(0, |s| s.following_count);
    let followers = stats.as_ref().map_or(0, |s| s.followers_count);
    let limit = if following < FOLLOW_LIMIT {
        FOLLOW_LIMIT
    } else {
        ((followers as f64 * FOLLOW_RATIO).round() as i64).max(FOLLOW_LIMIT)
    };
    Ok((limit, following))
}

/// What `Form::Import#save` stores, once the form is valid.
struct Prepared {
    rows: Vec<Value>,
    likely_mismatched: bool,
    missing_status: bool,
}

fn data_error(errors: &mut Vec<String>, message: impl AsRef<str>) {
    errors.push(format!("Data {}", message.as_ref()));
}

/// `Settings::ImportsController#create`: validate the upload as
/// `Form::Import` does and store it, unconfirmed. A refusal is a 422 with
/// upstream's messages.
pub async fn create(state: &AppState, account_id: i64, form: ImportForm) -> AppResult<BulkImport> {
    let mut errors = Vec::new();
    let type_name = form.r#type.as_deref().filter(|t| !t.trim().is_empty());
    let import_type = match type_name {
        None => {
            errors.push("Type can't be blank".to_owned());
            None
        }
        Some(name) => match ImportType::parse(name) {
            Some(import_type) => Some(import_type),
            None => {
                errors.push("Type is not included in the list".to_owned());
                None
            }
        },
    };
    if form.data.is_none() {
        errors.push("Data can't be blank".to_owned());
    }
    let overwrite = form.mode.as_deref() == Some("overwrite");

    let mut prepared = None;
    if let (Some(import_type), Some(data)) = (import_type, form.data.as_ref()) {
        prepared =
            validate_data(state, account_id, import_type, overwrite, data, &mut errors).await?;
    }
    if !errors.is_empty() {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: {}",
            errors.join(", ")
        )));
    }
    let (Some(import_type), Some(data), Some(prepared)) = (import_type, form.data, prepared) else {
        return Err(AppError::Unprocessable("Validation failed".into()));
    };

    let mut tx = state.db.begin().await?;
    let id = sqlx::query_scalar!(
        r#"INSERT INTO bulk_imports (type, state, overwrite, original_filename, likely_mismatched,
                                     missing_status, account_id, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, now(), now())
           RETURNING id"#,
        import_type.as_i32(),
        import_state::UNCONFIRMED,
        overwrite,
        data.filename.unwrap_or_default(),
        prepared.likely_mismatched,
        prepared.missing_status,
        account_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    let inserted = sqlx::query!(
        r#"INSERT INTO bulk_import_rows (bulk_import_id, data, created_at, updated_at)
           SELECT $1, d, now(), now() FROM UNNEST($2::jsonb[]) WITH ORDINALITY AS t(d, n)
           ORDER BY n"#,
        id,
        &prepared.rows,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query!(
        "UPDATE bulk_imports SET total_items = $2, updated_at = now() WHERE id = $1",
        id,
        inserted as i32,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    show(state, account_id, id).await
}

/// `Form::Import#validate_data`, and what `save` would store.
async fn validate_data(
    state: &AppState,
    account_id: i64,
    import_type: ImportType,
    overwrite: bool,
    data: &Upload,
    errors: &mut Vec<String>,
) -> AppResult<Option<Prepared>> {
    if data.bytes.len() > FILE_SIZE_LIMIT {
        data_error(errors, "File is too large");
        return Ok(None);
    }
    let filename = data.filename.as_deref();
    let file_name_matches = |prefix: &str| filename.is_some_and(|f| f.starts_with(prefix));

    if data.content_type.as_deref() == Some("application/json") {
        // `validate_json_data`, as 4.7.2 orders it.
        let json: Value = match serde_json::from_slice(&data.bytes) {
            Ok(json) => json,
            Err(error) => {
                data_error(errors, format!("Invalid JSON file. Error: {error}"));
                return Ok(None);
            }
        };
        if import_type != ImportType::CustomFilters {
            data_error(errors, "Incompatible with the selected import type");
            return Ok(None);
        }
        let filters = json
            .get("custom_filters")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if filters.is_empty() {
            data_error(
                errors,
                "This file does not contain any data matching the given import type",
            );
            return Ok(None);
        }
        if filters.len() > ROWS_PROCESSING_LIMIT {
            data_error(
                errors,
                format!("contains more than {ROWS_PROCESSING_LIMIT} rows"),
            );
            return Ok(None);
        }
        // `missing_status?`: some filtered post is unknown here.
        let mut uris: Vec<String> = filters
            .iter()
            .filter_map(|f| f.get("statuses").and_then(Value::as_array))
            .flatten()
            .filter_map(|uri| uri.as_str().map(str::to_owned))
            .collect();
        uris.sort();
        uris.dedup();
        let known = sqlx::query_scalar!(
            r#"SELECT count(*) AS "count!" FROM statuses WHERE uri = ANY($1) AND deleted_at IS NULL"#,
            &uris,
        )
        .fetch_one(&state.db)
        .await?;
        // `likely_mismatched_json?`: the file names its own type.
        let guessed = json
            .get("custom_filters")
            .map(|_| ImportType::CustomFilters);
        return Ok(Some(Prepared {
            rows: filters,
            likely_mismatched: guessed.is_some_and(|g| g != import_type),
            missing_status: uris.len() as i64 != known,
        }));
    }

    let csv = match CsvData::read(&data.bytes, import_type) {
        Ok(csv) => csv,
        Err(CsvError::Malformed(error)) => {
            data_error(errors, format!("Invalid CSV file. Error: {error}"));
            return Ok(None);
        }
        Err(CsvError::Empty) => {
            data_error(errors, "Empty CSV file");
            return Ok(None);
        }
    };
    // `validate_csv_data`.
    let compatible = import_type
        .default_csv_headers()
        .is_some_and(|defaults| defaults.iter().all(|h| csv.has_header(h)));
    if !compatible {
        data_error(errors, "Incompatible with the selected import type");
        return Ok(None);
    }
    let row_count = csv.rows.len().min(ROWS_PROCESSING_LIMIT + 2);
    if row_count > ROWS_PROCESSING_LIMIT {
        data_error(
            errors,
            format!("contains more than {ROWS_PROCESSING_LIMIT} rows"),
        );
    }
    if import_type == ImportType::Following {
        let (base_limit, following) = follow_limit(state, account_id).await?;
        let limit = if overwrite {
            base_limit
        } else {
            base_limit - following
        };
        if row_count as i64 > limit {
            data_error(
                errors,
                format!("You cannot follow more than {base_limit} people"),
            );
        }
    }
    if !errors.is_empty() {
        return Ok(None);
    }

    // `guessed_type`, from the headers and then the file's name.
    let guessed = if csv.has_header("Hide notifications")
        || file_name_matches("mutes")
        || file_name_matches("muted_accounts")
    {
        Some(ImportType::Muting)
    } else if csv.has_header("Show boosts")
        || csv.has_header("Notify on new posts")
        || csv.has_header("Languages")
        || file_name_matches("follows")
        || file_name_matches("following_accounts")
    {
        Some(ImportType::Following)
    } else if file_name_matches("blocks") || file_name_matches("blocked_accounts") {
        Some(ImportType::Blocking)
    } else if file_name_matches("domain_blocks") || file_name_matches("blocked_domains") {
        Some(ImportType::DomainBlocking)
    } else if file_name_matches("bookmarks") {
        Some(ImportType::Bookmarks)
    } else if file_name_matches("lists") {
        Some(ImportType::Lists)
    } else {
        None
    };
    Ok(Some(Prepared {
        rows: csv.parsed_rows(import_type),
        likely_mismatched: guessed.is_some_and(|g| g != import_type),
        missing_status: false,
    }))
}

/// `Settings::ImportsController#destroy`: an unconfirmed import is dropped.
pub async fn destroy(state: &AppState, account_id: i64, id: i64) -> AppResult<()> {
    let deleted = sqlx::query!(
        "DELETE FROM bulk_imports WHERE id = $1 AND account_id = $2 AND state = $3",
        id,
        account_id,
        import_state::UNCONFIRMED,
    )
    .execute(&state.db)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// `Settings::ImportsController#confirm`: an unconfirmed import is
/// scheduled, and `BulkImportWorker` takes it from there.
pub async fn confirm(state: &AppState, account_id: i64, id: i64) -> AppResult<BulkImport> {
    let confirmed = sqlx::query!(
        "UPDATE bulk_imports SET state = $4, updated_at = now()
         WHERE id = $1 AND account_id = $2 AND state = $3",
        id,
        account_id,
        import_state::UNCONFIRMED,
        import_state::SCHEDULED,
    )
    .execute(&state.db)
    .await?;
    if confirmed.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    crate::jobs::perform_async(state, BulkImportWorker { bulk_import_id: id }).await?;
    show(state, account_id, id).await
}

// ── The failures file ───────────────────────────────────────────────────

/// Pg's JSON text without the spaces it puts after `:` and `,`, which is how
/// `JSON.generate` writes the hash Rails read from it, keys in the same order.
fn compact_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in text.chars() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if !c.is_whitespace() {
            out.push(c);
        }
    }
    out
}

/// Ruby's `to_s` on what a row's JSON holds, for a CSV field.
fn ruby_field(value: Option<&Value>, default: Option<&str>) -> Field {
    match value {
        None => default.map(str::to_owned),
        Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        Some(other) => Some(other.to_string()),
    }
}

/// `Settings::ImportsController#failures`: the rows of a finished import
/// that did not import, as a file of the kind they came from. Returns the
/// file's name, its content type and its contents.
pub async fn failures(
    state: &AppState,
    account_id: i64,
    id: i64,
) -> AppResult<(&'static str, &'static str, String)> {
    let record = load_record(state, id)
        .await?
        .filter(|r| r.account_id == account_id && r.state == import_state::FINISHED)
        .ok_or(AppError::NotFound)?;
    let import_type = record.import_type();
    let rows = sqlx::query!(
        r#"SELECT data, data::text AS "text" FROM bulk_import_rows
           WHERE bulk_import_id = $1 ORDER BY id"#,
        id,
    )
    .fetch_all(&state.db)
    .await?;
    let filename = import_type.failures_filename();

    if import_type == ImportType::CustomFilters {
        let items: Vec<String> = rows
            .iter()
            .map(|row| compact_json(row.text.as_deref().unwrap_or("null")))
            .collect();
        let body = format!("{{\"custom_filters\":[{}]}}", items.join(","));
        return Ok((filename, "application/json", body));
    }

    let mut out = String::new();
    let headers: &[&str] = match import_type {
        ImportType::Following => &[
            "Account address",
            "Show boosts",
            "Notify on new posts",
            "Languages",
        ],
        ImportType::Muting => &["Account address", "Hide notifications"],
        _ => &[],
    };
    if !headers.is_empty() {
        let headers: Vec<Field> = headers.iter().map(|h| Some((*h).to_owned())).collect();
        csv::write_row(&mut out, &headers);
    }
    for row in rows {
        let data = row.data.unwrap_or(Value::Null);
        let get = |key: &str| data.get(key);
        let fields: Vec<Field> = match import_type {
            ImportType::Following => vec![
                ruby_field(get("acct"), None),
                ruby_field(get("show_reblogs"), Some("true")),
                ruby_field(get("notify"), Some("false")),
                get("languages").and_then(Value::as_array).map(|languages| {
                    languages
                        .iter()
                        .map(|l| {
                            l.as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| l.to_string())
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                }),
            ],
            ImportType::Blocking => vec![ruby_field(get("acct"), None)],
            ImportType::Muting => vec![
                ruby_field(get("acct"), None),
                ruby_field(get("hide_notifications"), Some("true")),
            ],
            ImportType::DomainBlocking => vec![ruby_field(get("domain"), None)],
            ImportType::Bookmarks => vec![ruby_field(get("uri"), None)],
            ImportType::Lists => vec![
                ruby_field(get("list_name"), None),
                ruby_field(get("acct"), None),
            ],
            ImportType::CustomFilters => vec![],
        };
        csv::write_row(&mut out, &fields);
    }
    Ok((filename, "text/csv", out))
}

// ── BulkImportWorker ────────────────────────────────────────────────────

/// `BulkImportWorker`: a confirmed import's first pass, `BulkImportService`,
/// which queues a [`RowWorker`] for each row it leaves.
#[derive(Serialize, Deserialize)]
pub struct BulkImportWorker {
    pub bulk_import_id: i64,
}

impl Job for BulkImportWorker {
    const KIND: &'static str = "BulkImportWorker";
    // `sidekiq_options queue: 'pull', retry: false`.
    const OPTIONS: Options = Options::DEFAULT.queue(Queue::Pull).no_retry();

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        bulk_import_worker(state, self.bulk_import_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))
    }
}

/// `BulkImportWorker#perform`, then `BulkImportService#call`.
async fn bulk_import_worker(state: &AppState, id: i64) -> AppResult<()> {
    // `BulkImport.find`, which raises for an import that has gone.
    let record = load_record(state, id).await?.ok_or(AppError::NotFound)?;
    sqlx::query!(
        "UPDATE bulk_imports SET state = $2, updated_at = now() WHERE id = $1",
        id,
        import_state::IN_PROGRESS,
    )
    .execute(&state.db)
    .await?;
    let result = match load_account(state, record.account_id).await {
        Ok(Some(account)) => bulk_import_service(state, &record, &account).await,
        Ok(None) => Err(AppError::NotFound),
        Err(error) => Err(error),
    };
    match result {
        // `processing_complete?`, of the service's own copy of the import,
        // which counts only what the service itself settled: rows run by
        // the jobs it queued finish the import themselves.
        Ok(processed) => {
            if processed == record.total_items {
                finish(state, id).await?;
            }
            Ok(())
        }
        // `rescue`: the import is finished as it stands, and the error
        // raised again.
        Err(error) => {
            finish(state, id).await?;
            Err(error)
        }
    }
}

/// `@import.update!(state: :finished, finished_at: Time.now.utc)`.
async fn finish(state: &AppState, id: i64) -> AppResult<()> {
    sqlx::query!(
        "UPDATE bulk_imports SET state = $2, finished_at = now(), updated_at = now()
         WHERE id = $1",
        id,
        import_state::FINISHED,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

async fn load_account(state: &AppState, id: i64) -> AppResult<Option<Account>> {
    Ok(
        sqlx::query_as!(Account, "SELECT * FROM accounts WHERE id = $1", id)
            .fetch_optional(&state.db)
            .await?,
    )
}

/// `BulkImport.progress!`: one more row processed, and imported if it was;
/// the import is finished once as many rows have been processed as it has.
async fn progress(state: &AppState, id: i64, imported: bool) -> AppResult<()> {
    // `increment_counter`, atomically, as upstream does.
    let complete = sqlx::query_scalar!(
        r#"UPDATE bulk_imports
           SET processed_items = processed_items + 1,
               imported_items = imported_items + (CASE WHEN $2 THEN 1 ELSE 0 END)
           WHERE id = $1
           RETURNING processed_items = total_items AS "complete!""#,
        id,
        imported,
    )
    .fetch_optional(&state.db)
    .await?
    // `BulkImport.find`, which raises for an import that has gone.
    .ok_or(AppError::NotFound)?;
    if complete {
        finish(state, id).await?;
    }
    Ok(())
}

/// A row the first pass settled: it goes, and counts as processed and
/// imported.
async fn settle_row(state: &AppState, id: i64, row_id: i64) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    sqlx::query!("DELETE FROM bulk_import_rows WHERE id = $1", row_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "UPDATE bulk_imports SET processed_items = processed_items + 1,
                                 imported_items = imported_items + 1
         WHERE id = $1",
        id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

struct Row {
    id: i64,
    data: Value,
}

async fn all_rows(state: &AppState, id: i64) -> AppResult<Vec<Row>> {
    Ok(sqlx::query!(
        "SELECT id, data FROM bulk_import_rows WHERE bulk_import_id = $1 ORDER BY id",
        id,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| Row {
        id: r.id,
        data: r.data.unwrap_or(Value::Null),
    })
    .collect())
}

fn data_str<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    data.get(key).and_then(Value::as_str)
}

fn data_bool(data: &Value, key: &str) -> Option<bool> {
    data.get(key).and_then(Value::as_bool)
}

fn data_languages(data: &Value) -> Option<Vec<String>> {
    data.get("languages").and_then(Value::as_array).map(|l| {
        l.iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    })
}

/// Ruby's `NoMethodError` for a method called on a value that is not there.
fn undefined_method(method: &str) -> AppError {
    AppError::Unrescued(format!("undefined method '{method}' for nil"))
}

/// `BulkImportService#extract_rows_by_acct`: `index_by` the handle, a local
/// one without its domain. A repeated handle keeps the place of its first
/// row and the last row itself, so the rows before it are never queued.
fn rows_by_acct(state: &AppState, rows: Vec<Row>) -> AppResult<IndexMap<String, Row>> {
    let suffix = format!("@{}", state.instance.domain);
    let mut map = IndexMap::new();
    for row in rows {
        let acct = data_str(&row.data, "acct").ok_or_else(|| undefined_method("delete_suffix"))?;
        let acct = acct
            .strip_suffix(suffix.as_str())
            .unwrap_or(acct)
            .to_owned();
        map.insert(acct, row);
    }
    Ok(map)
}

/// `Account#acct`.
fn raw_acct(username: &str, domain: Option<&str>) -> String {
    match domain {
        Some(domain) => format!("{username}@{domain}"),
        None => username.to_owned(),
    }
}

/// `BulkImportService#call`'s work: in overwrite mode, what the file does
/// not list is undone and what it does list is settled; the rows left are
/// queued, one [`RowWorker`] each. Returns `processed_items` as the
/// service's own copy of the import has it once it is done.
async fn bulk_import_service(
    state: &AppState,
    record: &ImportRecord,
    account: &Account,
) -> AppResult<i32> {
    let id = record.id;
    let mut processed = record.processed_items;
    let queued: Vec<i64> = match record.import_type() {
        ImportType::Following => {
            let mut rows = rows_by_acct(state, all_rows(state, id).await?)?;
            if record.overwrite {
                let followees = sqlx::query_as!(
                    Account,
                    "SELECT a.* FROM follows f JOIN accounts a ON a.id = f.target_account_id
                     WHERE f.account_id = $1",
                    account.id,
                )
                .fetch_all(&state.db)
                .await?;
                for followee in followees {
                    let acct = raw_acct(&followee.username, followee.domain.as_deref());
                    match rows.shift_remove(&acct) {
                        None => {
                            relationships::unfollow(state, account.id, followee.id, false).await?
                        }
                        Some(row) => {
                            settle_row(state, id, row.id).await?;
                            processed += 1;
                            relationships::follow(
                                state,
                                account,
                                &followee,
                                relationships::FollowOptions {
                                    reblogs: data_bool(&row.data, "show_reblogs"),
                                    notify: data_bool(&row.data, "notify"),
                                    languages: data_languages(&row.data),
                                    ..Default::default()
                                },
                            )
                            .await?;
                        }
                    }
                }
            }
            rows.values().map(|row| row.id).collect()
        }
        ImportType::Blocking => {
            let mut rows = rows_by_acct(state, all_rows(state, id).await?)?;
            if record.overwrite {
                let blocked = sqlx::query!(
                    "SELECT a.id, a.username, a.domain FROM blocks b
                     JOIN accounts a ON a.id = b.target_account_id WHERE b.account_id = $1",
                    account.id,
                )
                .fetch_all(&state.db)
                .await?;
                for target in blocked {
                    match rows.shift_remove(&raw_acct(&target.username, target.domain.as_deref())) {
                        None => relationships::unblock(state, account.id, target.id).await?,
                        Some(row) => {
                            settle_row(state, id, row.id).await?;
                            processed += 1;
                            relationships::block(state, account.id, target.id).await?;
                        }
                    }
                }
            }
            rows.values().map(|row| row.id).collect()
        }
        ImportType::Muting => {
            let mut rows = rows_by_acct(state, all_rows(state, id).await?)?;
            if record.overwrite {
                let muted = sqlx::query!(
                    "SELECT a.id, a.username, a.domain FROM mutes m
                     JOIN accounts a ON a.id = m.target_account_id WHERE m.account_id = $1",
                    account.id,
                )
                .fetch_all(&state.db)
                .await?;
                for target in muted {
                    match rows.shift_remove(&raw_acct(&target.username, target.domain.as_deref())) {
                        None => relationships::unmute(state, account.id, target.id).await?,
                        Some(row) => {
                            settle_row(state, id, row.id).await?;
                            processed += 1;
                            relationships::mute(
                                state,
                                account.id,
                                target.id,
                                data_bool(&row.data, "hide_notifications").unwrap_or(true),
                                0,
                            )
                            .await?;
                        }
                    }
                }
            }
            rows.values().map(|row| row.id).collect()
        }
        ImportType::DomainBlocking => {
            // `import_domain_blocks!`: no rows are queued; every domain is
            // blocked here. Upstream's overwrite compares each block record
            // with the file's domains, never finds one equal, and so lifts
            // every block before blocking the file's domains again.
            let domains: Vec<Option<String>> = all_rows(state, id)
                .await?
                .into_iter()
                .map(|row| data_str(&row.data, "domain").map(str::to_owned))
                .collect();
            if record.overwrite {
                sqlx::query!(
                    "DELETE FROM account_domain_blocks WHERE account_id = $1",
                    account.id
                )
                .execute(&state.db)
                .await?;
            }
            sqlx::query!("DELETE FROM bulk_import_rows WHERE bulk_import_id = $1", id)
                .execute(&state.db)
                .await?;
            for domain in domains {
                // `AccountDomainBlock` validates the domain's presence.
                let domain = domain
                    .filter(|d| !d.trim().is_empty())
                    .ok_or_else(|| AppError::Unprocessable("Domain can't be blank".into()))?;
                crate::api::mastodon::domain_blocks::block_domain_for(state, account.id, &domain)
                    .await?;
            }
            sqlx::query!(
                "UPDATE bulk_imports SET processed_items = total_items, imported_items = total_items,
                                         updated_at = now()
                 WHERE id = $1",
                id,
            )
            .execute(&state.db)
            .await?;
            processed = record.total_items;
            Vec::new()
        }
        ImportType::Bookmarks => {
            // `index_by { |row| row.data['uri'] }`: a repeated post keeps
            // only its last row, as a repeated handle does.
            let mut rows: IndexMap<Option<String>, Row> = IndexMap::new();
            for row in all_rows(state, id).await? {
                rows.insert(data_str(&row.data, "uri").map(str::to_owned), row);
            }
            if record.overwrite {
                let bookmarks = sqlx::query!(
                    r#"SELECT b.id AS bookmark_id, s.id AS "status_id?", s.uri,
                              (s.reblog_of_id IS NOT NULL) AS "reblog?",
                              a.id AS "account_id?", a.id_scheme, a.username AS "username?", a.domain
                       FROM bookmarks b
                       LEFT JOIN statuses s ON s.id = b.status_id AND s.deleted_at IS NULL
                       LEFT JOIN accounts a ON a.id = s.account_id
                       WHERE b.account_id = $1"#,
                    account.id,
                )
                .fetch_all(&state.db)
                .await?;
                for bookmark in bookmarks {
                    let uri = match (bookmark.status_id, bookmark.account_id, &bookmark.username) {
                        (Some(status_id), Some(account_id), Some(username)) => {
                            Some(super::status_uri(
                                state,
                                super::StatusUriParts {
                                    status_id,
                                    uri: bookmark.uri.as_deref(),
                                    reblog: bookmark.reblog.unwrap_or(false),
                                    account_id,
                                    account_id_scheme: bookmark.id_scheme,
                                    account_username: username,
                                    account_domain: bookmark.domain.as_deref(),
                                },
                            ))
                        }
                        _ => None,
                    };
                    match uri.and_then(|uri| rows.shift_remove(&Some(uri))) {
                        Some(row) => {
                            settle_row(state, id, row.id).await?;
                            processed += 1;
                        }
                        None => {
                            sqlx::query!(
                                "DELETE FROM bookmarks WHERE id = $1",
                                bookmark.bookmark_id
                            )
                            .execute(&state.db)
                            .await?;
                        }
                    }
                }
            }
            rows.values().map(|row| row.id).collect()
        }
        ImportType::Lists => {
            let rows = all_rows(state, id).await?;
            let mut titles: Vec<String> = Vec::new();
            for row in &rows {
                let title = data_str(&row.data, "list_name")
                    .map(str::to_owned)
                    .ok_or_else(|| AppError::Unprocessable("Title can't be blank".into()))?;
                if !titles.contains(&title) {
                    titles.push(title);
                }
            }
            if record.overwrite {
                let dropped: Vec<i64> = sqlx::query_scalar!(
                    "DELETE FROM lists WHERE account_id = $1 AND NOT (title = ANY($2)) RETURNING id",
                    account.id,
                    &titles,
                )
                .fetch_all(&state.db)
                .await?;
                let mut redis = state.redis.clone();
                for list_id in dropped {
                    crate::feed::delete_list_feed(&mut redis, &state.redis_keys, list_id).await;
                }
                // Membership changes do not reach back into timelines, so
                // upstream simply clears every list.
                sqlx::query!(
                    "DELETE FROM list_accounts WHERE list_id IN (SELECT id FROM lists WHERE account_id = $1)",
                    account.id,
                )
                .execute(&state.db)
                .await?;
            }
            for title in &titles {
                find_or_create_list(state, account.id, title).await?;
            }
            rows.iter().map(|row| row.id).collect()
        }
        ImportType::CustomFilters => {
            let rows = all_rows(state, id).await?;
            if record.overwrite {
                sqlx::query!(
                    "DELETE FROM custom_filters WHERE account_id = $1",
                    account.id
                )
                .execute(&state.db)
                .await?;
                crate::api::mastodon::filters::publish_filters_changed(state, account.id).await;
            }
            rows.iter().map(|row| row.id).collect()
        }
    };
    // `Import::RowWorker.push_bulk`.
    crate::jobs::perform_bulk(
        state,
        queued
            .into_iter()
            .map(|bulk_import_row_id| RowWorker { bulk_import_row_id }),
    )
    .await?;
    Ok(processed)
}

// ── Import::RowWorker ───────────────────────────────────────────────────

/// `Import::RowWorker`: one row of an import, `BulkImportRowService`.
#[derive(Serialize, Deserialize)]
pub struct RowWorker {
    pub bulk_import_row_id: i64,
}

impl Job for RowWorker {
    const KIND: &'static str = "Import::RowWorker";
    // `sidekiq_options queue: 'pull', retry: 6, dead: false`.
    const OPTIONS: Options = Options::DEFAULT.queue(Queue::Pull).retry(6).dead(false);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        row_worker(state, self.bulk_import_row_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))
    }

    /// `sidekiq_retries_exhausted`: the row counts as processed, not
    /// imported, and may finish its import.
    async fn retries_exhausted(self, state: &AppState, _error: &str) {
        let bulk_import_id = sqlx::query_scalar!(
            "SELECT bulk_import_id FROM bulk_import_rows WHERE id = $1",
            self.bulk_import_row_id,
        )
        .fetch_optional(&state.db)
        .await;
        let result = match bulk_import_id {
            Ok(Some(id)) => progress(state, id, false).await,
            Ok(None) => Ok(()),
            Err(error) => Err(error.into()),
        };
        if let Err(error) = result {
            tracing::warn!(
                row_id = self.bulk_import_row_id,
                %error,
                "could not count a failed import row"
            );
        }
    }
}

/// `Import::RowWorker#perform`.
async fn row_worker(state: &AppState, row_id: i64) -> AppResult<()> {
    let row = sqlx::query!(
        r#"SELECT r.bulk_import_id, r.data, b.type AS "import_type", b.account_id
           FROM bulk_import_rows r JOIN bulk_imports b ON b.id = r.bulk_import_id
           WHERE r.id = $1"#,
        row_id,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    let Some(account) = load_account(state, row.account_id).await? else {
        return Ok(());
    };
    let import_type = ImportType::from_i32(row.import_type).unwrap_or(ImportType::Following);
    let data = row.data.unwrap_or(Value::Null);
    let imported = match import_row(state, &account, import_type, &data).await {
        // `rescue ActiveRecord::RecordNotFound`.
        Err(AppError::NotFound) => false,
        result => result?,
    };
    // `mark_as_processed!`.
    if imported {
        sqlx::query!("DELETE FROM bulk_import_rows WHERE id = $1", row_id)
            .execute(&state.db)
            .await?;
    }
    progress(state, row.bulk_import_id, imported).await
}

/// `account.owned_lists.find_or_create_by!(title:)`.
async fn find_or_create_list(state: &AppState, account_id: i64, title: &str) -> AppResult<i64> {
    if let Some(id) = sqlx::query_scalar!(
        "SELECT id FROM lists WHERE account_id = $1 AND title = $2 ORDER BY id LIMIT 1",
        account_id,
        title,
    )
    .fetch_optional(&state.db)
    .await?
    {
        return Ok(id);
    }
    if ruby_strip(title).is_empty() {
        return Err(AppError::Unprocessable(
            "Validation failed: Title can't be blank".into(),
        ));
    }
    if title.chars().count() > LIST_TITLE_MAX {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: Title is too long (maximum is {LIST_TITLE_MAX} characters)"
        )));
    }
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM lists WHERE account_id = $1"#,
        account_id
    )
    .fetch_one(&state.db)
    .await?;
    if count >= LIST_PER_ACCOUNT_LIMIT {
        return Err(AppError::Unprocessable(
            "Validation failed: You have reached the maximum number of lists".into(),
        ));
    }
    Ok(sqlx::query_scalar!(
        "INSERT INTO lists (account_id, title, created_at, updated_at)
         VALUES ($1, $2, now(), now()) RETURNING id",
        account_id,
        title,
    )
    .fetch_one(&state.db)
    .await?)
}

/// `ResolveAccountService` with `check_delivery_availability: true`: a
/// handle on a server deliveries have given up on is not looked up there,
/// though an account already known from it still resolves.
async fn resolve_target(state: &AppState, acct: &str) -> AppResult<Option<Account>> {
    let acct = crate::moves::normalize_acct(acct);
    if let Some((username, domain)) = acct.split_once('@') {
        let domain = domain.to_lowercase();
        let local = domain.eq_ignore_ascii_case(&state.instance.domain)
            || state
                .instance
                .aliases
                .iter()
                .any(|alias| domain.eq_ignore_ascii_case(alias));
        if !local && state.delivery_failures.is_unavailable(&domain) {
            if crate::federation::moderation::domain_not_allowed(state, &domain).await {
                return Ok(None);
            }
            return Ok(sqlx::query_as!(
                Account,
                "SELECT * FROM accounts WHERE lower(username) = lower($1) AND lower(domain) = $2",
                username,
                domain,
            )
            .fetch_optional(&state.db)
            .await?);
        }
    }
    crate::moves::resolve_account(state, &acct, false).await
}

/// `BulkImportRowService#call`: carry out one row. `Ok(false)` is a row
/// that did not import, which stays to be listed among the failures.
async fn import_row(
    state: &AppState,
    account: &Account,
    import_type: ImportType,
    data: &Value,
) -> AppResult<bool> {
    match import_type {
        ImportType::Following | ImportType::Blocking | ImportType::Muting | ImportType::Lists => {
            // `domain(target_acct)` splits the handle, and raises without one.
            let acct = data_str(data, "acct").ok_or_else(|| undefined_method("split"))?;
            let Some(target) = resolve_target(state, acct).await? else {
                return Ok(false);
            };
            match import_type {
                ImportType::Following => {
                    relationships::follow(
                        state,
                        account,
                        &target,
                        relationships::FollowOptions {
                            reblogs: data_bool(data, "show_reblogs"),
                            notify: data_bool(data, "notify"),
                            languages: data_languages(data),
                            ..Default::default()
                        },
                    )
                    .await?;
                }
                ImportType::Blocking => relationships::block(state, account.id, target.id).await?,
                ImportType::Muting => {
                    // `Account#mute!` takes a missing choice as `true`.
                    let hide = data_bool(data, "hide_notifications").unwrap_or(true);
                    relationships::mute(state, account.id, target.id, hide, 0).await?;
                }
                _ => {
                    let title = data_str(data, "list_name").unwrap_or_default();
                    let list_id = find_or_create_list(state, account.id, title).await?;
                    if account.id != target.id {
                        relationships::follow(
                            state,
                            account,
                            &target,
                            relationships::FollowOptions::default(),
                        )
                        .await?;
                    }
                    add_to_list(state, account.id, list_id, target.id).await?;
                }
            }
            Ok(true)
        }
        ImportType::Bookmarks => {
            // `Addressable::URI.parse(nil).normalized_host` raises.
            let uri = data_str(data, "uri").ok_or_else(|| undefined_method("normalized_host"))?;
            let local = url::Url::parse(uri).ok().is_some_and(|url| {
                url.host_str()
                    .is_some_and(|host| host.eq_ignore_ascii_case(&state.instance.domain))
            });
            let mut status_id = crate::federation::local_uri::status(state, uri).await;
            if status_id.is_none() && local {
                return Ok(false);
            }
            if status_id.is_none() {
                status_id = crate::api::ap::inbox::fetch_remote_status(state, uri)
                    .await
                    .unwrap_or(None);
            }
            let Some(status_id) = status_id else {
                return Ok(false);
            };
            // `StatusPolicy#show?`.
            if crate::api::mastodon::resolve_url::authorized_status(
                state,
                status_id,
                Some(account.id),
            )
            .await?
            .is_none()
            {
                return Ok(false);
            }
            sqlx::query!(
                "INSERT INTO bookmarks (account_id, status_id, created_at, updated_at)
                 VALUES ($1, $2, now(), now())
                 ON CONFLICT (account_id, status_id) DO NOTHING",
                account.id,
                status_id,
            )
            .execute(&state.db)
            .await?;
            // `Bookmark`'s `update_index('statuses', :status)`.
            crate::search::elasticsearch::indexing::status_interaction(state, status_id).await;
            Ok(true)
        }
        ImportType::CustomFilters => create_filter(state, account.id, data).await,
        ImportType::DomainBlocking => Ok(true),
    }
}

/// `list.accounts << target`: the owner's follow of the target, or request
/// to follow it, ties it to the list (`ListAccount#set_follow`). Not
/// following it, or having it on the list already, raises as upstream's
/// validations do.
async fn add_to_list(
    state: &AppState,
    owner_id: i64,
    list_id: i64,
    target_id: i64,
) -> AppResult<()> {
    let (follow_id, follow_request_id) = if owner_id == target_id {
        (None, None)
    } else {
        let follow_id = sqlx::query_scalar!(
            "SELECT id FROM follows WHERE account_id = $1 AND target_account_id = $2",
            owner_id,
            target_id,
        )
        .fetch_optional(&state.db)
        .await?;
        let follow_request_id = match follow_id {
            Some(_) => None,
            None => {
                sqlx::query_scalar!(
                "SELECT id FROM follow_requests WHERE account_id = $1 AND target_account_id = $2",
                owner_id,
                target_id,
            )
                .fetch_optional(&state.db)
                .await?
            }
        };
        if follow_id.is_none() && follow_request_id.is_none() {
            return Err(AppError::Unprocessable(
                "Validation failed: Account must be following".into(),
            ));
        }
        (follow_id, follow_request_id)
    };
    let inserted = sqlx::query_scalar!(
        "INSERT INTO list_accounts (list_id, account_id, follow_id, follow_request_id)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT DO NOTHING RETURNING id",
        list_id,
        target_id,
        follow_id,
        follow_request_id,
    )
    .fetch_optional(&state.db)
    .await?;
    match inserted {
        Some(_) => Ok(()),
        None => Err(AppError::Unprocessable(
            "Validation failed: Account has already been taken".into(),
        )),
    }
}

/// A time as Rails reads one assigned as a string: what `JSON.generate`
/// wrote (`2026-01-01 00:00:00 UTC`), or ISO 8601. Anything else is nil.
fn parse_time(value: &str) -> Option<chrono::NaiveDateTime> {
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(time.naive_utc());
    }
    let value = value.strip_suffix(" UTC").unwrap_or(value);
    chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S").ok()
}

/// What a string attribute holds once a JSON value is assigned to it: nil
/// is blank, as is a missing value.
fn string_attribute(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(true)) => "t".into(),
        Some(Value::Bool(false)) => "f".into(),
        Some(other) => other.to_string(),
    }
}

/// What a boolean attribute holds once a JSON value is assigned to it
/// (`ActiveModel::Type::Boolean#cast`): nil for nil or an empty string.
fn boolean_attribute(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Null => None,
        Value::Bool(b) => Some(*b),
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(!matches!(
            s.as_str(),
            "0" | "f" | "F" | "false" | "FALSE" | "off" | "OFF"
        )),
        Value::Number(n) => Some(n.as_i64() != Some(0)),
        _ => Some(true),
    }
}

/// `BulkImportRowService`'s custom filter, in upstream's order: the filter
/// is created with its title and context, then given its keywords, action,
/// expiry and posts, and saved again. Each step that raises leaves what the
/// steps before it saved, so a row whose action is bad leaves its filter
/// and keywords behind, and leaves another each time it is retried.
async fn create_filter(state: &AppState, account_id: i64, data: &Value) -> AppResult<bool> {
    use crate::api::mastodon::filters::{FILTER_TITLE_MAX, VALID_FILTER_CONTEXTS};

    // `@account.custom_filters.create!(title:, context:)`.
    let title = string_attribute(data.get("title"));
    // `normalizes :context`, which calls `strip` on every value.
    let context: Vec<String> = match data.get("context") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|c| {
                c.as_str()
                    .map(|c| ruby_strip(c).to_owned())
                    .ok_or_else(|| undefined_method("strip"))
            })
            .collect::<AppResult<Vec<_>>>()?
            .into_iter()
            .filter(|c| !c.is_empty())
            .collect(),
        Some(_) => return Err(undefined_method("map")),
    };
    let mut errors = Vec::new();
    if ruby_strip(&title).is_empty() {
        errors.push("Title can't be blank".to_owned());
    }
    if context.is_empty() {
        errors.push("Context can't be blank".to_owned());
    }
    if title.chars().count() > FILTER_TITLE_MAX {
        errors.push(format!(
            "Title is too long (maximum is {FILTER_TITLE_MAX} characters)"
        ));
    }
    if context.is_empty()
        || context
            .iter()
            .any(|c| !VALID_FILTER_CONTEXTS.contains(&c.as_str()))
    {
        errors.push("Context None or invalid context supplied".to_owned());
    }
    if !errors.is_empty() {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: {}",
            errors.join(", ")
        )));
    }
    let filter_id = sqlx::query_scalar!(
        r#"INSERT INTO custom_filters (account_id, phrase, context, action, created_at, updated_at)
           VALUES ($1, $2, $3, $4, now(), now()) RETURNING id"#,
        account_id,
        title,
        &context,
        crate::db::models::filter_action::WARN,
    )
    .fetch_one(&state.db)
    .await?;
    let result = complete_filter(state, account_id, filter_id, data).await;
    // `invalidate_cache!`, after each commit.
    crate::api::mastodon::filters::publish_filters_changed(state, account_id).await;
    result.map(|()| true)
}

/// The rest of the custom filter row, once the filter has been created.
async fn complete_filter(
    state: &AppState,
    account_id: i64,
    filter_id: i64,
    data: &Value,
) -> AppResult<()> {
    // `filter.keywords = …`, which saves them at once: one that is invalid
    // raises, and none is kept.
    let Some(Value::Array(items)) = data.get("keywords_attributes") else {
        return Err(undefined_method("map"));
    };
    let not_saved = || {
        AppError::Unprocessable(
            "Failed to replace keywords because one or more of the new records could not be saved."
                .into(),
        )
    };
    let mut keywords = Vec::new();
    for keyword in items {
        let Value::Object(keyword) = keyword else {
            return Err(not_saved());
        };
        let text = string_attribute(keyword.get("keyword"));
        if ruby_strip(&text).is_empty() || text.chars().count() > KEYWORD_MAX {
            return Err(not_saved());
        }
        // `whole_word: nil` is written as NULL, which the column refuses.
        let whole_word = boolean_attribute(keyword.get("whole_word")).ok_or_else(|| {
            AppError::Unprocessable(
                "PG::NotNullViolation: null value in column \"whole_word\"".into(),
            )
        })?;
        keywords.push((text, whole_word));
    }
    let mut tx = state.db.begin().await?;
    for (keyword, whole_word) in keywords {
        sqlx::query!(
            "INSERT INTO custom_filter_keywords (custom_filter_id, keyword, whole_word, created_at, updated_at)
             VALUES ($1, $2, $3, now(), now())",
            filter_id,
            keyword,
            whole_word,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    // `filter.action = @data['action'].to_sym`. The enum validates rather
    // than raising, so a bad action is only refused when the filter is saved.
    let action = match data.get("action") {
        Some(Value::String(action)) => match action.as_str() {
            "warn" => Some(crate::db::models::filter_action::WARN),
            "hide" => Some(crate::db::models::filter_action::HIDE),
            "blur" => Some(crate::db::models::filter_action::BLUR),
            _ => None,
        },
        _ => return Err(undefined_method("to_sym")),
    };
    let expires_at = data_str(data, "expires_at").and_then(parse_time);

    // `Status.where(uri: @data['statuses'])`, which finds a local post by
    // its stored `uri` as well as any other.
    let mut status_ids: Vec<i64> = Vec::new();
    for uri in data
        .get("statuses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if let Some(id) = crate::federation::local_uri::status(state, uri).await {
            if !status_ids.contains(&id) {
                status_ids.push(id);
            }
        }
    }
    if !status_ids.is_empty() {
        // `filter.statuses = …`, saved at once; `CustomFilterStatus`
        // refuses a post the account may not see.
        for &status_id in &status_ids {
            if crate::api::mastodon::resolve_url::authorized_status(
                state,
                status_id,
                Some(account_id),
            )
            .await?
            .is_none()
            {
                return Err(AppError::Unprocessable(
                    "Failed to replace statuses because one or more of the new records could not be saved."
                        .into(),
                ));
            }
        }
        let mut tx = state.db.begin().await?;
        for status_id in status_ids {
            sqlx::query!(
                "INSERT INTO custom_filter_statuses (custom_filter_id, status_id, created_at, updated_at)
                 VALUES ($1, $2, now(), now())",
                filter_id,
                status_id,
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
    }

    // `filter.save!`.
    let action = action.ok_or_else(|| {
        AppError::Unprocessable("Validation failed: Action is not included in the list".into())
    })?;
    sqlx::query!(
        "UPDATE custom_filters SET action = $2, expires_at = $3, updated_at = now() WHERE id = $1",
        filter_id,
        action,
        expires_at,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

// ── Vacuum::ImportsVacuum ────────────────────────────────────────────────

/// `Vacuum::ImportsVacuum`: unconfirmed imports go once their ten minutes
/// are up, and every import a week after it was made.
pub async fn vacuum(state: &AppState) -> AppResult<u64> {
    let unconfirmed = sqlx::query!(
        "DELETE FROM bulk_imports
         WHERE state = $1 AND created_at <= now() - make_interval(mins => $2)",
        import_state::UNCONFIRMED,
        CONFIRM_PERIOD_MINUTES,
    )
    .execute(&state.db)
    .await?
    .rows_affected();
    let old = sqlx::query!(
        "DELETE FROM bulk_imports WHERE created_at <= now() - make_interval(days => $1)",
        ARCHIVE_PERIOD_DAYS,
    )
    .execute(&state.db)
    .await?
    .rows_affected();
    Ok(unconfirmed + old)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_are_converted_as_upstream() {
        assert_eq!(
            convert(Some("Account address"), "  @alice@example.com "),
            Value::String("alice@example.com".into())
        );
        assert_eq!(convert(Some("Show boosts"), "FALSE"), Value::Bool(false));
        assert_eq!(convert(Some("Show boosts"), "yes"), Value::Bool(true));
        assert_eq!(convert(Some("Hide notifications"), ""), Value::Null);
        assert_eq!(
            convert(Some("Languages"), "en, fr,"),
            serde_json::json!(["en", "fr"])
        );
        assert_eq!(convert(Some("Languages"), ","), Value::Null);
        assert_eq!(
            convert(Some("#domain"), " Example.COM "),
            Value::String("example.com".into())
        );
    }

    #[test]
    fn json_is_compacted_in_order() {
        assert_eq!(
            compact_json(r#"{"title": "a, b: c", "context": ["home", "public"]}"#),
            r#"{"title":"a, b: c","context":["home","public"]}"#
        );
        assert_eq!(compact_json(r#"{"a": "q\" x"}"#), r#"{"a":"q\" x"}"#);
    }

    #[test]
    fn times_are_read_as_rails_reads_them() {
        assert!(parse_time("2026-10-02 12:00:00 UTC").is_some());
        assert!(parse_time("2026-10-02T12:00:00.000Z").is_some());
        assert!(parse_time("soon").is_none());
    }
}
