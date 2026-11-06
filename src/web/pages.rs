//! What a local account's, a post's and a hashtag's pages say before any
//! script runs: their titles, OpenGraph tags and alternate links, and the post
//! embed. The body of each page is still the web app, which draws them.
//!
//! Mastodon: app/controllers/accounts_controller.rb
//! Mastodon: app/controllers/statuses_controller.rb
//! Mastodon: app/controllers/tags_controller.rb
//! Mastodon: app/controllers/concerns/account_owned_concern.rb
//! Mastodon: app/controllers/concerns/account_controller_concern.rb
//! Mastodon: app/views/accounts/show.html.haml
//! Mastodon: app/views/accounts/_og.html.haml
//! Mastodon: app/views/statuses/show.html.haml
//! Mastodon: app/views/statuses/embed.html.haml
//! Mastodon: app/views/statuses/_og_description.html.haml
//! Mastodon: app/views/statuses/_og_image.html.haml
//! Mastodon: app/views/tags/show.html.haml
//! Mastodon: app/views/shared/_og.html.haml
//! Mastodon: app/helpers/accounts_helper.rb
//! Mastodon: app/helpers/statuses_helper.rb
//! Mastodon: app/serializers/seo/social_media_posting_serializer.rb

use axum::http::StatusCode;
use serde_json::{json, Value};

use super::{escape_html_attribute, escape_html_text, json_escape};
use crate::{
    api::mastodon::{
        accounts::{account_to_api, batch_noindex},
        types::{self, MediaAttachment},
    },
    db::models::{Account, Status as DbStatus},
    error::AppResult,
    state::AppState,
};

/// The public paths whose answer depends on what they name.
#[derive(Debug, PartialEq)]
pub enum Route {
    /// `/@username`, and the tabs `accounts#show` also answers.
    Account {
        username: String,
        tab: Tab,
        rss: bool,
    },
    /// `/@username/:id`.
    Status { username: String, id: i64 },
    /// `/@username/:id/embed`.
    Embed { username: String, id: i64 },
    /// `/tags/:id`.
    Tag { name: String, rss: bool },
}

#[derive(Debug, PartialEq)]
pub enum Tab {
    Posts,
    WithReplies,
    Media,
    Featured,
    Collections,
    Tagged(String),
}

/// `constraints(username: %r{[^@/.]+})`.
fn username(segment: &str) -> Option<String> {
    (!segment.is_empty() && !segment.contains(['@', '/', '.'])).then(|| segment.to_owned())
}

fn without_rss(segment: &str) -> (&str, bool) {
    match segment.strip_suffix(".rss") {
        Some(rest) => (rest, true),
        None => (segment, false),
    }
}

fn numeric_id(segment: &str) -> Option<i64> {
    segment
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| segment.parse().ok())
        .flatten()
}

fn decoded(segment: &str) -> Option<String> {
    urlencoding::decode(segment).ok().map(|s| s.into_owned())
}

/// Which page `path`, without its leading slash, is.
pub fn route(path: &str) -> Option<Route> {
    if let Some(name) = path.strip_prefix("tags/") {
        let (name, rss) = without_rss(name);
        if name.is_empty() || name.contains('/') {
            return None;
        }
        return Some(Route::Tag {
            name: decoded(name)?,
            rss,
        });
    }
    let rest = path.strip_prefix('@')?;
    let segments: Vec<&str> = rest.split('/').collect();
    let account = |name: &str, tab: Tab, rss: bool| {
        Some(Route::Account {
            username: username(name)?,
            tab,
            rss,
        })
    };
    match segments.as_slice() {
        [name] => {
            let (name, rss) = without_rss(name);
            account(name, Tab::Posts, rss)
        }
        [name, "featured"] => account(name, Tab::Featured, false),
        [name, "collections"] => account(name, Tab::Collections, false),
        [name, tab] => match without_rss(tab) {
            ("with_replies", rss) => account(name, Tab::WithReplies, rss),
            ("media", rss) => account(name, Tab::Media, rss),
            _ => Some(Route::Status {
                username: username(name)?,
                id: numeric_id(tab)?,
            }),
        },
        [name, "tagged", tag] => {
            let (tag, rss) = without_rss(tag);
            if tag.is_empty() {
                return None;
            }
            account(name, Tab::Tagged(decoded(tag)?), rss)
        }
        [name, id, "embed"] => Some(Route::Embed {
            username: username(name)?,
            id: numeric_id(id)?,
        }),
        _ => None,
    }
}

