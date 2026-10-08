//! Inbound quotes and `FeatureRequest`s: the quote a remote post makes, and
//! how it is verified (`ActivityPub::VerifyQuoteService`); the consent
//! handshakes by which a remote actor asks to quote a local status
//! (`QuoteRequest`) or feature a local account (`FeatureRequest`), our answers,
//! and theirs to ours; and a quote's stamp taken back (`Delete` of a
//! `QuoteAuthorization`).

use serde_json::{json, Value};

use crate::db::models::{quote_state, vis};
use crate::quotes::Quote;
use crate::{error::AppResult, state::AppState};

use super::{fetch_remote_status_prefetched, resolve_or_fetch_remote_account, same_host};
use ojak_vocab::json_ld_helper::{first_of_value, value_or_id};

/// `value_or_id(value)`, or `""` when there is none.
fn uri_of(value: Option<&Value>) -> &str {
    value.and_then(value_or_id).unwrap_or_default()
}

/// `first_of_value(value)`, for a value that may be absent.
fn first(value: Option<&Value>) -> Option<&Value> {
    value.and_then(first_of_value)
}

/// Whether a JSON value is `present?`: not null, nor an empty string, array
/// or object.
fn present(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Bool(b)) => *b,
        Some(_) => true,
    }
}

/// What `ActivityPub::Parser::StatusParser` reads of a post's quote.
pub(super) struct QuoteFields<'a> {
    /// `quote_uri`: the first of `quote`, `_misskey_quote`, `quoteUrl` and
    /// `quoteUri` that names something.
    pub uri: Option<&'a str>,
    /// `deleted_quote?`: `quote` is a `Tombstone`.
    pub deleted: bool,
    /// `legacy_quote?`: no FEP-044f `quote` at all.
    pub legacy: bool,
    /// `quoted_object`: the inlined quote, if `quote` inlines one.
    pub object: Option<&'a Value>,
    /// `quote_approval_uri`, less one we could not fetch or that is ours.
    pub approval_uri: Option<&'a str>,
}

const QUOTE_KEYS: [&str; 4] = ["quote", "_misskey_quote", "quoteUrl", "quoteUri"];

impl<'a> QuoteFields<'a> {
    /// `None` unless the post quotes (`quote?`).
    pub fn parse(object: &'a Value, local_domain: &str) -> Option<Self> {
        if !QUOTE_KEYS.iter().any(|k| present(object.get(*k))) {
            return None;
        }
        let uri = QUOTE_KEYS
            .iter()
            .find_map(|k| first(object.get(*k)).and_then(value_or_id));
        let quote = object.get("quote");
        let deleted =
            quote.and_then(|q| q.get("type")).and_then(Value::as_str) == Some("Tombstone");
        // `unsupported_uri_scheme?` or `TagManager#local_url?`.
        let approval_uri = first(object.get("quoteAuthorization"))
            .and_then(value_or_id)
            .filter(|u| {
                let Ok(url) = url::Url::parse(u) else {
                    return false;
                };
                matches!(url.scheme(), "http" | "https")
                    && !url
                        .host_str()
                        .is_some_and(|h| h.eq_ignore_ascii_case(local_domain))
            });
        Some(Self {
            uri,
            deleted,
            legacy: !object.as_object().is_some_and(|o| o.contains_key("quote")),
            object: first(quote).filter(|q| q.is_object()),
            approval_uri,
        })
    }

    /// `safe_prefetched_embed`: the inlined quote, given the activity's
    /// context, when it is the quoting account's own post on its own server.
    fn embedded(&self, account_uri: &str, context: Option<&Value>) -> Option<Value> {
        let object = self.object?;
        let mut object = object.clone();
        if let Some(map) = object.as_object_mut() {
            map.insert("@context".into(), context.cloned().unwrap_or(Value::Null));
        }
        let attributed_to = first(object.get("attributedTo")).and_then(value_or_id);
        let id = object.get("id").and_then(Value::as_str).unwrap_or_default();
        (attributed_to == Some(account_uri) && same_host(account_uri, id)).then_some(object)
    }
}

