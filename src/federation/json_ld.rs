//! Mastodon's `JsonLdHelper`, as far as fetching goes: `fetch_resource`,
//! `fetch_resource_without_id_validation`, `collection_items` and their
//! `raise_on_error`, over ojak's fetcher.
//!
//! The requests are signed as Mastodon signs them: on behalf of a given
//! account — a local follower of the account whose collection is read, when
//! Mastodon asks for one — or else as the instance actor
//! (`Account.representative`).
//!
//! What raises in Mastodon is an `Err` here, for a worker to be retried on:
//! a request that was not answered (a connection error, a refused address, a
//! redirect that could not be followed), and, as `raise_on_error` says, a
//! status other than success. Anything else Mastodon reads as `nil` is
//! `Ok(None)`.

use ojak::fetch::FetchError;
use ojak::sig::SenderKey;
use serde_json::Value;

use crate::state::AppState;

/// `raise_on_error`: which statuses other than success raise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaiseOn {
    /// `:none`: none do.
    None,
    /// `:temporary`: those `response_error_unsalvageable?` does not call
    /// permanent.
    Temporary,
    /// `:all`: all do.
    All,
}

/// `JsonLdHelper#response_error_unsalvageable?`.
pub fn unsalvageable(status: u16) -> bool {
    status == 501 || ((400..500).contains(&status) && ![401, 408, 429].contains(&status))
}

/// Whether Mastodon raises for `error`, fetching with `raise_on_error`.
pub fn raises(error: &FetchError, raise_on_error: RaiseOn) -> bool {
    match error {
        FetchError::Status(status) => match raise_on_error {
            RaiseOn::None => false,
            RaiseOn::Temporary => !unsalvageable(*status),
            RaiseOn::All => true,
        },
        FetchError::Request(_) | FetchError::Signing(_) | FetchError::Redirect(_) => true,
        _ => false,
    }
}

/// The key a request on behalf of `account_id` is signed with: that local
/// account's, or the instance actor's when there is none.
pub async fn signing_key(state: &AppState, on_behalf_of: Option<i64>) -> anyhow::Result<SenderKey> {
    if let Some(account_id) = on_behalf_of {
        let account = sqlx::query!(
            "SELECT id, username, id_scheme FROM accounts WHERE id = $1 AND domain IS NULL",
            account_id,
        )
        .fetch_optional(&state.db)
        .await?;
        if let Some(account) = account {
            let pem = crate::federation::keypair::signing_key(state, account.id)
                .await?
                .private_key;
            return Ok(SenderKey {
                key_id: crate::federation::tag::key_id(
                    &state.instance.domain,
                    account.id,
                    account.id_scheme,
                    &account.username,
                ),
                private_key: std::sync::Arc::new(ojak::sig::PrivateKey::from_pem(&pem)?),
            });
        }
    }
    crate::federation::fetch::instance_key(state).await
}