/// What `AccountOwnedConcern` makes of a username.
pub enum Owner {
    Found(Box<Account>),
    NotFound,
    /// `temporary_suspension_response`.
    Suspended,
    /// `permanent_unavailability_response`.
    Gone,
}

/// `set_account`, `check_account_approval`, `check_account_confirmation` and
/// `check_account_suspension`.
pub async fn owner(state: &AppState, username: &str) -> AppResult<Owner> {
    let account = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE domain IS NULL AND lower(username) = lower($1)",
        username,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(account) = account else {
        return Ok(Owner::NotFound);
    };
    let user = sqlx::query!(
        r#"SELECT approved, confirmed_at FROM users WHERE account_id = $1"#,
        account.id,
    )
    .fetch_optional(&state.db)
    .await?;
    if !user.is_some_and(|user| user.approved && user.confirmed_at.is_some()) {
        return Ok(Owner::NotFound);
    }
    if account.is_unavailable() {
        let reversible = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM account_deletion_requests WHERE account_id = $1) AS "e!""#,
            account.id,
        )
        .fetch_one(&state.db)
        .await?;
        return Ok(if reversible {
            Owner::Suspended
        } else {
            Owner::Gone
        });
    }
    Ok(Owner::Found(Box::new(account)))
}

/// The status of `owner` a visitor who is not signed in may read:
/// `StatusPolicy#show?` with no account, which public and unlisted posts pass.
pub async fn readable_status(
    state: &AppState,
    account_id: i64,
    id: i64,
) -> AppResult<Option<DbStatus>> {
    Ok(sqlx::query_as::<_, DbStatus>(
        "SELECT * FROM statuses WHERE id = $1 AND account_id = $2 AND deleted_at IS NULL
           AND visibility IN (0, 1)",
    )
    .bind(id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?)
}

/// A page's additions, and the headers that go with them.
#[derive(Debug, Default)]
pub struct Head {
    pub title: Option<String>,
    pub head: String,
    /// The `Link` header.
    pub link: Option<String>,
}

/// What `og:*` and the rest say about a page, in the instance's default
/// locale. Eunha's server-side text is in English and Korean.
pub struct Words {
    korean: bool,
}

impl Words {
    pub fn for_instance(state: &AppState) -> Self {
        Self {
            korean: state.instance.default_locale().starts_with("ko"),
        }
    }

    /// `rss.content_warning`.
    pub fn content_warning_label(&self) -> &'static str {
        if self.korean {
            "열람 주의:"
        } else {
            "Content warning:"
        }
    }

    /// `rss.descriptions.account`.
    pub fn account_feed(&self, acct: &str) -> String {
        if self.korean {
            format!("@{acct}의 공개 게시물")
        } else {
            format!("Public posts from @{acct}")
        }
    }

    /// `rss.descriptions.tag`.
    pub fn tag_feed(&self, hashtag: &str) -> String {
        if self.korean {
            format!("#{hashtag} 해시태그가 붙은 공개 게시물")
        } else {
            format!("Public posts tagged #{hashtag}")
        }
    }

    /// `statuses.content_warning`.
    fn content_warning(&self, warning: &str) -> String {
        format!("{} {warning}", self.content_warning_label())
    }

    /// `statuses.attached`.
    fn attached(&self, images: usize, videos: usize, audios: usize) -> Option<String> {
        let plural =
            |n: usize, one: &str, other: &str| format!("{n} {}", if n == 1 { one } else { other });
        let mut parts = Vec::new();
        if images > 0 {
            parts.push(if self.korean {
                format!("{images}장의 이미지")
            } else {
                plural(images, "image", "images")
            });
        }
        if videos > 0 {
            parts.push(if self.korean {
                format!("{videos} 개의 영상")
            } else {
                plural(videos, "video", "videos")
            });
        }
        if audios > 0 {
            parts.push(if self.korean {
                format!("{audios} 개의 오디오")
            } else {
                format!("{audios} audio")
            });
        }
        if parts.is_empty() {
            return None;
        }
        let list = parts.join(" · ");
        Some(if self.korean {
            format!("첨부: {list}")
        } else {
            format!("Attached: {list}")
        })
    }

    /// `accounts.posts`, `accounts.following` and `accounts.followers`.
    fn counts(&self, posts: i64, following: i64, followers: i64) -> String {
        let (p, g, f) = if self.korean {
            ("게시물", "팔로잉", "팔로워")
        } else {
            (
                if posts == 1 { "Post" } else { "Posts" },
                "Following",
                if followers == 1 {
                    "Follower"
                } else {
                    "Followers"
                },
            )
        };
        format!(
            "{} {p}, {} {g}, {} {f}",
            formatted_stat(posts),
            formatted_stat(following),
            formatted_stat(followers),
        )
    }

    fn sensitive(&self) -> &'static str {
        if self.korean {
            "민감한 콘텐츠"
        } else {
            "Sensitive content"
        }
    }

    fn view_post(&self) -> &'static str {
        if self.korean {
            "게시물 보기"
        } else {
            "View post"
        }
    }
}