/// `ActivityPub::Activity::Create#process_quote` and `#fetch_and_verify_quote`,
/// for the remote status `status_id` by `account_id` just stored from
/// `object`: its quote is recorded pending (deleted, for a `Tombstone`) and
/// verified.
pub(super) async fn process_quote(
    state: &AppState,
    status_id: i64,
    account_id: i64,
    object: &Value,
    context: Option<&Value>,
    depth: u8,
) -> AppResult<()> {
    let Some(fields) = QuoteFields::parse(object, &state.instance.domain) else {
        return Ok(());
    };
    let quote_state = if fields.deleted {
        quote_state::DELETED
    } else {
        quote_state::PENDING
    };
    let Some(quote_id) = sqlx::query_scalar!(
        r#"INSERT INTO quotes (id, status_id, account_id, state, legacy, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, now(), now())
           ON CONFLICT (status_id) DO NOTHING
           RETURNING id"#,
        crate::snowflake::next_id(),
        status_id,
        account_id,
        quote_state,
        fields.legacy,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let Some(quote) = crate::quotes::find(&state.db, quote_id).await? else {
        return Ok(());
    };
    let account_uri = crate::quotes::account_uri(state, account_id)
        .await?
        .unwrap_or_default();
    let embedded = fields.embedded(&account_uri, context);
    let quote_id = quote.id;
    if verify(
        state,
        quote,
        fields.approval_uri,
        fields.uri,
        embedded,
        depth,
    )
    .await?
    {
        refetch_and_verify_later(state, quote_id, fields.uri, fields.approval_uri).await;
    }
    Ok(())
}

/// `ActivityPub::ProcessStatusUpdateService`'s quote handling, for the remote
/// status `status_id` just updated from `object`: `update_quote!` for an
/// edit (`explicit`), `update_quote_approval!` otherwise. Says whether the
/// quote changed state or was replaced, which is when an implicit update is
/// broadcast.
pub(super) async fn update_quote(
    state: &AppState,
    status_id: i64,
    account_id: i64,
    object: &Value,
    context: Option<&Value>,
    explicit: bool,
) -> AppResult<bool> {
    let fields = QuoteFields::parse(object, &state.instance.domain);
    let existing = crate::quotes::find_by_status(&state.db, status_id).await?;
    let before = existing.as_ref().map(|q| (q.id, q.state));
    let account_uri = crate::quotes::account_uri(state, account_id)
        .await?
        .unwrap_or_default();

    let quoted_uri = match existing.as_ref().and_then(|q| q.quoted_status_id) {
        Some(id) => crate::quotes::status_uri(state, id).await?,
        None => None,
    };

    if !explicit {
        // `update_quote_approval!`
        let (Some(fields), Some(quote)) = (fields, existing) else {
            return Ok(false);
        };
        let Some(uri) = fields.uri else {
            return Ok(false);
        };
        if let Some(quoted_uri) = &quoted_uri {
            let quoted_local = quoted_account_local(state, &quote).await?;
            if quoted_uri != uri || quoted_local {
                return Ok(false);
            }
        }
        let quote =
            reset_if_stamp_changed(state, quote, fields.approval_uri, fields.legacy).await?;
        let embedded = fields.embedded(&account_uri, context);
        let quote_id = quote.id;
        if verify(state, quote, fields.approval_uri, Some(uri), embedded, 0).await? {
            refetch_and_verify_later(state, quote_id, Some(uri), fields.approval_uri).await;
        }
        return state_moved(state, status_id, before).await;
    }

    // `update_quote!`
    let Some(fields) = fields else {
        if let Some(quote) = existing {
            crate::quotes::destroy(&state.db, &quote).await?;
            return Ok(true);
        }
        return Ok(false);
    };
    let quote = match existing {
        Some(quote) if quoted_uri.is_some() && quoted_uri.as_deref() != fields.uri => {
            // The quoted post changed: the old quote goes, revoked first if
            // it was an accepted quote of ours.
            if quote.accepted() && quoted_account_local(state, &quote).await? {
                crate::quotes::revoke(state, &quote, true)
                    .await
                    .map_err(crate::error::AppError::Internal)?;
            }
            crate::quotes::destroy(&state.db, &quote).await?;
            insert_pending(state, status_id, account_id, &fields).await?
        }
        Some(quote) => {
            Some(reset_if_stamp_changed(state, quote, fields.approval_uri, fields.legacy).await?)
        }
        None => insert_pending(state, status_id, account_id, &fields).await?,
    };
    if let Some(quote) = quote {
        let embedded = fields.embedded(&account_uri, context);
        let quote_id = quote.id;
        if verify(state, quote, fields.approval_uri, fields.uri, embedded, 0).await? {
            refetch_and_verify_later(state, quote_id, fields.uri, fields.approval_uri).await;
        }
    }
    state_moved(state, status_id, before).await
}

async fn insert_pending(
    state: &AppState,
    status_id: i64,
    account_id: i64,
    fields: &QuoteFields<'_>,
) -> AppResult<Option<Quote>> {
    let quote_state = if fields.deleted {
        quote_state::DELETED
    } else {
        quote_state::PENDING
    };
    let id = sqlx::query_scalar!(
        r#"INSERT INTO quotes (id, status_id, account_id, state, legacy, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, now(), now())
           ON CONFLICT (status_id) DO NOTHING
           RETURNING id"#,
        crate::snowflake::next_id(),
        status_id,
        account_id,
        quote_state,
        fields.legacy,
    )
    .fetch_optional(&state.db)
    .await?;
    Ok(match id {
        Some(id) => crate::quotes::find(&state.db, id).await?,
        None => None,
    })
}

/// `quote.update(approval_uri: nil, state: :pending, legacy:) if
/// quote.approval_uri.present? && quote.approval_uri != approval_uri`, and
/// the counter the update moves.
async fn reset_if_stamp_changed(
    state: &AppState,
    quote: Quote,
    approval_uri: Option<&str>,
    legacy: bool,
) -> AppResult<Quote> {
    let Some(stored) = quote.approval_uri.as_deref() else {
        return Ok(quote);
    };
    if Some(stored) == approval_uri {
        return Ok(quote);
    }
    sqlx::query!(
        "UPDATE quotes SET approval_uri = NULL, state = 0, legacy = $2, updated_at = now() \
         WHERE id = $1",
        quote.id,
        legacy,
    )
    .execute(&state.db)
    .await?;
    crate::quotes::state_changed(
        &state.db,
        quote.quoted_status_id,
        legacy,
        quote.state,
        quote_state::PENDING,
    )
    .await;
    Ok(crate::quotes::find(&state.db, quote.id)
        .await?
        .unwrap_or(quote))
}

/// Whether the quote of `status_id` is another than `before`, or in another
/// state.
async fn state_moved(
    state: &AppState,
    status_id: i64,
    before: Option<(i64, i32)>,
) -> AppResult<bool> {
    let after = crate::quotes::find_by_status(&state.db, status_id)
        .await?
        .map(|q| (q.id, q.state));
    Ok(after != before)
}

async fn quoted_account_local(state: &AppState, quote: &Quote) -> AppResult<bool> {
    let Some(id) = quote.quoted_account_id else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar!(
        r#"SELECT (domain IS NULL) AS "local!" FROM accounts WHERE id = $1"#,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false))
}

/// `ActivityPub::VerifyQuoteService#call`: fetch the quoted post if needed,
/// and accept the quote when its author's stamp says it may be, reject it
/// when the stamp is gone. A local quoted post waits for its `QuoteRequest`.
///
/// Says whether the stamp could not be fetched for now, which Mastodon
/// raises for `RefetchAndVerifyQuoteWorker` to try again.
async fn verify(
    state: &AppState,
    mut quote: Quote,
    approval_uri: Option<&str>,
    quoted_uri: Option<&str>,
    embedded_quote: Option<Value>,
    depth: u8,
) -> AppResult<bool> {
    let approval_uri = approval_uri
        .map(str::to_owned)
        .or_else(|| quote.approval_uri.clone());

    // `fetch_quoted_post_if_needed!`
    if quote.quoted_status_id.is_none() {
        if let Some(uri) = quoted_uri {
            // A quoted post whose server does not answer is verified later
            // (`fetch_and_verify_quote` rescues `HTTP_CONNECTION_ERRORS`
            // into a `RefetchAndVerifyQuoteWorker`).
            let found = match find_or_fetch_status(state, uri, embedded_quote, depth).await {
                Ok(found) => found,
                Err(error) if super::fetch::unanswered(&error) => return Ok(true),
                Err(error) => return Err(error),
            };
            if let Some(found) = found {
                quote = set_quoted_status(state, quote, found).await?;
            }
        }
    }
    if quoted_account_local(state, &quote).await? {
        return Ok(false);
    }
    // `fast_track_approval!`: always allow someone to quote themselves. (It
    // returns false whatever it does, so verifying goes on.)
    fast_track(state, &quote).await?;
    let Some(approval_uri) = approval_uri.filter(|u| !u.is_empty()) else {
        return Ok(false);
    };

    let json = match crate::federation::fetch::signed_get_json(state, &approval_uri).await {
        Ok(json) if json.get("id").and_then(Value::as_str) == Some(approval_uri.as_str()) => json,
        Ok(_) => {
            crate::quotes::reject(&state.db, &quote).await?;
            return Ok(false);
        }
        Err(error) if temporary(&error) => {
            tracing::debug!(%approval_uri, %error, "quote stamp not fetched for now");
            return Ok(true);
        }
        Err(_) => {
            // `return quote.reject! if @json.nil?`
            crate::quotes::reject(&state.db, &quote).await?;
            return Ok(false);
        }
    };

    let attributed_to = first(json.get("attributedTo")).and_then(value_or_id);
    if !attributed_to.is_some_and(|a| same_host(&approval_uri, a)) {
        return Ok(false);
    }
    // `matching_type?`: `supported_context?` and a `QuoteAuthorization`.
    let supported_context = match json.get("@context") {
        Some(Value::String(c)) => c == ojak_vocab::ACTIVITYSTREAMS_CONTEXT,
        Some(Value::Array(cs)) => cs
            .iter()
            .any(|c| c.as_str() == Some(ojak_vocab::ACTIVITYSTREAMS_CONTEXT)),
        _ => false,
    };
    let is_authorization = match json.get("type") {
        Some(Value::String(t)) => t == "QuoteAuthorization",
        Some(Value::Array(ts)) => ts.iter().any(|t| t.as_str() == Some("QuoteAuthorization")),
        _ => false,
    };
    // `matching_quote_uri?`
    let quoting_uri = crate::quotes::status_uri(state, quote.status_id).await?;
    if !supported_context
        || !is_authorization
        || quoting_uri.as_deref() != first(json.get("interactingObject")).and_then(value_or_id)
    {
        return Ok(false);
    }

    // `import_quoted_post_if_needed!`: an inlined `interactionTarget` from
    // the stamp's own server.
    if quote.quoted_status_id.is_none() {
        if let (Some(uri), Some(target)) = (quoted_uri, json.get("interactionTarget")) {
            if target.is_object()
                && target.get("id").and_then(Value::as_str) == Some(uri)
                && same_host(&approval_uri, uri)
            {
                let mut target = target.clone();
                target["@context"] = json.get("@context").cloned().unwrap_or(Value::Null);
                if let Some(found) = fetch_remote_status_prefetched(state, uri, target).await? {
                    quote = set_quoted_status(state, quote, found).await?;
                    fast_track(state, &quote).await?;
                }
            }
        }
    }
    let Some(quoted_status_id) = quote.quoted_status_id else {
        return Ok(false);
    };

    // `matching_quoted_post?` and `matching_quoted_author?`
    let target = first(json.get("interactionTarget")).and_then(value_or_id);
    let quoted_status_uri = crate::quotes::status_uri(state, quoted_status_id).await?;
    let quoted_account_uri = match quote.quoted_account_id {
        Some(id) => crate::quotes::account_uri(state, id).await?,
        None => None,
    };
    if quoted_status_uri.as_deref() != target || quoted_account_uri.as_deref() != attributed_to {
        return Ok(false);
    }
    crate::quotes::accept(&state.db, &quote, Some(&approval_uri)).await?;
    Ok(false)
}

/// `PROCESSING_DELAY`: how long after a failed verification the first retry
/// waits, at random.
const PROCESSING_DELAY: std::ops::RangeInclusive<u64> = 30..=600;

/// `ActivityPub::RefetchAndVerifyQuoteWorker.perform_in(rand(PROCESSING_DELAY),
/// quote.id, quote_uri, { 'approval_uri' => approval_uri })`.
async fn refetch_and_verify_later(
    state: &AppState,
    quote_id: i64,
    quoted_uri: Option<&str>,
    approval_uri: Option<&str>,
) {
    let delay = std::time::Duration::from_secs(rand::random_range(PROCESSING_DELAY));
    crate::jobs::push_in(
        state,
        delay,
        RefetchAndVerifyQuoteWorker {
            quote_id,
            quoted_uri: quoted_uri.map(str::to_owned),
            approval_uri: approval_uri.map(str::to_owned),
        },
    )
    .await;
}

/// `ActivityPub::RefetchAndVerifyQuoteWorker`: verify the quote again, retried
/// while its stamp cannot be fetched for now, and refresh the quoting post in
/// local timelines if its state moved.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct RefetchAndVerifyQuoteWorker {
    pub quote_id: i64,
    #[serde(default)]
    pub quoted_uri: Option<String>,
    #[serde(default)]
    pub approval_uri: Option<String>,
}

impl crate::jobs::Job for RefetchAndVerifyQuoteWorker {
    const KIND: &'static str = "ActivityPub::RefetchAndVerifyQuoteWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Pull)
        .retry(5);

    fn retry_in(count: u32) -> Option<std::time::Duration> {
        crate::jobs::exponential_backoff(count)
    }

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        // `Quote.find(quote_id)`, else nothing to do.
        let Some(quote) = crate::quotes::find(&state.db, self.quote_id).await? else {
            return Ok(());
        };
        let before = quote.state;
        let status_id = quote.status_id;
        let retry = verify(
            state,
            quote,
            self.approval_uri.as_deref(),
            self.quoted_uri.as_deref(),
            None,
            0,
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        let after = crate::quotes::find(&state.db, self.quote_id)
            .await?
            .map(|q| q.state);
        if after.is_some_and(|after| after != before) {
            crate::quotes::distribute_update(state, status_id, false).await;
        }
        anyhow::ensure!(!retry, "the quote's stamp could not be fetched for now");
        Ok(())
    }
}

/// A fetch that failed for now, which Mastodon retries (`raise_on_error:
/// :temporary`): no answer, or one that may change.
fn temporary(error: &anyhow::Error) -> bool {
    match error.downcast_ref::<ojak::fetch::FetchError>() {
        Some(ojak::fetch::FetchError::Status(code)) => {
            !crate::federation::delivery::unsalvageable(*code)
        }
        Some(ojak::fetch::FetchError::Request(_) | ojak::fetch::FetchError::Signing(_)) => true,
        Some(_) => false,
        None => true,
    }
}

async fn fast_track(state: &AppState, quote: &Quote) -> AppResult<()> {
    if quote.quoted_status_id.is_some() && Some(quote.account_id) == quote.quoted_account_id {
        crate::quotes::accept(&state.db, quote, None).await?;
    }
    Ok(())
}

/// A status by `uri`, fetched (bounded) when it is not here
/// (`uri_to_resource`, then `FetchRemoteStatusService`).
async fn find_or_fetch_status(
    state: &AppState,
    uri: &str,
    prefetched: Option<Value>,
    depth: u8,
) -> AppResult<Option<i64>> {
    if let Some(id) = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE uri = $1 AND deleted_at IS NULL",
        uri,
    )
    .fetch_optional(&state.db)
    .await?
    {
        return Ok(Some(id));
    }
    // `raise Mastodon::RecursionLimitExceededError if @depth >
    // MAX_SYNCHRONOUS_DEPTH && status.nil?`
    if depth >= super::fetch::MAX_FETCH_DEPTH {
        return Ok(None);
    }
    match prefetched {
        Some(body) => {
            super::fetch::fetch_remote_status_at_depth(state, uri, Some(body), depth + 1).await
        }
        None => super::fetch::fetch_remote_status_at_depth(state, uri, None, depth + 1).await,
    }
}

