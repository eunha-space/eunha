//! A member's own data, out and back in, as Mastodon's settings pages offer
//! it: the CSV and JSON exports (`Export`, `Settings::Exports::*`), the
//! imports that read them back (`Form::Import`, `BulkImport`,
//! `BulkImportService`, `BulkImportRowService`), and the archive takeout
//! (`Backup`, `BackupService`).
//!
//! This is not `eunha import-mastodon` (*src/import.rs*), which moves a
//! whole instance's database; this is one account's lists and archive.
//!
//! Mastodon serves all of it from web forms. Eunha's client is a single-page
//! app, so it is served over REST under `/api/eunha/v1/` (the
//! `data-portability-rest-api` divergence); the handlers are in
//! *src/api/eunha/portability.rs*.

pub mod csv;
pub mod export;

use crate::state::AppState;

/// `Export#acct`: a local account as `username@local_domain`, so that the
/// file still names it once it is read on another server, and a remote
/// one as `username@domain`.
pub fn export_acct(state: &AppState, username: &str, domain: Option<&str>) -> String {
    format!(
        "{username}@{}",
        domain.unwrap_or(state.instance.domain.as_str())
    )
}

/// `ActivityPub::TagManager#uri_for` on a status: a local one's ActivityPub
/// id (its `Announce`'s, for a boost), a remote one's stored `uri`.
pub struct StatusUriParts<'a> {
    pub status_id: i64,
    pub uri: Option<&'a str>,
    pub reblog: bool,
    pub account_id: i64,
    pub account_id_scheme: Option<i32>,
    pub account_username: &'a str,
    pub account_domain: Option<&'a str>,
}

pub fn status_uri(state: &AppState, parts: StatusUriParts<'_>) -> String {
    if parts.account_domain.is_some() {
        return parts.uri.unwrap_or_default().to_owned();
    }
    let uri = crate::federation::tag::status_uri(
        &state.instance.domain,
        parts.account_id,
        parts.account_id_scheme,
        parts.account_username,
        parts.status_id,
    );
    if parts.reblog {
        format!("{uri}/activity")
    } else {
        uri
    }
}