/// `account_formatted_stat`: `number_to_human` to three significant digits.
fn formatted_stat(value: i64) -> String {
    let (divisor, unit) = match value.abs() {
        n if n < 1_000 => return value.to_string(),
        n if n < 1_000_000 => (1e3, "K"),
        n if n < 1_000_000_000 => (1e6, "M"),
        n if n < 1_000_000_000_000 => (1e9, "B"),
        _ => (1e12, "T"),
    };
    let scaled = value as f64 / divisor;
    let decimals = 2usize.saturating_sub(scaled.abs().log10().floor() as usize);
    let text = format!("{scaled:.decimals$}");
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.')
    } else {
        &text
    };
    format!("{text}{unit}")
}

fn name_tag(key: &str, content: &str) -> String {
    format!(
        r#"<meta name="{}" content="{}" />"#,
        escape_html_attribute(key),
        escape_html_attribute(content)
    )
}

fn opengraph(property: &str, content: &str) -> String {
    format!(
        r#"<meta property="{}" content="{}" />"#,
        escape_html_attribute(property),
        escape_html_attribute(content)
    )
}

fn alternate(media_type: &str, href: &str) -> String {
    format!(
        r#"<link rel="alternate" type="{}" href="{}" />"#,
        escape_html_attribute(media_type),
        escape_html_attribute(href)
    )
}

const LD_JSON: &str = r#"application/ld+json; profile="https://www.w3.org/ns/activitystreams""#;

fn display_name(account: &types::Account) -> &str {
    Some(account.display_name.as_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&account.username)
}

fn local_handle(state: &AppState, username: &str) -> String {
    format!("{username}@{}", state.instance.local_domain)
}

/// `shared/_og`, for the pages with nothing more particular to say.
pub async fn instance_head(state: &AppState, url: &str) -> Head {
    let settings = crate::settings::Snapshot::load(state).await;
    let description = settings.site_short_description(&state.instance);
    let mut head = String::new();
    if !description.is_empty() {
        head.push_str(&name_tag("description", &description));
    }
    let title = settings.site_title(&state.instance);
    head.push_str(&opengraph("og:site_name", &title));
    head.push_str(&opengraph("og:url", url));
    head.push_str(&opengraph("og:type", "website"));
    head.push_str(&opengraph("og:title", &title));
    if !description.is_empty() {
        head.push_str(&opengraph("og:description", &description));
    }
    let thumbnail = crate::site_uploads::find(state, "thumbnail")
        .await
        .ok()
        .flatten();
    let image = thumbnail
        .as_ref()
        .and_then(|upload| upload.url(state, "@1x"));
    match (&thumbnail, image) {
        (Some(upload), Some(image)) => {
            head.push_str(&opengraph("og:image", &image));
            let meta = upload.meta.as_ref();
            for (property, key) in [("og:image:width", "width"), ("og:image:height", "height")] {
                if let Some(value) = meta.and_then(|meta| meta.get(key)) {
                    head.push_str(&opengraph(property, &value.to_string()));
                }
            }
        }
        _ => {
            head.push_str(&opengraph(
                "og:image",
                &format!("https://{}/images/preview.png", state.urls.web_domain),
            ));
            head.push_str(&opengraph("og:image:width", "1200"));
            head.push_str(&opengraph("og:image:height", "630"));
        }
    }
    head.push_str(&opengraph("twitter:card", "summary_large_image"));
    Head {
        title: None,
        head,
        link: None,
    }
}