/// `@quote.update(quoted_status: status) if status.present? &&
/// !status.reblog?`, which `Quote#validate_visibility` refuses for someone
/// else's post that is not public or unlisted.
async fn set_quoted_status(state: &AppState, quote: Quote, status_id: i64) -> AppResult<Quote> {
    let Some(quoted) = sqlx::query!(
        "SELECT id, account_id, visibility, reblog_of_id FROM statuses WHERE id = $1",
        status_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(quote);
    };
    if quoted.reblog_of_id.is_some()
        || (quoted.account_id != quote.account_id
            && !matches!(quoted.visibility, vis::PUBLIC | vis::UNLISTED))
    {
        return Ok(quote);
    }
    sqlx::query!(
        r#"UPDATE quotes SET quoted_status_id = $2, quoted_account_id = $3, updated_at = now()
           WHERE id = $1"#,
        quote.id,
        quoted.id,
        quoted.account_id,
    )
    .execute(&state.db)
    .await?;
    Ok(crate::quotes::find(&state.db, quote.id)
        .await?
        .unwrap_or(quote))
}

/// The `QuoteRequest` a quote is asked for with, as
/// `ActivityPub::QuoteRequestSerializer` writes it without inlining: who asks
/// (`actor`), to quote what (`object`), in which post (`instrument`).
fn quote_request_object(id: &str, actor: &str, object: &str, instrument: &str) -> Value {
    json!({
        "id": id,
        "type": "QuoteRequest",
        "actor": actor,
        "object": object,
        "instrument": instrument,
    })
}

