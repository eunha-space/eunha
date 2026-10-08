//! Formatting as the API serializers use it: Mastodon's `FormattingHelper`
//! over [`crate::formatter`], with the database lookups `TextFormatter`
//! makes along the way, and `StatusLengthValidator`.

use std::collections::HashMap;

use sqlx::Row;
use unicode_segmentation::UnicodeSegmentation;

use super::types;
use crate::db::models;
use crate::formatter::{self, extractor, MentionTarget, Options};
use crate::state::AppState;

/// Number of characters a URL counts as, regardless of its real length
/// (Mastodon `StatusLengthValidator::URL_PLACEHOLDER_CHARS`).
const URL_PLACEHOLDER_CHARS: usize = 23;

/// Countable length of a status for the 500-char limit, matching Mastodon's
/// `StatusLengthValidator`: the spoiler/CW text plus the body, where every URL
/// counts as 23 characters and every mention drops its `@domain` part, measured
/// in grapheme clusters (not codepoints).
pub fn countable_length(text: &str, spoiler_text: &str) -> usize {
    let combined = format!("{spoiler_text}{}", countable_text(text));
    combined.graphemes(true).count()
}

/// `StatusLengthValidator#countable_text`: the URLs and mentions entity
/// extraction finds, rewritten to a fixed placeholder and to `@username`.
fn countable_text(text: &str) -> String {
    if formatter::text::blank(text) {
        return String::new();
    }
    let mut entities = extractor::extract_urls(text);
    entities.extend(extractor::extract_mentions(text));
    let mut result = String::with_capacity(text.len());
    let mut last = 0usize;
    for entity in extractor::remove_overlapping_entities(entities) {
        result.push_str(&text[last..entity.start]);
        match &entity.kind {
            extractor::Kind::Mention(screen_name) => {
                result.push('@');
                result.push_str(screen_name.split('@').next().unwrap_or_default());
            }
            _ => result.push_str(&"x".repeat(URL_PLACEHOLDER_CHARS)),
        }
        last = entity.end;
    }
    result.push_str(&text[last..]);
    result
}

/// `PlainTextFormatter`, as `FormattingHelper.extract_status_plain_text` uses it
/// for a remote post.
pub fn html_to_plain_text(html: &str) -> String {
    formatter::plain_text::format(html, false)
}

/// The account a local status is written by, as `preloaded_accounts` holds it.
pub fn mention_target_for_account(local_domain: &str, account: &models::Account) -> MentionTarget {
    MentionTarget {
        username: account.username.clone(),
        domain: account.domain.clone(),
        url: account_url(
            local_domain,
            &account.username,
            account.domain.as_deref(),
            account.url.as_deref(),
        ),
    }
}

/// `ActivityPub::TagManager#url_for` an account.
fn account_url(
    local_domain: &str,
    username: &str,
    domain: Option<&str>,
    url: Option<&str>,
) -> String {
    match domain {
        None => format!("https://{local_domain}/@{username}"),
        Some(_) => url.unwrap_or_default().to_owned(),
    }
}

/// A status's mention, as `preloaded_accounts` holds the account.
pub fn mention_target_for_mention(
    local_domain: &str,
    mention: &types::StatusMention,
) -> MentionTarget {
    let domain = mention.acct.split_once('@').map(|(_, d)| d.to_owned());
    MentionTarget {
        username: mention.username.clone(),
        url: if domain.is_none() {
            format!("https://{local_domain}/@{}", mention.username)
        } else {
            mention.url.clone()
        },
        domain,
    }
}

/// `FormattingHelper#status_content_format` for a status that has no quote
/// (see [`apply_quote_fallbacks`]): a local one's text through
/// `TextFormatter`, its author and mentioned accounts preloaded, a remote
/// one's HTML sanitized.
pub fn status_content(
    local_domain: &str,
    text: &str,
    account: &models::Account,
    mentions: &[types::StatusMention],
) -> String {
    let local = account.domain.is_none();
    if !local {
        return formatter::html_aware(text, false, &Options::new(local_domain));
    }
    let mut preloaded = vec![mention_target_for_account(local_domain, account)];
    preloaded.extend(
        mentions
            .iter()
            .map(|m| mention_target_for_mention(local_domain, m)),
    );
    formatter::html_aware(
        text,
        true,
        &Options {
            preloaded_accounts: &preloaded,
            ..Options::new(local_domain)
        },
    )
}