/// `accounts/show` and `accounts/_og`.
pub async fn account_head(state: &AppState, account: &Account, tab: &Tab) -> Head {
    let entity = account_to_api(state, account).await;
    let noindex = batch_noindex(state, &[account.id])
        .await
        .remove(&account.id)
        .unwrap_or(false);
    let settings = crate::settings::Snapshot::load(state).await;
    let words = Words::for_instance(state);
    let handle = local_handle(state, &account.username);
    let title = format!("{} (@{handle})", display_name(&entity));
    let mut description = words.counts(
        entity.statuses_count,
        entity.following_count,
        entity.followers_count,
    );
    if !account.note.is_empty() {
        description.push_str(" · ");
        description.push_str(&account.note);
    }

    let mut head = String::new();
    if noindex {
        head.push_str(&name_tag("robots", "noindex, noarchive"));
    }
    let rss_url = match tab {
        Tab::Tagged(tag) => format!(
            "{}/tagged/{}.rss",
            entity.url,
            crate::formatter::text::escape_segment(tag)
        ),
        _ => format!("{}.rss", entity.url),
    };
    head.push_str(&alternate("application/rss+xml", &rss_url));
    head.push_str(&alternate("application/activity+json", &entity.uri));
    head.push_str(&alternate(LD_JSON, &entity.uri));
    for href in verifiable_fields(account) {
        head.push_str(&format!(
            r#"<link rel="me" type="text/html" href="{}" />"#,
            escape_html_attribute(&href)
        ));
    }
    head.push_str(&opengraph("og:type", "profile"));
    head.push_str(&name_tag("description", &description));
    head.push_str(&opengraph("og:url", &entity.url));
    head.push_str(&opengraph(
        "og:site_name",
        &settings.site_title(&state.instance),
    ));
    head.push_str(&opengraph("og:title", &title));
    head.push_str(&opengraph("og:description", &description));
    head.push_str(&opengraph("og:image", &entity.avatar));
    head.push_str(&opengraph("og:image:width", "400"));
    head.push_str(&opengraph("og:image:height", "400"));
    head.push_str(&opengraph("twitter:card", "summary"));
    head.push_str(&opengraph("profile:username", &handle));

    // `AccountControllerConcern#set_link_headers`.
    let link = format!(
        r#"<https://{web}/.well-known/webfinger?resource=acct%3A{user}%40{local}>; rel="lrdd"; type="application/jrd+json", <{uri}>; rel="alternate"; type="application/activity+json""#,
        web = state.instance.web_domain(),
        local = state.instance.local_domain,
        user = urlencoding::encode(&account.username),
        uri = entity.uri,
    );
    Head {
        title: Some(title),
        head,
        link: Some(link),
    }
}

/// `@account.fields.select(&:verifiable?)`, by their values.
fn verifiable_fields(account: &Account) -> Vec<String> {
    account
        .fields
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|field| field.get("value")?.as_str())
        .filter(|value| crate::link_verification::is_verifiable(value))
        .map(str::to_owned)
        .collect()
}