fn quote_request_context() -> Value {
    json!([
        ojak_vocab::ACTIVITYSTREAMS_CONTEXT,
        {"QuoteRequest": "https://w3id.org/fep/044f#QuoteRequest"},
    ])
}

/// `ActivityPub::Activity::QuoteRequest#perform`: a remote account asks to
/// quote a local post. It is accepted when `StatusPolicy#quote?` lets it
/// quote, and rejected otherwise.
pub(super) async fn handle_quote_request(
    state: &AppState,
    _instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    let req_id = activity.get("id").and_then(Value::as_str).unwrap_or("");
    let actor_uri = uri_of(activity.get("actor"));
    let object_uri = uri_of(activity.get("object"));
    let instrument_uri = uri_of(activity.get("instrument"));
    if req_id.is_empty() || object_uri.is_empty() || actor_uri.is_empty() {
        return Ok(());
    }
    // `return if non_matching_uri_hosts?(@account.uri, @json['id'])`
    if !same_host(actor_uri, req_id) {
        return Ok(());
    }

    // `return if quoted_status.nil? || !quoted_status.account.local? ||
    // !quoted_status.distributable? || quoted_status.reblog?`
    let Some(quoted) = sqlx::query!(
        r#"SELECT s.id, s.account_id, s.quote_approval_policy, s.visibility, s.reblog_of_id,
                  a.username, a.id_scheme,
                  (a.suspended_at IS NOT NULL OR a.requested_deletion_at IS NOT NULL) AS "unavailable!"
           FROM statuses s JOIN accounts a ON a.id = s.account_id
           WHERE s.uri = $1 AND s.deleted_at IS NULL AND a.domain IS NULL"#,
        object_uri,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    if !matches!(quoted.visibility, vis::PUBLIC | vis::UNLISTED) || quoted.reblog_of_id.is_some() {
        return Ok(());
    }

    let Ok(quoter_id) = resolve_or_fetch_remote_account(state, actor_uri).await else {
        return Ok(());
    };
    let quoter = sqlx::query!(
        "SELECT uri, domain, inbox_url FROM accounts WHERE id = $1",
        quoter_id,
    )
    .fetch_one(&state.db)
    .await?;
    let domain = &state.instance.domain;
    let actor_url = crate::federation::tag::account_uri(
        domain,
        quoted.account_id,
        quoted.id_scheme,
        &quoted.username,
    );
    let key_id = format!("{actor_url}#main-key");
    let quoter_uri = quoter.uri.clone().unwrap_or_else(|| actor_uri.to_string());

    // `StatusPolicy#quote?`: `show?` (the author is there, and blocks
    // neither the quoter nor its server), `!blocking_author?`, and a policy
    // that does not deny it.
    let relation = sqlx::query!(
        r#"SELECT
             EXISTS (SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2) AS "follows_author!",
             EXISTS (SELECT 1 FROM follows WHERE account_id = $2 AND target_account_id = $1) AS "followed_by_author!",
             EXISTS (SELECT 1 FROM blocks
                     WHERE (account_id = $1 AND target_account_id = $2)
                        OR (account_id = $2 AND target_account_id = $1)) AS "blocked!",
             EXISTS (SELECT 1 FROM account_domain_blocks
                     WHERE account_id = $2 AND domain = $3) AS "domain_blocked!""#,
        quoter_id,
        quoted.account_id,
        quoter.domain.as_deref().unwrap_or_default(),
    )
    .fetch_one(&state.db)
    .await?;
    use crate::db::models::quote_policy;
    let allowed = !quoted.unavailable
        && !relation.blocked
        && !relation.domain_blocked
        && quote_policy::for_account(
            quoted.quote_approval_policy,
            quoter_id == quoted.account_id,
            relation.follows_author,
            relation.followed_by_author,
        ) != quote_policy::ForAccount::Denied;

    if !crate::federation::keypair::has_signing_key(state, quoted.account_id)
        .await
        .unwrap_or(false)
        || quoter.inbox_url.is_empty()
    {
        return Ok(());
    }

    if !allowed {
        // `reject_quote_request!`, about a quote never saved:
        // `RejectQuoteRequestSerializer` names the Reject after that unsaved
        // quote's id, which is nil, so every Reject an account sends has the
        // one id. ojak tells such activities apart by what they say.
        let mut reject = json!({
            "@context": quote_request_context(),
            "id": format!("{actor_url}#rejects/quote_requests/"),
            "type": "Reject",
            "actor": actor_url,
            "object": quote_request_object(req_id, &quoter_uri, object_uri, instrument_uri),
        });
        if instrument_uri.is_empty() {
            reject["object"]["instrument"] = Value::Null;
        }
        if let Err(e) = crate::federation::delivery::deliver_to_inboxes(
            state,
            reject,
            vec![quoter.inbox_url],
            key_id,
        )
        .await
        {
            tracing::warn!(error = %e, "failed to enqueue quote Reject");
        }
        return Ok(());
    }

    // `accept_quote_request!`: the quoting post, known, inlined, or fetched.
    let mut quoting_id = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE uri = $1 AND deleted_at IS NULL",
        instrument_uri,
    )
    .fetch_optional(&state.db)
    .await?;
    if quoting_id.is_none() {
        // `import_instrument`
        if let Some(instrument) = activity.get("instrument").filter(|i| i.is_object()) {
            let id = instrument.get("id").and_then(Value::as_str).unwrap_or("");
            if same_host(actor_uri, id) {
                let mut body = instrument.clone();
                body["@context"] = activity.get("@context").cloned().unwrap_or(Value::Null);
                quoting_id = fetch_remote_status_prefetched(state, id, body).await?;
            }
        }
    }
    // `FetchRemoteStatusService.new.call(instrument_uri, on_behalf_of:
    // quoted_status.account)`, whose unanswered request fails the activity,
    // to be retried.
    if quoting_id.is_none() && !instrument_uri.is_empty() {
        quoting_id = super::fetch_remote_status_with(
            state,
            instrument_uri,
            super::FetchOptions {
                on_behalf_of: Some(quoted.account_id),
                ..Default::default()
            },
        )
        .await?
        .map(|(id, _)| id);
    }
    let Some(quoting_id) = quoting_id else {
        return Ok(());
    };
    // Sanity check: `status.quote.quoted_status == quoted_status &&
    // status.account == @account`.
    let quoting_account =
        sqlx::query_scalar!("SELECT account_id FROM statuses WHERE id = $1", quoting_id)
            .fetch_one(&state.db)
            .await?;
    let Some(quote) = crate::quotes::find_by_status(&state.db, quoting_id).await? else {
        return Ok(());
    };
    if quote.quoted_status_id != Some(quoted.id) || quoting_account != quoter_id {
        return Ok(());
    }

    crate::quotes::ensure_quoted_access(&state.db, &quote).await;
    if let Err(error) = sqlx::query!(
        "UPDATE quotes SET activity_uri = $2, updated_at = now() WHERE id = $1",
        quote.id,
        req_id,
    )
    .execute(&state.db)
    .await
    {
        tracing::debug!(quote_id = quote.id, %error, "could not keep the QuoteRequest's id");
    }
    crate::quotes::accept(&state.db, &quote, None).await?;
    let Some(quote) = crate::quotes::find(&state.db, quote.id).await? else {
        return Ok(());
    };

    // `ActivityPub::AcceptQuoteRequestSerializer`, to the quoter's own inbox.
    let result = crate::quotes::approval_uri_for(state, &quote, true).await?;
    let instrument = crate::quotes::status_uri(state, quote.status_id)
        .await?
        .unwrap_or_default();
    let accept = json!({
        "@context": quote_request_context(),
        "id": format!("{actor_url}#accepts/quote_requests/{}", quote.id),
        "type": "Accept",
        "actor": actor_url,
        "object": quote_request_object(
            quote.activity_uri.as_deref().unwrap_or(req_id),
            &quoter_uri,
            object_uri,
            &instrument,
        ),
        "result": result,
    });
    if let Err(e) = crate::federation::delivery::deliver_to_inboxes(
        state,
        accept,
        vec![quoter.inbox_url],
        key_id,
    )
    .await
    {
        tracing::warn!(error = %e, "failed to enqueue quote Accept");
    }

    // `LocalNotificationWorker`, then `DistributionWorker` with
    // `skip_notifications` so local followers see the approval.
    crate::quotes::notify(state, &quote).await;
    crate::quotes::distribute_update(state, quote.status_id, true).await;
    Ok(())
}

