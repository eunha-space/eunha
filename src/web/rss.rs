//! RSS feeds of an account's and a hashtag's public posts.
//!
//! Mastodon: app/lib/rss/builder.rb
//! Mastodon: app/lib/rss/channel.rb
//! Mastodon: app/lib/rss/element.rb
//! Mastodon: app/lib/rss/item.rb
//! Mastodon: app/lib/rss/media_content.rb
//! Mastodon: app/views/accounts/show.rss.ruby
//! Mastodon: app/views/tags/show.rss.ruby
//! Mastodon: app/helpers/formatting_helper.rb

use std::collections::HashMap;
use std::fmt::Write;

use super::{
    escape_html_text,
    pages::{with_custom_emojis, Tab, Words},
};
use crate::{
    api::mastodon::{
        accounts::account_to_api,
        timelines::{build_status_list_with_filters, feed_reach, FeedKind},
        types,
    },
    db::models::{Account, Status as DbStatus},
    error::AppResult,
    search::tags::FoundTag,
    state::AppState,
};

/// `AccountsController::PAGE_SIZE`, and `TagsController`'s.
const PAGE_SIZE: i64 = 20;
/// `AccountsController::PAGE_SIZE_MAX`, and `TagsController`'s.
const PAGE_SIZE_MAX: i64 = 200;

/// `FormattingHelper::SYNDICATED_EMOJI_STYLES`.
const SYNDICATED_EMOJI_STYLES: &str =
    "height: 1.1em; margin: -.2ex .15em .2ex; object-fit: contain; vertical-align: middle; width: 1.1em;";

/// `params[:limit].present? ? [params[:limit].to_i, PAGE_SIZE_MAX].min : PAGE_SIZE`.
pub fn limit_param(limit: Option<&str>) -> i64 {
    match limit.filter(|limit| !limit.is_empty()) {
        Some(limit) => {
            // `String#to_i`: the leading digits, or nothing.
            let digits: String = limit
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse::<i64>().unwrap_or(0).min(PAGE_SIZE_MAX)
        }
        None => PAGE_SIZE,
    }
}

/// `AccountsController#filtered_statuses.without_reblogs.limit(limit)`.
pub async fn account_statuses(
    state: &AppState,
    account: &Account,
    tab: &Tab,
    limit: i64,
) -> AppResult<Vec<DbStatus>> {
    let tag_id = match tab {
        Tab::Tagged(tag) => match crate::search::tags::find_normalized(state, tag).await? {
            Some(tag) => Some(tag.id),
            // `Status.none`.
            None => return Ok(Vec::new()),
        },
        _ => None,
    };
    let only_media = matches!(tab, Tab::Media);
    let with_replies = matches!(tab, Tab::WithReplies);
    // `no_replies_scope unless replies_requested?`.
    let without_replies = !with_replies;
    let statuses = sqlx::query_as::<_, DbStatus>(
        "SELECT s.* FROM statuses s
         WHERE s.account_id = $1 AND s.deleted_at IS NULL
           AND s.visibility IN (0, 1) AND s.reblog_of_id IS NULL
           AND (NOT $2 OR s.reply = FALSE OR s.in_reply_to_account_id = s.account_id)
           AND (NOT $3 OR EXISTS (SELECT 1 FROM media_attachments m WHERE m.status_id = s.id))
           AND ($4::bigint IS NULL OR EXISTS (
               SELECT 1 FROM statuses_tags st WHERE st.status_id = s.id AND st.tag_id = $4))
         ORDER BY s.id DESC LIMIT $5",
    )
    .bind(account.id)
    .bind(without_replies)
    .bind(only_media)
    .bind(tag_id)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    Ok(statuses)
}

/// `TagFeed.new(@tag, nil, local: @local).get(limit)`, which is empty when the
/// settings keep the feed from visitors.
pub async fn tag_statuses(
    state: &AppState,
    tag: &FoundTag,
    local: bool,
    limit: i64,
) -> AppResult<Vec<DbStatus>> {
    let Ok(reach) = feed_reach(state, None, FeedKind::Topic, local, false).await else {
        return Ok(Vec::new());
    };
    if reach.nothing {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as::<_, DbStatus>(
        "SELECT s.* FROM statuses s
         JOIN statuses_tags st ON st.status_id = s.id
         JOIN accounts a ON a.id = s.account_id
         WHERE st.tag_id = $1 AND s.visibility = 0 AND s.deleted_at IS NULL
           AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
           AND a.silenced_at IS NULL
           AND (NOT $2 OR a.domain IS NULL)
           AND (NOT $3 OR a.domain IS NOT NULL)
         ORDER BY s.id DESC LIMIT $4",
    )
    .bind(tag.id)
    .bind(reach.local_only)
    .bind(reach.remote_only)
    .bind(limit)
    .fetch_all(&state.db)
    .await?)
}