/// `status_description`.
fn status_description(words: &Words, status: &DbStatus, entity: &types::Status) -> String {
    let count = |kinds: &[&str]| {
        entity
            .media_attachments
            .iter()
            .filter(|media| kinds.contains(&media.media_type.as_str()))
            .count()
    };
    let attached = words.attached(
        count(&["image", "gifv"]),
        count(&["video"]),
        count(&["audio"]),
    );
    let warning =
        (!status.spoiler_text.is_empty()).then(|| words.content_warning(&status.spoiler_text));
    let mut components = vec![[attached, warning]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")];
    if status.spoiler_text.is_empty() {
        components.push(status.text.clone());
        if let Some(poll) = &entity.poll {
            components.push(
                poll.options
                    .iter()
                    .map(|option| format!("[ ] {}", option.title))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    }
    components.retain(|component| !component.trim().is_empty());
    components.join("\n\n")
}

/// `truncate(text, length: 50, omission: '…')`.
fn truncated(text: &str) -> String {
    if text.chars().count() <= 50 {
        return text.to_owned();
    }
    let kept: String = text.chars().take(49).collect();
    format!("{kept}…")
}

fn dimension(media: &MediaAttachment, style: &str, key: &str) -> Option<String> {
    media
        .meta
        .as_ref()?
        .get(style)?
        .get(key)
        .map(ToString::to_string)
}

fn media_url(media: &MediaAttachment) -> Option<&str> {
    media.url.as_deref().or(media.remote_url.as_deref())
}

/// `statuses/_og_image`. Eunha has no `/media/:id/player` to offer as a player
/// card, so a video or an audio file is a large summary.
fn og_image(
    entity: &types::Status,
    content_types: &std::collections::HashMap<String, Option<String>>,
    preview_sensitive: bool,
) -> String {
    let mut head = String::new();
    if entity.media_attachments.is_empty() || (entity.sensitive && !preview_sensitive) {
        head.push_str(&opengraph("twitter:card", "summary"));
        return head;
    }
    for media in &entity.media_attachments {
        let Some(url) = media_url(media) else {
            continue;
        };
        let content_type = content_types.get(&media.id).cloned().flatten();
        match media.media_type.as_str() {
            "image" => {
                head.push_str(&opengraph("og:image", url));
                if let Some(content_type) = &content_type {
                    head.push_str(&opengraph("og:image:type", content_type));
                }
                for (property, key) in [("og:image:width", "width"), ("og:image:height", "height")]
                {
                    if let Some(value) = dimension(media, "original", key) {
                        head.push_str(&opengraph(property, &value));
                    }
                }
                if let Some(description) = media.description.as_deref().filter(|d| !d.is_empty()) {
                    head.push_str(&opengraph("og:image:alt", description));
                }
            }
            "video" | "gifv" => {
                if let Some(preview) = media.preview_url.as_deref() {
                    head.push_str(&opengraph("og:image", preview));
                    head.push_str(&opengraph("og:image:type", "image/png"));
                    for (property, key) in
                        [("og:image:width", "width"), ("og:image:height", "height")]
                    {
                        if let Some(value) = dimension(media, "small", key) {
                            head.push_str(&opengraph(property, &value));
                        }
                    }
                }
                head.push_str(&opengraph("og:video", url));
                head.push_str(&opengraph("og:video:secure_url", url));
                if let Some(content_type) = &content_type {
                    head.push_str(&opengraph("og:video:type", content_type));
                }
                for (property, key) in [("og:video:width", "width"), ("og:video:height", "height")]
                {
                    if let Some(value) = dimension(media, "original", key) {
                        head.push_str(&opengraph(property, &value));
                    }
                }
            }
            "audio" => {
                head.push_str(&opengraph("og:image", &entity.account.avatar));
                head.push_str(&opengraph("og:image:width", "400"));
                head.push_str(&opengraph("og:image:height", "400"));
                head.push_str(&opengraph("og:audio", url));
                head.push_str(&opengraph("og:audio:secure_url", url));
                if let Some(content_type) = &content_type {
                    head.push_str(&opengraph("og:audio:type", content_type));
                }
            }
            _ => {}
        }
    }
    head.push_str(&opengraph("twitter:card", "summary_large_image"));
    head
}

fn interaction_counter(kind: &str, count: i64) -> Value {
    json!({
        "@type": "InteractionCounter",
        "interactionType": format!("https://schema.org/{kind}"),
        "userInteractionCount": count,
    })
}

/// `render_seo_schema`: `SEO::SocialMediaPostingSerializer`, without the
/// players' `embedUrl`, which Eunha has no page for.
fn seo_schema(state: &AppState, entity: &types::Status) -> Value {
    let handle = local_handle(state, &entity.account.username);
    let mut schema = json!({
        "@context": "https://schema.org",
        "@type": "SocialMediaPosting",
        "url": entity.url,
        "datePublished": entity.created_at,
        "dateModified": entity.edited_at,
        "author": {
            "@type": "Person",
            "name": display_name(&entity.account),
            "alternateName": handle,
            "identifier": handle,
            "url": entity.account.url,
            "interactionStatistic": [
                interaction_counter("FollowAction", entity.account.followers_count),
            ],
        },
        "text": entity.content,
        "interactionStatistic": [
            interaction_counter("LikeAction", entity.favourites_count),
            interaction_counter("ShareAction", entity.reblogs_count),
            interaction_counter("ReplyAction", entity.replies_count),
        ],
    });
    let objects = |kinds: &[&str], kind: &str| -> Vec<Value> {
        entity
            .media_attachments
            .iter()
            .filter(|media| kinds.contains(&media.media_type.as_str()))
            .map(|media| {
                json!({
                    "@type": kind,
                    "contentUrl": media_url(media),
                    "thumbnailUrl": media.preview_url,
                    "description": media.description,
                })
            })
            .collect()
    };
    for (key, kinds, kind) in [
        ("image", &["image"][..], "ImageObject"),
        ("video", &["video", "gifv"][..], "VideoObject"),
        ("audio", &["audio"][..], "AudioObject"),
    ] {
        let list = objects(kinds, kind);
        if !list.is_empty() {
            schema[key] = Value::Array(list);
        }
    }
    if let Some(card) = &entity.card {
        schema["sharedContent"] = json!({ "@type": "WebPage", "url": card.url });
    }
    schema
}

/// `statuses/show`.
pub async fn status_head(
    state: &AppState,
    account: &Account,
    status: &DbStatus,
    entity: &types::Status,
) -> AppResult<Head> {
    let settings = crate::settings::Snapshot::load(state).await;
    let words = Words::for_instance(state);
    let noindex = batch_noindex(state, &[account.id])
        .await
        .remove(&account.id)
        .unwrap_or(false);
    let handle = local_handle(state, &account.username);
    let name = display_name(&entity.account);
    let quoted = if status.spoiler_text.is_empty() {
        &status.text
    } else {
        &status.spoiler_text
    };
    let title = format!("{name}: \"{}\"", truncated(quoted));
    let url = entity.url.clone().unwrap_or_else(|| entity.uri.clone());
    let description = status_description(&words, status, entity);
    let content_types = sqlx::query!(
        "SELECT id, file_content_type FROM media_attachments WHERE status_id = $1",
        status.id,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|row| (row.id.to_string(), row.file_content_type))
    .collect();

    let mut head = String::new();
    if noindex {
        head.push_str(&name_tag("robots", "noindex, noarchive"));
    } else {
        head.push_str(&format!(
            r#"<script type="application/ld+json">{}</script>"#,
            json_escape(&seo_schema(state, entity).to_string())
        ));
    }
    head.push_str(&alternate(
        "application/json+oembed",
        &format!(
            "https://{}/api/oembed?format=json&url={}",
            state.urls.web_domain,
            urlencoding::encode(&url)
        ),
    ));
    head.push_str(&alternate("application/activity+json", &entity.uri));
    head.push_str(&alternate(LD_JSON, &entity.uri));
    head.push_str(&opengraph(
        "og:site_name",
        &settings.site_title(&state.instance),
    ));
    head.push_str(&opengraph("og:type", "article"));
    head.push_str(&opengraph("og:title", &format!("{name} (@{handle})")));
    head.push_str(&opengraph("og:url", &url));
    head.push_str(&opengraph("og:published_time", &entity.created_at));
    if let Some(language) = entity.language.as_deref().filter(|l| !l.is_empty()) {
        head.push_str(&opengraph("og:locale", language));
    }
    head.push_str(&opengraph("profile:username", &handle));
    head.push_str(&name_tag("description", &description));
    head.push_str(&opengraph("og:description", &description));
    head.push_str(&og_image(
        entity,
        &content_types,
        settings.boolean("preview_sensitive_media"),
    ));

    // `StatusesController#set_link_headers`.
    let link = format!(
        r#"<{}>; rel="alternate"; type="application/activity+json""#,
        entity.uri
    );
    Ok(Head {
        title: Some(title),
        head,
        link: Some(link),
    })
}

/// `tags/show`, whose feed link Eunha points at the feed; Mastodon's names the
/// tag's page.
pub async fn tag_head(state: &AppState, name: &str, url: &str) -> Head {
    let mut page = instance_head(state, url).await;
    let mut head = alternate(
        "application/rss+xml",
        &format!(
            "https://{}/tags/{}.rss",
            state.urls.web_domain,
            crate::formatter::text::escape_segment(name)
        ),
    );
    let tag_url = format!(
        "https://{}/tags/{}",
        state.urls.web_domain,
        crate::formatter::text::escape_segment(name)
    );
    head.push_str(&alternate("application/activity+json", &tag_url));
    head.push_str(&alternate(LD_JSON, &tag_url));
    head.push_str(&name_tag("robots", "noindex"));
    head.push_str(&page.head);
    page.head = head;
    page
}

/// `prerender_custom_emojis`, in the text of `html` and never inside a tag.
pub fn with_custom_emojis(html: &str, emojis: &[types::CustomEmoji], style: &str) -> String {
    if emojis.is_empty() {
        return html.to_owned();
    }
    let render = |text: &str| {
        let mut text = text.to_owned();
        for emoji in emojis {
            let code = format!(":{}:", emoji.shortcode);
            if text.contains(&code) {
                text = text.replace(
                    &code,
                    &format!(
                        r#"<img rel="emoji" draggable="false" alt="{code}" title="{code}" src="{}" style="{style}" />"#,
                        escape_html_attribute(&emoji.url),
                    ),
                );
            }
        }
        text
    };
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        out.push_str(&render(&rest[..open]));
        match rest[open..].find('>') {
            Some(close) => {
                out.push_str(&rest[open..=open + close]);
                rest = &rest[open + close + 1..];
            }
            None => {
                out.push_str(&rest[open..]);
                rest = "";
            }
        }
    }
    out.push_str(&render(rest));
    out
}

const EMBED_EMOJI_STYLE: &str = "height:1.2em;width:1.2em;object-fit:contain;vertical-align:middle";

const EMBED_STYLE: &str = "\
*{box-sizing:border-box}\
html,body{margin:0;background:#fcf8ff;color:#1c1a25;font:15px/1.45 system-ui,-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif}\
main{padding:16px}\
a{color:#563acc}\
.author{display:flex;gap:10px;align-items:center;color:inherit;text-decoration:none;margin-bottom:12px}\
.author img{width:44px;height:44px;border-radius:8px;object-fit:cover}\
.author b,.author span{display:block;overflow-wrap:anywhere}\
.author span{color:#787588;font-size:13px}\
.content{overflow-wrap:anywhere}\
.content p{margin:0 0 .7em}\
.content p:last-child{margin-bottom:0}\
details{margin:0 0 .7em}\
summary{cursor:pointer;font-weight:600}\
.media{display:grid;gap:4px;margin-top:12px}\
.media img,.media video{width:100%;max-height:480px;object-fit:cover;border-radius:8px;display:block}\
.media audio{width:100%}\
ul{list-style:none;margin:12px 0 0;padding:0}\
li{padding:6px 10px;border:1px solid #c9c4da;border-radius:6px;margin-top:4px}\
footer{display:flex;justify-content:space-between;gap:12px;margin-top:12px;color:#787588;font-size:13px}\
";

/// Answers `embed.js`'s question: how tall the page is.
const EMBED_SCRIPT: &str = "\
window.addEventListener('message',function(e){\
var d=e.data||{};\
if(!window.parent||d.type!=='setHeight')return;\
function send(){window.parent.postMessage({type:'setHeight',id:d.id,height:document.documentElement.scrollHeight},'*')}\
send();\
if(window.ResizeObserver)new ResizeObserver(send).observe(document.body);\
});";

/// `statuses/embed`, which Mastodon draws with React: the same post, rendered
/// here, so that a page embedding it loads no web app.
pub fn embed_page(state: &AppState, entity: &types::Status) -> String {
    let words = Words::for_instance(state);
    let esc = escape_html_text;
    let attr = escape_html_attribute;
    let url = entity.url.as_deref().unwrap_or(&entity.uri);
    let handle = local_handle(state, &entity.account.username);
    let name = with_custom_emojis(
        &esc(display_name(&entity.account)),
        &entity.account.emojis,
        EMBED_EMOJI_STYLE,
    );

    let mut body = format!(
        r#"<div class="content">{}</div>"#,
        with_custom_emojis(&entity.content, &entity.emojis, EMBED_EMOJI_STYLE)
    );
    if let Some(poll) = &entity.poll {
        body.push_str("<ul>");
        for option in &poll.options {
            body.push_str(&format!("<li>{}</li>", esc(&option.title)));
        }
        body.push_str("</ul>");
    }
    let mut media = String::new();
    for attachment in &entity.media_attachments {
        let Some(src) = media_url(attachment) else {
            continue;
        };
        let alt = attr(attachment.description.as_deref().unwrap_or_default());
        let poster = attachment
            .preview_url
            .as_deref()
            .map(|preview| format!(r#" poster="{}""#, attr(preview)))
            .unwrap_or_default();
        media.push_str(&match attachment.media_type.as_str() {
            "image" => format!(
                r#"<a href="{url}"><img src="{}" alt="{alt}" loading="lazy" /></a>"#,
                attr(attachment.preview_url.as_deref().unwrap_or(src)),
                url = attr(url),
            ),
            "gifv" => format!(
                r#"<video src="{}"{poster} aria-label="{alt}" autoplay loop muted playsinline></video>"#,
                attr(src)
            ),
            "video" => format!(
                r#"<video src="{}"{poster} aria-label="{alt}" controls preload="none"></video>"#,
                attr(src)
            ),
            "audio" => format!(
                r#"<audio src="{}" aria-label="{alt}" controls preload="none"></audio>"#,
                attr(src)
            ),
            _ => String::new(),
        });
    }
    let media = if media.is_empty() {
        media
    } else {
        format!(r#"<div class="media">{media}</div>"#)
    };

    // A content warning hides the post and its media; `sensitive` alone only
    // the media.
    let content = if entity.spoiler_text.is_empty() {
        if entity.sensitive && !media.is_empty() {
            format!(
                "{body}<details><summary>{}</summary>{media}</details>",
                words.sensitive()
            )
        } else {
            format!("{body}{media}")
        }
    } else {
        format!(
            "<details><summary>{}</summary>{body}{media}</details>",
            with_custom_emojis(
                &esc(&entity.spoiler_text),
                &entity.emojis,
                EMBED_EMOJI_STYLE
            )
        )
    };
    let date = entity.created_at.get(..10).unwrap_or(&entity.created_at);
    format!(
        concat!(
            r#"<!doctype html><html lang="{lang}"><head><meta charset="utf-8" />"#,
            r#"<meta name="robots" content="noindex" />"#,
            r#"<meta name="viewport" content="width=device-width, initial-scale=1" />"#,
            r#"<meta name="color-scheme" content="light" />"#,
            r#"<base target="_blank" /><title>{title}</title><style>{style}</style></head>"#,
            r#"<body><main><a class="author" href="{account_url}">"#,
            r#"<img src="{avatar}" alt="" /><div><b>{name}</b><span>@{handle}</span></div></a>"#,
            "{content}",
            r#"<footer><time datetime="{created}">{date}</time><a href="{url}">{view}</a></footer>"#,
            "</main><script>{script}</script></body></html>"
        ),
        lang = attr(state.instance.default_locale()),
        title = esc(&format!("@{handle}")),
        style = EMBED_STYLE,
        account_url = attr(&entity.account.url),
        avatar = attr(&entity.account.avatar),
        name = name,
        handle = esc(&handle),
        content = content,
        created = attr(&entity.created_at),
        date = esc(date),
        url = attr(url),
        view = words.view_post(),
        script = EMBED_SCRIPT,
    )
}

/// The status a refusal answers with.
pub fn refusal(owner: &Owner) -> Option<StatusCode> {
    match owner {
        Owner::Found(_) => None,
        Owner::NotFound => Some(StatusCode::NOT_FOUND),
        Owner::Suspended => Some(StatusCode::FORBIDDEN),
        Owner::Gone => Some(StatusCode::GONE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(username: &str, tab: Tab, rss: bool) -> Option<Route> {
        Some(Route::Account {
            username: username.to_owned(),
            tab,
            rss,
        })
    }

    #[test]
    fn routes_are_mastodons() {
        assert_eq!(route("@alice"), account("alice", Tab::Posts, false));
        assert_eq!(route("@alice.rss"), account("alice", Tab::Posts, true));
        assert_eq!(
            route("@alice/with_replies.rss"),
            account("alice", Tab::WithReplies, true)
        );
        assert_eq!(route("@alice/media"), account("alice", Tab::Media, false));
        assert_eq!(
            route("@alice/tagged/caf%C3%A9.rss"),
            account("alice", Tab::Tagged("café".to_owned()), true)
        );
        assert_eq!(
            route("@alice/123"),
            Some(Route::Status {
                username: "alice".to_owned(),
                id: 123
            })
        );
        assert_eq!(
            route("@alice/123/embed"),
            Some(Route::Embed {
                username: "alice".to_owned(),
                id: 123
            })
        );
        assert_eq!(
            route("tags/rust.rss"),
            Some(Route::Tag {
                name: "rust".to_owned(),
                rss: true
            })
        );
        for path in [
            "",
            "@",
            "@alice@example.com",
            "@alice@example.com/123",
            "@a.b",
            "@alice/nope",
            "@alice/123/nope",
            "@alice/wrapstodon/2025/key",
            "tags/",
            "explore",
        ] {
            assert_eq!(route(path), None, "{path}");
        }
    }

    #[test]
    fn stats_are_three_significant_digits() {
        assert_eq!(formatted_stat(0), "0");
        assert_eq!(formatted_stat(999), "999");
        assert_eq!(formatted_stat(1_000), "1K");
        assert_eq!(formatted_stat(1_234), "1.23K");
        assert_eq!(formatted_stat(12_345), "12.3K");
        assert_eq!(formatted_stat(123_456), "123K");
        assert_eq!(formatted_stat(1_500_000), "1.5M");
    }

    #[test]
    fn title_quote_is_cut_at_fifty() {
        assert_eq!(truncated("short"), "short");
        let long = "가".repeat(60);
        let cut = truncated(&long);
        assert_eq!(cut.chars().count(), 50);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn custom_emojis_stay_out_of_tags() {
        let emoji: types::CustomEmoji = types::CustomEmoji {
            shortcode: "blob".to_owned(),
            url: "https://e.example/blob.png".to_owned(),
            static_url: "https://e.example/blob.png".to_owned(),
            visible_in_picker: true,
            category: None,
            featured: None,
        };
        let html = with_custom_emojis(
            r#"<a href="https://x.example/:blob:">:blob:</a> tail :blob:"#,
            &[emoji],
            "s",
        );
        assert!(html.starts_with(r#"<a href="https://x.example/:blob:"><img"#));
        assert_eq!(html.matches("<img").count(), 2);
    }
}