/// `ActivityPub::Activity::Accept#accept_quote!` and
/// `ActivityPub::Activity::Reject#reject_quote!`: the quoted author answers
/// a `QuoteRequest` of ours, found by its id (`quote_request_from_object`).
/// Says whether the object was one.
pub(super) async fn handle_quote_answer(
    state: &AppState,
    activity: &Value,
    accept: bool,
) -> AppResult<bool> {
    let actor_uri = uri_of(activity.get("actor"));
    let object_uri = uri_of(activity.get("object"));
    if object_uri.is_empty() {
        return Ok(false);
    }
    let Some(quote_id) = sqlx::query_scalar!(
        r#"SELECT q.id FROM quotes q JOIN accounts a ON a.id = q.quoted_account_id
           WHERE q.activity_uri = $1 AND a.uri = $2"#,
        object_uri,
        actor_uri,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(false);
    };
    let Some(quote) = crate::quotes::find(&state.db, quote_id).await? else {
        return Ok(false);
    };
    // `quote.status.local?` (a deleted status's quote went with it).
    let status_local = sqlx::query_scalar!(
        r#"SELECT local AS "local?" FROM statuses s JOIN accounts a ON a.id = s.account_id
           WHERE s.id = $1 AND s.deleted_at IS NULL AND a.domain IS NULL"#,
        quote.status_id,
    )
    .fetch_optional(&state.db)
    .await?
    .is_some();
    if !status_local {
        return Ok(true);
    }

    if !accept {
        // TODO upstream: "broadcast an update?" It does not.
        crate::quotes::reject(&state.db, &quote).await?;
        return Ok(true);
    }

    let approval_uri = first(activity.get("result")).and_then(value_or_id);
    let Some(approval_uri) = approval_uri.filter(|u| {
        url::Url::parse(u).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
            && same_host(u, actor_uri)
    }) else {
        return Ok(true);
    };
    if quote.state != quote_state::PENDING {
        return Ok(true);
    }
    crate::quotes::accept(&state.db, &quote, Some(approval_uri)).await?;

    // `DistributionWorker` with `update`, and
    // `ActivityPub::StatusUpdateDistributionWorker`: the quote is shown
    // approved here, and federated with its stamp.
    crate::quotes::distribute_update(state, quote.status_id, false).await;
    if let (Ok(Some(quoting)), Ok(Some(author))) = (
        sqlx::query_as!(
            crate::db::models::Status,
            "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
            quote.status_id,
        )
        .fetch_optional(&state.db)
        .await,
        sqlx::query_as!(
            crate::db::models::Account,
            "SELECT * FROM accounts WHERE id = $1",
            quote.account_id,
        )
        .fetch_optional(&state.db)
        .await,
    ) {
        if let Err(e) = crate::api::mastodon::statuses::federate_status_update(
            state, quoting.id, &author, &quoting,
        )
        .await
        {
            tracing::warn!(error = %e, "failed to federate quote acceptance Update");
        }
    }
    Ok(true)
}