/// What the entities leave out of an attachment.
struct MediaFile {
    content_type: Option<String>,
    size: Option<i32>,
    thumbnail: bool,
}

async fn media_files(
    state: &AppState,
    status_ids: &[i64],
) -> AppResult<HashMap<String, MediaFile>> {
    Ok(sqlx::query!(
        "SELECT id, file_content_type, file_file_size, thumbnail_file_name
         FROM media_attachments WHERE status_id = ANY($1::bigint[])",
        status_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|row| {
        (
            row.id.to_string(),
            MediaFile {
                content_type: row.file_content_type,
                size: row.file_file_size,
                thumbnail: row.thumbnail_file_name.is_some(),
            },
        )
    })
    .collect())
}

/// Text in XML. `Ox` writes these five.
fn xml(text: &str) -> String {
    escape_html_text(text)
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn element(out: &mut String, name: &str, content: &str) {
    // `create_element`: `element << content if content.present?`.
    if content.trim().is_empty() {
        let _ = write!(out, "<{name}/>");
    } else {
        let _ = write!(out, "<{name}>{}</{name}>", xml(content));
    }
}

/// `date.to_fs(:rfc822)`.
fn rfc822(iso: &str) -> Option<String> {
    let date = chrono::DateTime::parse_from_rfc3339(iso).ok()?;
    Some(date.format("%a, %d %b %Y %H:%M:%S %z").to_string())
}

/// `rss_status_content_format`.
fn description(words: &Words, status: &types::Status) -> String {
    let mut html = String::new();
    if !status.spoiler_text.is_empty() {
        let _ = write!(
            html,
            "<p><strong>{}</strong>{}</p><hr />",
            words.content_warning_label(),
            escape_html_text(&status.spoiler_text),
        );
    }
    html.push_str(&status.content);
    if let Some(poll) = &status.poll {
        let input = if poll.multiple { "checkbox" } else { "radio" };
        let options: Vec<String> = poll
            .options
            .iter()
            .map(|option| {
                format!(
                    r#"<{input} disabled="disabled">{}</{input}>"#,
                    escape_html_text(&option.title)
                )
            })
            .collect();
        let _ = write!(html, "<p>{}</p>", options.join("<br />"));
    }
    with_custom_emojis(&html, &status.emojis, SYNDICATED_EMOJI_STYLES)
}

fn item(
    out: &mut String,
    words: &Words,
    status: &types::Status,
    files: &HashMap<String, MediaFile>,
) {
    out.push_str("<item>");
    let url = status.url.as_deref().unwrap_or(&status.uri);
    let _ = write!(out, r#"<guid isPermaLink="true">{}</guid>"#, xml(url));
    element(out, "link", url);
    if let Some(date) = rfc822(&status.created_at) {
        element(out, "pubDate", &date);
    }
    element(out, "description", &description(words, status));

    let file_url = |media: &types::MediaAttachment| {
        media
            .url
            .clone()
            .or_else(|| media.remote_url.clone())
            .unwrap_or_default()
    };
    if let Some(media) = status
        .media_attachments
        .first()
        .filter(|media| media.media_type == "audio")
    {
        let file = files.get(&media.id);
        let _ = write!(
            out,
            r#"<enclosure url="{}" length="{}" type="{}"/>"#,
            xml(&file_url(media)),
            file.and_then(|file| file.size).unwrap_or(0),
            xml(file
                .and_then(|file| file.content_type.as_deref())
                .unwrap_or_default()),
        );
    }
    for media in &status.media_attachments {
        let url = file_url(media);
        if url.is_empty() {
            continue;
        }
        let file = files.get(&media.id);
        let _ = write!(out, r#"<media:content url="{}""#, xml(&url));
        if let Some(content_type) = file.and_then(|file| file.content_type.as_deref()) {
            let _ = write!(out, r#" type="{}""#, xml(content_type));
        }
        if let Some(size) = file.and_then(|file| file.size) {
            let _ = write!(out, r#" fileSize="{size}""#);
        }
        let medium = if media.media_type == "gifv" {
            "image"
        } else {
            media.media_type.as_str()
        };
        let _ = write!(out, r#" medium="{}">"#, xml(medium));
        let _ = write!(
            out,
            r#"<media:rating scheme="urn:simple">{}</media:rating>"#,
            if status.sensitive {
                "adult"
            } else {
                "nonadult"
            }
        );
        if let Some(text) = media
            .description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
        {
            let _ = write!(
                out,
                r#"<media:description type="plain">{}</media:description>"#,
                xml(text)
            );
        }
        if file.is_some_and(|file| file.thumbnail) {
            if let Some(thumbnail) = media.preview_url.as_deref() {
                let _ = write!(out, r#"<media:thumbnail url="{}"/>"#, xml(thumbnail));
            }
        }
        out.push_str("</media:content>");
    }
    for tag in &status.tags {
        element(out, "category", &tag.name);
    }
    out.push_str("</item>");
}

/// `RSS::Builder#wrap_in_document`.
fn document(channel: String) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<rss version="2.0" xmlns:webfeeds="http://webfeeds.org/rss/1.0" "#,
            r#"xmlns:media="http://search.yahoo.com/mrss/"><channel>{}</channel></rss>"#
        ),
        channel
    )
}

fn generator() -> String {
    format!("eunha v{}", crate::version::EUNHA)
}

async fn items(
    state: &AppState,
    words: &Words,
    out: &mut String,
    statuses: Vec<DbStatus>,
) -> AppResult<()> {
    let ids: Vec<i64> = statuses.iter().map(|status| status.id).collect();
    let files = media_files(state, &ids).await?;
    let entities = build_status_list_with_filters(state, statuses, None).await?;
    for status in &entities {
        item(out, words, status, &files);
    }
    Ok(())
}

/// `accounts/show.rss`.
pub async fn account_feed(
    state: &AppState,
    account: &Account,
    tab: &Tab,
    statuses: Vec<DbStatus>,
) -> AppResult<String> {
    let words = Words::for_instance(state);
    let entity = account_to_api(state, account).await;
    let name = if entity.display_name.trim().is_empty() {
        entity.username.clone()
    } else {
        entity.display_name.clone()
    };
    let link = match tab {
        Tab::Tagged(tag) => format!(
            "{}/tagged/{}",
            entity.url,
            crate::formatter::text::escape_segment(tag)
        ),
        _ => entity.url.clone(),
    };
    let mut out = String::new();
    element(&mut out, "title", &name);
    element(
        &mut out,
        "description",
        &words.account_feed(&format!(
            "{}@{}",
            account.username, state.instance.local_domain
        )),
    );
    element(&mut out, "link", &link);
    out.push_str("<image>");
    element(&mut out, "url", &entity.avatar);
    element(&mut out, "title", &name);
    element(&mut out, "link", &link);
    out.push_str("</image>");
    if let Some(date) = statuses.first().map(|status| status.created_at) {
        element(
            &mut out,
            "lastBuildDate",
            &date
                .and_utc()
                .format("%a, %d %b %Y %H:%M:%S %z")
                .to_string(),
        );
    }
    element(&mut out, "webfeeds:icon", &entity.avatar);
    element(&mut out, "generator", &generator());
    items(state, &words, &mut out, statuses).await?;
    Ok(document(out))
}

/// `tags/show.rss`.
pub async fn tag_feed(
    state: &AppState,
    tag: &FoundTag,
    statuses: Vec<DbStatus>,
) -> AppResult<String> {
    let words = Words::for_instance(state);
    let mut out = String::new();
    element(&mut out, "title", &format!("#{}", tag.display_name));
    element(&mut out, "description", &words.tag_feed(&tag.display_name));
    element(
        &mut out,
        "link",
        &format!(
            "https://{}/tags/{}",
            state.urls.web_domain,
            crate::formatter::text::escape_segment(&tag.name)
        ),
    );
    if let Some(date) = statuses.first().map(|status| status.created_at) {
        element(
            &mut out,
            "lastBuildDate",
            &date
                .and_utc()
                .format("%a, %d %b %Y %H:%M:%S %z")
                .to_string(),
        );
    }
    element(&mut out, "generator", &generator());
    items(state, &words, &mut out, statuses).await?;
    Ok(document(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_is_rails_to_i_capped() {
        assert_eq!(limit_param(None), 20);
        assert_eq!(limit_param(Some("")), 20);
        assert_eq!(limit_param(Some("5")), 5);
        assert_eq!(limit_param(Some("9000")), 200);
        assert_eq!(limit_param(Some("7up")), 7);
        assert_eq!(limit_param(Some("nope")), 0);
        assert_eq!(limit_param(Some("-3")), 0);
    }

    #[test]
    fn dates_are_rfc822() {
        assert_eq!(
            rfc822("2026-11-05T09:45:01.000Z").as_deref(),
            Some("Thu, 05 Nov 2026 09:45:01 +0000")
        );
    }

    #[test]
    fn text_is_escaped_for_xml() {
        let mut out = String::new();
        element(&mut out, "title", r#"<a> & "b" 'c'"#);
        assert_eq!(
            out,
            "<title>&lt;a&gt; &amp; &quot;b&quot; &apos;c&apos;</title>"
        );
    }
}