/// `EntityCache#mention` for every mention `TextFormatter` would look up in
/// `texts`: the accounts it would link to.
pub async fn mention_lookup(state: &AppState, texts: &[&str]) -> Vec<MentionTarget> {
    let local_domain = &state.urls.local_domain;
    let mut usernames: Vec<String> = Vec::new();
    let mut domains: Vec<String> = Vec::new();
    for text in texts {
        for (username, domain) in formatter::text::mention_lookups(text, local_domain) {
            // `Account.find_remote` refuses this one.
            if domain.as_deref() == Some("handle.invalid") {
                continue;
            }
            usernames.push(username);
            domains.push(domain.unwrap_or_default());
        }
    }
    if usernames.is_empty() {
        return Vec::new();
    }
    let rows = sqlx::query(
        r#"SELECT DISTINCT ON (lower(a.username), COALESCE(lower(a.domain), ''))
                  a.username, a.domain, a.url
           FROM accounts a
           JOIN unnest($1::text[], $2::text[]) AS h(username, domain)
             ON lower(a.username) = lower(h.username)
            AND COALESCE(lower(a.domain), '') = lower(h.domain)
           ORDER BY lower(a.username), COALESCE(lower(a.domain), ''), a.id"#,
    )
    .bind(&usernames)
    .bind(&domains)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    rows.into_iter()
        .map(|row| {
            let username: String = row.get("username");
            let domain: Option<String> = row.get("domain");
            let url: Option<String> = row.get("url");
            MentionTarget {
                url: account_url(local_domain, &username, domain.as_deref(), url.as_deref()),
                username,
                domain,
            }
        })
        .collect()
}

/// `FormattingHelper#account_bio_format`, mentions looked up.
pub async fn account_bio(state: &AppState, note: &str, local: bool) -> String {
    if !local {
        return formatter::html_aware(note, false, &Options::new(&state.urls.local_domain));
    }
    let lookup = mention_lookup(state, &[note]).await;
    formatter::local_bio(note, &state.urls.local_domain, &lookup)
}

/// `linkify(text)`: announcements, warnings, strikes, report notes.
pub async fn linkify(state: &AppState, text: &str) -> String {
    let lookup = mention_lookup(state, &[text]).await;
    formatter::text::format(
        text,
        &Options {
            lookup: &lookup,
            ..Options::new(&state.urls.local_domain)
        },
    )
}

/// `FormattingHelper#account_field_value_format` for each of `fields`.
pub fn field_values(
    local_domain: &str,
    fields: Vec<types::Field>,
    local: bool,
    lookup: &[MentionTarget],
) -> Vec<types::Field> {
    fields
        .into_iter()
        .map(|f| types::Field {
            value: if local {
                formatter::local_field_value(&f.value, local_domain, lookup)
            } else {
                formatter::remote_field_value(&f.value, f.verified_at.is_some())
            },
            name: f.name,
            verified_at: f.verified_at,
        })
        .collect()
}

/// `TextFormatter#to_s` reads no accounts when it has to serialize one
/// without the database to hand, so a local account's bio and fields are
/// rendered with their mentions unlinked; this renders them again, the
/// mentions looked up, for every local account among `accounts` that has any.
pub async fn link_profile_mentions<'a>(
    state: &AppState,
    accounts: impl IntoIterator<Item = &'a mut types::Account>,
) {
    let mut accounts: Vec<&mut types::Account> = accounts
        .into_iter()
        .filter(|a| !a.acct.contains('@'))
        .collect();
    let ids: Vec<i64> = accounts.iter().filter_map(|a| a.id.parse().ok()).collect();
    if ids.is_empty() {
        return;
    }
    let rows = sqlx::query(
        r#"SELECT id, note, fields FROM accounts
           WHERE id = ANY($1) AND domain IS NULL
             AND (note LIKE '%@%' OR fields::text LIKE '%@%')"#,
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    if rows.is_empty() {
        return;
    }
    let profiles: HashMap<String, (String, Option<serde_json::Value>)> = rows
        .into_iter()
        .map(|row| {
            let id: i64 = row.get("id");
            (id.to_string(), (row.get("note"), row.get("fields")))
        })
        .collect();
    let mut texts: Vec<&str> = Vec::new();
    for (note, fields) in profiles.values() {
        texts.push(note);
        if let Some(arr) = fields.as_ref().and_then(|f| f.as_array()) {
            texts.extend(arr.iter().filter_map(|f| f["value"].as_str()));
        }
    }
    let lookup = mention_lookup(state, &texts).await;
    if lookup.is_empty() {
        return;
    }
    let local_domain = &state.urls.local_domain;
    for account in accounts.iter_mut() {
        let Some((note, fields)) = profiles.get(&account.id) else {
            continue;
        };
        // An unavailable account's profile is blanked; leave it so.
        if !account.note.is_empty() {
            account.note = formatter::local_bio(note, local_domain, &lookup);
        }
        if !account.fields.is_empty() {
            let raw = super::convert::fields_from_db(
                fields.as_ref().unwrap_or(&serde_json::json!([])),
                true,
            );
            account.fields = field_values(local_domain, raw, true, &lookup);
        }
    }
}

/// `url_for(quoted_status) || uri_for(quoted_status)` for each local status
/// among `status_ids` that quotes one still there: what `TextFormatter`'s
/// quote fallback links to.
pub async fn quote_fallback_urls(state: &AppState, status_ids: &[i64]) -> HashMap<i64, String> {
    if status_ids.is_empty() {
        return HashMap::new();
    }
    sqlx::query(
        r#"SELECT q.status_id, qs.id AS quoted_id, qs.url, qs.uri, qa.username,
                  qa.domain IS NULL AS local
           FROM quotes q
           JOIN statuses s ON s.id = q.status_id
           JOIN accounts sa ON sa.id = s.account_id AND sa.domain IS NULL
           JOIN statuses qs ON qs.id = q.quoted_status_id AND qs.deleted_at IS NULL
           JOIN accounts qa ON qa.id = qs.account_id
           WHERE q.status_id = ANY($1)"#,
    )
    .bind(status_ids)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|row| {
        let status_id: i64 = row.get("status_id");
        let url = if row.get::<bool, _>("local") {
            format!(
                "https://{}/@{}/{}",
                state.urls.local_domain,
                row.get::<String, _>("username"),
                row.get::<i64, _>("quoted_id")
            )
        } else {
            row.get::<Option<String>, _>("url")
                .or_else(|| row.get::<Option<String>, _>("uri"))
                .unwrap_or_default()
        };
        (status_id, url)
    })
    .collect()
}