/// `@account.followers.local.without_suspended.first`: the local follower
/// Mastodon fetches an account's collections on behalf of.
pub async fn local_follower(state: &AppState, account_id: i64) -> Option<i64> {
    sqlx::query_scalar!(
        r#"SELECT a.id FROM follows f JOIN accounts a ON a.id = f.account_id
           WHERE f.target_account_id = $1 AND a.domain IS NULL AND a.suspended_at IS NULL
           ORDER BY a.id LIMIT 1"#,
        account_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
}

/// `JsonLdHelper#fetch_resource_without_id_validation`.
pub async fn fetch_resource_without_id_validation(
    state: &AppState,
    uri: &str,
    on_behalf_of: Option<i64>,
    raise_on_error: RaiseOn,
) -> anyhow::Result<Option<Value>> {
    let key = signing_key(state, on_behalf_of).await?;
    let Ok(url) = url::Url::parse(uri) else {
        anyhow::bail!("invalid URL {uri:?}");
    };
    match state.fetcher.unverified_json(&url, Some(&key)).await {
        Ok(json) => Ok(json),
        Err(error) if raises(&error, raise_on_error) => Err(error.into()),
        Err(error) => {
            tracing::debug!(uri, %error, "could not fetch");
            Ok(None)
        }
    }
}

/// `JsonLdHelper#fetch_resource(uri, true, …)`: the document only if its
/// `id` is `uri`. A portable `ap:` URI is fetched from its gateways, and
/// taken only when its proof holds.
pub async fn fetch_resource(
    state: &AppState,
    uri: &str,
    on_behalf_of: Option<i64>,
    raise_on_error: RaiseOn,
) -> anyhow::Result<Option<Value>> {
    if ojak::portable::ApUri::parse(uri).is_some() {
        return Ok(crate::federation::fetch::signed_get_json(state, uri)
            .await
            .ok());
    }
    let json = fetch_resource_without_id_validation(state, uri, on_behalf_of, raise_on_error)
        .await?
        .filter(is_present);
    Ok(json.filter(|json| json.get("id").and_then(Value::as_str) == Some(uri)))
}

/// `JsonLdHelper`'s accessors for a document as it arrived, which ojak
/// ports.
pub use ojak_vocab::json_ld_helper::{supported_context, value_or_id};

/// `JsonLdHelper#non_matching_uri_hosts?`.
pub fn non_matching_uri_hosts(base: &str, comparison: &str) -> bool {
    !ojak::origin::same_host(base, comparison)
}

/// Rails' `present?` for a JSON value: not `nil`, not `false`, not an empty
/// or whitespace-only string, not an empty hash or array. A number is
/// always present, `0` included.
pub fn is_present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.trim().is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Number(_) => true,
    }
}

/// `JsonLdHelper#collection_items`: the items of a collection, page after
/// page until `max_items` have been gathered or `max_pages` read, with the
/// page count Mastodon reports — which counts one more than was read when
/// the collection runs out, or a page cannot be had, first. `None` when not
/// even the first page can be read.
///
/// The pages are walked by ojak, read as Mastodon reads them
/// ([`ojak::fetch::Walk::mastodon_compatible`]): from the collection as it
/// is given, a page fetched by its IRI only from `reference_uri`'s host,
/// each fetched as `fetch_collection_page` fetches it, with
/// `raise_on_error: :temporary` — so a temporary failure is an `Err`. Every
/// caller Mastodon has gives a reference.
pub async fn collection_items(
    state: &AppState,
    collection_or_uri: &Value,
    max_pages: Option<usize>,
    max_items: Option<usize>,
    reference_uri: &str,
    on_behalf_of: Option<i64>,
) -> anyhow::Result<Option<(Vec<Value>, usize)>> {
    let key = signing_key(state, on_behalf_of).await?;
    let limits = ojak::fetch::WalkLimits {
        // The collection may be fetched as well as each page read.
        pages: max_pages.map_or(usize::MAX, |pages| pages.saturating_add(1)),
        items: usize::MAX,
    };
    let mut walk = state
        .fetcher
        .walk_embedded(collection_or_uri, reference_uri, Some(&key), limits)
        .mastodon_compatible();
    let mut items = Vec::new();
    let mut n_pages = 0;
    loop {
        match walk.next_page().await {
            Ok(Some(page)) => {
                n_pages += 1;
                items.extend(page);
                if max_items.is_some_and(|max| items.len() >= max)
                    || max_pages.is_some_and(|max| n_pages >= max)
                {
                    return Ok(Some((items, n_pages)));
                }
            }
            Err(error) if raises(&error, RaiseOn::Temporary) => return Err(error.into()),
            Ok(None) | Err(_) if n_pages == 0 => return Ok(None),
            Ok(None) | Err(_) => return Ok(Some((items, n_pages + 1))),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::is_present;

    /// `present?` as ActiveSupport has it: `false` is blank, as is a
    /// whitespace-only string; `0` and `true` are present.
    #[test]
    fn is_present_is_rails_present() {
        for blank in [
            json!(null),
            json!(false),
            json!(""),
            json!(" \n"),
            json!([]),
            json!({}),
        ] {
            assert!(!is_present(&blank), "{blank} is blank");
        }
        for present in [
            json!(true),
            json!(0),
            json!("x"),
            json!([null]),
            json!({"a": 1}),
        ] {
            assert!(is_present(&present), "{present} is present");
        }
    }
}