/// `ActivityPub::Activity::Delete#revoke_quote`: the remote author of a
/// quoted post takes back the stamp `approval_uri` they gave. Says whether
/// there was such a quote.
pub(super) async fn revoke_by_stamp(
    state: &AppState,
    activity: &Value,
    actor_uri: &str,
    approval_uri: &str,
) -> AppResult<bool> {
    let Some(quote_id) = sqlx::query_scalar!(
        r#"SELECT q.id FROM quotes q JOIN accounts a ON a.id = q.quoted_account_id
           WHERE q.approval_uri = $1 AND a.uri = $2 AND q.state IN (0, 1)"#,
        approval_uri,
        actor_uri,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(false);
    };
    let Some(quote) = crate::quotes::find(&state.db, quote_id).await? else {
        return Ok(false);
    };
    // `ActivityPub::Forwarder.new(@account, @json, @quote.status).forward!
    // if @quote.status.present?`, whether or not the Delete is signed.
    let quoting_live = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        quote.status_id,
    )
    .fetch_optional(&state.db)
    .await?
    .is_some();
    if quoting_live {
        if let Some(sender) = quote.quoted_account_id {
            crate::federation::forwarder::forward(state, sender, activity, quote.status_id).await;
        }
    }
    crate::quotes::reject(&state.db, &quote).await?;
    crate::quotes::distribute_update(state, quote.status_id, false).await;
    Ok(true)
}