/// `TextFormatter`'s quote fallback over the content of every status in
/// `statuses`, the boosts and quotes they carry included, that is local and
/// quotes another.
pub async fn apply_quote_fallbacks(state: &AppState, statuses: &mut [&mut types::Status]) {
    fn collect(s: &types::Status, ids: &mut Vec<i64>) {
        if let Ok(id) = s.id.parse() {
            ids.push(id);
        }
        if let Some(rb) = s.reblog.as_deref() {
            collect(rb, ids);
        }
        if let Some(q) = s.quote.as_ref().and_then(|q| q.quoted_status.as_deref()) {
            collect(q, ids);
        }
    }
    fn apply(s: &mut types::Status, urls: &HashMap<i64, String>) {
        if let Some(url) = s.id.parse::<i64>().ok().and_then(|id| urls.get(&id)) {
            s.content =
                formatter::text::add_quote_fallback(std::mem::take(&mut s.content), Some(url));
        }
        if let Some(rb) = s.reblog.as_deref_mut() {
            apply(rb, urls);
        }
        if let Some(q) = s
            .quote
            .as_mut()
            .and_then(|q| q.quoted_status.as_deref_mut())
        {
            apply(q, urls);
        }
    }
    let mut ids = Vec::new();
    for s in statuses.iter() {
        collect(s, &mut ids);
    }
    let urls = quote_fallback_urls(state, &ids).await;
    if urls.is_empty() {
        return;
    }
    for s in statuses.iter_mut() {
        apply(s, &urls);
    }
}

#[cfg(test)]
mod tests {
    use super::countable_length;

    #[test]
    fn plain_text_counts_graphemes() {
        assert_eq!(countable_length("hello", ""), 5);
    }

    #[test]
    fn spoiler_text_is_included() {
        // 3 (spoiler) + 5 (body) = 8
        assert_eq!(countable_length("hello", "cw:"), 8);
    }

    #[test]
    fn url_counts_as_23_regardless_of_length() {
        let url = "https://example.com/a/very/long/path/that/is/way/over/23/characters";
        assert!(url.len() > 23);
        assert_eq!(countable_length(url, ""), 23);
        // "see " (4) + url (23) = 27
        assert_eq!(countable_length(&format!("see {url}"), ""), 27);
    }

    #[test]
    fn mention_drops_domain() {
        // "@alice" (6) + " hi" (3) = 9, the "@remote.example.org" is not counted.
        assert_eq!(countable_length("@alice@remote.example.org hi", ""), 9);
        // A local mention (no domain) is unchanged: "@bob" (4) + " hi" (3) = 7.
        assert_eq!(countable_length("@bob hi", ""), 7);
    }

    #[test]
    fn blank_text_counts_nothing() {
        assert_eq!(countable_length("   ", "cw"), 2);
    }

    #[test]
    fn html_to_plain_text_keeps_breaks() {
        assert_eq!(
            super::html_to_plain_text("<p>a &amp; b<br>c</p><p>d</p>"),
            "a & b\nc\nd"
        );
    }
}