/// Handle an incoming `FeatureRequest`: a remote collection wants to feature one
/// of our local accounts. We fetch/store the remote collection, record an
/// accepted item, and reply with an `Accept` whose `result` points at a
/// `FeatureAuthorization` we serve. (Rejection policy is intentionally simple:
/// suspended local accounts are skipped.)
pub(super) async fn handle_feature_request(
    state: &AppState,
    instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    let req_id = activity.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let account_uri = activity
        .get("object")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let collection_uri = activity
        .get("instrument")
        .and_then(|v| {
            if v.is_string() {
                v.as_str()
            } else {
                v.get("id").and_then(|i| i.as_str())
            }
        })
        .unwrap_or("");
    if req_id.is_empty() || account_uri.is_empty() || collection_uri.is_empty() {
        return Ok(());
    }

    // The featured account must be local and active.
    let Some(local) = sqlx::query!(
        r#"SELECT id, username, suspended_at, requested_deletion_at, id_scheme
           FROM accounts WHERE uri = $1 AND domain IS NULL"#,
        account_uri,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    if local.suspended_at.is_some() || local.requested_deletion_at.is_some() {
        return Ok(());
    }

    // Fetch the remote FeaturedCollection to learn its owner and name.
    let coll: Value = match crate::federation::fetch::signed_get_json(state, collection_uri).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let owner_uri = coll
        .get("attributedTo")
        .and_then(|v| {
            if v.is_string() {
                v.as_str()
            } else {
                v.get("id").and_then(|i| i.as_str())
            }
        })
        .unwrap_or("");
    if owner_uri.is_empty() {
        return Ok(());
    }
    let Ok(owner_id) = resolve_or_fetch_remote_account(state, owner_uri).await else {
        return Ok(());
    };
    let name = coll
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Featured collection");
    let sensitive = coll
        .get("sensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let discoverable = coll
        .get("discoverable")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    // Upsert the remote collection (local = false).
    let collection_id = sqlx::query_scalar!(
        r#"INSERT INTO collections
             (account_id, name, discoverable, local, sensitive, item_count, uri, created_at, updated_at)
           VALUES ($1, $2, $3, false, $4, 0, $5, now(), now())
           ON CONFLICT (uri) WHERE uri IS NOT NULL
             DO UPDATE SET name = EXCLUDED.name, updated_at = now()
           RETURNING id"#,
        owner_id,
        name,
        discoverable,
        sensitive,
        collection_uri,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(collection_id) = collection_id else {
        return Ok(());
    };

    // Record the accepted item with our authorization URI.
    let item_id = sqlx::query_scalar!(
        r#"INSERT INTO collection_items
             (collection_id, account_id, state, activity_uri, position, created_at, updated_at)
           VALUES ($1, $2, 1, $3,
                   (SELECT COALESCE(MAX(position), 0) + 1 FROM collection_items WHERE collection_id = $1),
                   now(), now())
           ON CONFLICT (account_id, collection_id)
             DO UPDATE SET state = 1, activity_uri = EXCLUDED.activity_uri, updated_at = now()
           RETURNING id"#,
        collection_id,
        local.id,
        req_id,
    )
    .fetch_one(&state.db)
    .await?;

    let domain = &instance.domain;
    // `ap_account_feature_authorization_url`.
    let authorization_uri = format!(
        "https://{domain}/ap/users/{}/feature_authorizations/{item_id}",
        local.id
    );
    sqlx::query!(
        "UPDATE collection_items SET approval_uri = $2 WHERE id = $1",
        item_id,
        authorization_uri,
    )
    .execute(&state.db)
    .await?;

    // `notify_local_user!`.
    crate::push::notify_collection(
        state,
        local.id,
        "added_to_collection",
        ("CollectionItem", item_id),
        owner_id,
    )
    .await;

    // Reply with Accept(result = our FeatureAuthorization) to the collection owner.
    let owner = sqlx::query!(
        "SELECT uri, inbox_url, shared_inbox_url FROM accounts WHERE id = $1",
        owner_id,
    )
    .fetch_one(&state.db)
    .await?;
    if !crate::federation::keypair::has_signing_key(state, local.id)
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }

    let inbox = if !owner.shared_inbox_url.is_empty() {
        owner.shared_inbox_url
    } else {
        owner.inbox_url
    };
    if !inbox.is_empty() {
        let actor_url =
            crate::federation::tag::account_uri(domain, local.id, local.id_scheme, &local.username);
        let accept_id = format!("{actor_url}#accepts/feature_requests/{item_id}");
        let owner_uri = owner.uri.unwrap_or_default();
        if let Ok(accept) = crate::federation::consent::accept(
            &accept_id,
            &actor_url,
            &owner_uri,
            req_id,
            &authorization_uri,
        ) {
            let key_id = format!("{actor_url}#main-key");
            if let Err(e) =
                crate::federation::delivery::deliver_to_inboxes(state, accept, vec![inbox], key_id)
                    .await
            {
                tracing::warn!(error = %e, "failed to enqueue feature Accept");
            }
        }
    }

    Ok(())
}
