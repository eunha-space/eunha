//! Shared ActivityPub `Note` construction for locally-authored statuses.
//!
//! Both the outbox and the per-status dereferencing endpoint build their `Note`
//! objects here so the wire representation (content, media `attachment`, and the
//! `tag` array of mentions/hashtags/custom emoji) stays identical regardless of
//! how a remote server discovers the post.

use serde_json::{json, Value};

use crate::api::mastodon::convert;
use crate::api::mastodon::formatting::render_content;
use crate::db::models::{self, vis};
use crate::{error::AppResult, state::AppState};

/// JSON-LD `@context` for a `Create(Note)` / `Note` we serve. Declares the Toot,
/// hashtag/emoji, and FEP-044f quote terms used below.
pub fn note_context() -> Value {
    json!([
        "https://www.w3.org/ns/activitystreams",
        {
            "sensitive": "as:sensitive",
            "toot": "http://joinmastodon.org/ns#",
            "votersCount": "toot:votersCount",
            "blurhash": "toot:blurhash",
            "Hashtag": "as:Hashtag",
            "Emoji": "toot:Emoji",
            "focalPoint": { "@container": "@list", "@id": "toot:focalPoint" },
            "fep": "https://w3id.org/fep/044f#",
            "quote": { "@id": "fep:quote", "@type": "@id" },
            "quoteUrl": { "@id": "fep:quote", "@type": "@id" },
            "_misskey_quote": "https://misskey-hub.net/ns#quoteUri",
            "quoteAuthorization": { "@id": "fep:quoteAuthorization", "@type": "@id" },
            // FEP-7888 / GoToSocial interaction policy terms, so the
            // `interactionPolicy` we emit below survives JSON-LD expansion.
            "gts": "https://gotosocial.org/ns#",
            "interactionPolicy": { "@id": "gts:interactionPolicy", "@type": "@id" },
            "canQuote": { "@id": "gts:canQuote", "@type": "@id" },
            "automaticApproval": { "@id": "gts:automaticApproval", "@type": "@id" },
            "manualApproval": { "@id": "gts:manualApproval", "@type": "@id" },
        }
    ])
}

/// A built `Note` plus the addressing needed to wrap it in a `Create`.
pub struct NoteBundle {
    /// The `Note` object, without an `@context` (suitable for embedding).
    pub note: Value,
    /// The local author's actor URI.
    pub actor_url: String,
    /// The canonical AP id of the note (`actor/statuses/{id}`).
    pub note_uri: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl NoteBundle {
    /// Wrap the note in a `Create` activity (id `{note_uri}/activity`), with the
    /// full `@context`.
    pub fn into_create(self) -> Value {
        let activity_id = format!("{}/activity", self.note_uri);
        json!({
            "@context": note_context(),
            "id": activity_id,
            "type": "Create",
            "actor": self.actor_url,
            "published": self.created_at.to_rfc3339(),
            "to": self.to,
            "cc": self.cc,
            "object": self.note,
        })
    }

    /// The standalone `Note` object with its `@context`, for serving at the
    /// note's own URI.
    pub fn into_note(mut self) -> Value {
        self.note["@context"] = note_context();
        self.note
    }
}

/// Build the AP `Note` for a local, non-reblog status. Returns `Ok(None)` if the
/// status doesn't exist, is deleted, is remote, or is a boost.
pub async fn build_note(
    state: &AppState,
    domain: &str,
    status_id: i64,
) -> AppResult<Option<NoteBundle>> {
    let s = sqlx::query!(
        r#"SELECT s.id, s.account_id, s.text, s.spoiler_text, s.visibility,
                  -- `object.account.sensitized? || object.sensitive`
                  (s.sensitive OR a.sensitized_at IS NOT NULL) AS "sensitive!",
                  s.created_at, s.edited_at, s.uri, s.url, s.in_reply_to_id, s.language,
                  s.quote_approval_policy,
                  a.username, a.uri AS account_uri, a.id_scheme,
                  quoted_s.uri AS "quote_uri?",
                  qr.approval_uri AS "quote_authorization_uri?"
           FROM statuses s
           JOIN accounts a ON a.id = s.account_id
           -- Include pending (0) and accepted (1) quotes, not rejected/revoked:
           -- the `quote` field declares the quote relationship (needed even
           -- while pending, e.g. as a QuoteRequest's inlined instrument), while
           -- `quoteAuthorization` separately proves it once accepted.
           LEFT JOIN quotes qr ON qr.status_id = s.id AND qr.state IN (0, 1)
           LEFT JOIN statuses quoted_s ON quoted_s.id = qr.quoted_status_id AND quoted_s.deleted_at IS NULL
           WHERE s.id = $1 AND s.deleted_at IS NULL AND a.domain IS NULL
             AND s.reblog_of_id IS NULL"#,
        status_id,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(s) = s else { return Ok(None) };

    // Local author (the query enforces `a.domain IS NULL`): build the canonical
    // actor URI from its id_scheme rather than the (empty for imports) uri column.
    let actor_url =
        crate::federation::tag::account_uri(domain, s.account_id, s.id_scheme, &s.username);
    let note_uri = s
        .uri
        .clone()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| format!("{actor_url}/statuses/{}", s.id));
    let note_url = s
        .url
        .clone()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| note_uri.clone());
    let followers_url = format!("{actor_url}/followers");

    // ── inReplyTo ───────────────────────────────────────────────────────────
    let in_reply_to: Option<String> = if let Some(parent) = s.in_reply_to_id {
        sqlx::query_scalar!("SELECT uri FROM statuses WHERE id = $1", parent)
            .fetch_optional(&state.db)
            .await?
            .flatten()
            .filter(|u| !u.is_empty())
    } else {
        None
    };

    // A silenced author only addresses mentioned accounts who follow them (or
    // have a pending follow request) — Mastodon's TagManager narrowing.
    let author_silenced = sqlx::query_scalar!(
        r#"SELECT (silenced_at IS NOT NULL) AS "silenced!" FROM accounts WHERE id = $1"#,
        s.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);

    // ── Mentions (for tag + addressing) ─────────────────────────────────────
    let mention_rows = sqlx::query!(
        r#"SELECT a.id AS account_id, a.id_scheme, a.username, a.domain,
                  a.uri AS account_uri, a.url AS "url?", a.actor_type, a.followers_url,
                  EXISTS(SELECT 1 FROM follows f
                         WHERE f.account_id = a.id AND f.target_account_id = $2) AS "is_follower!",
                  EXISTS(SELECT 1 FROM follow_requests fr
                         WHERE fr.account_id = a.id AND fr.target_account_id = $2) AS "has_request!"
           FROM mentions m JOIN accounts a ON a.id = m.account_id
           WHERE m.status_id = $1"#,
        s.id,
        s.account_id,
    )
    .fetch_all(&state.db)
    .await?;

    let mut mention_map: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    // Addressing (to/cc) — narrowed for silenced authors and augmented with
    // group actors' followers collections.
    let mut mention_uris: Vec<String> = Vec::new();
    // The `tag` array lists every mention regardless of audience narrowing.
    let mut mention_tags: Vec<Value> = Vec::new();
    for m in &mention_rows {
        let href = if m.domain.is_none() {
            // A local mention's actor id follows from the account's id scheme;
            // `accounts.uri` is not where local accounts keep theirs.
            crate::federation::tag::account_uri(domain, m.account_id, m.id_scheme, &m.username)
        } else if let Some(uri) = m.account_uri.clone().filter(|u| !u.is_empty()) {
            uri
        } else if let Some(u) = m.url.clone().filter(|u| !u.is_empty()) {
            u
        } else {
            format!("https://{domain}/users/{}", m.username)
        };
        let acct = match &m.domain {
            Some(d) => format!("{}@{}", m.username, d),
            None => m.username.clone(),
        };
        // Keys used by render_content's linkifier (lowercase user / user@domain).
        let url_for_render = m
            .url
            .clone()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| href.clone());
        mention_map
            .entry(m.username.to_lowercase())
            .or_insert_with(|| (url_for_render.clone(), acct.clone()));
        if let Some(d) = &m.domain {
            mention_map
                .entry(format!(
                    "{}@{}",
                    m.username.to_lowercase(),
                    d.to_lowercase()
                ))
                .or_insert_with(|| (url_for_render.clone(), acct.clone()));
        }
        mention_tags.push(json!({
            "type": "Mention",
            "href": href.clone(),
            "name": format!("@{acct}"),
        }));

        if author_silenced && !(m.is_follower || m.has_request) {
            continue;
        }
        mention_uris.push(href);
        // Mastodon also addresses a group actor's followers collection.
        if m.actor_type.as_deref() == Some("Group") {
            let followers_uri = if m.domain.is_none() {
                Some(format!(
                    "{}/followers",
                    crate::federation::tag::account_uri(
                        domain,
                        m.account_id,
                        m.id_scheme,
                        &m.username
                    )
                ))
            } else if !m.followers_url.is_empty() {
                Some(m.followers_url.clone())
            } else {
                None
            };
            if let Some(f) = followers_uri {
                mention_uris.push(f);
            }
        }
    }

    // ── Hashtags ────────────────────────────────────────────────────────────
    let hashtag_rows = sqlx::query!(
        r#"SELECT t.name FROM statuses_tags st JOIN tags t ON t.id = st.tag_id
           WHERE st.status_id = $1 ORDER BY t.name"#,
        s.id,
    )
    .fetch_all(&state.db)
    .await?;
    let hashtag_tags: Vec<Value> = hashtag_rows
        .iter()
        .map(|t| {
            json!({
                "type": "Hashtag",
                "href": format!("https://{domain}/tags/{}", t.name),
                "name": format!("#{}", t.name),
            })
        })
        .collect();

    // ── Custom emoji (best effort: only those with a resolvable image) ───────
    let emoji_tags = emoji_tags_for(state, &s.text, &s.spoiler_text).await?;

    let mut tag: Vec<Value> = mention_tags;
    tag.extend(hashtag_tags);
    tag.extend(emoji_tags);

    // ── Media attachments ───────────────────────────────────────────────────
    let media = sqlx::query_as!(
        models::MediaAttachment,
        r#"SELECT m.* FROM media_attachments m
           JOIN LATERAL (
               SELECT COALESCE(array_position(s.ordered_media_attachment_ids, m.id), 2147483647) AS ord
               FROM statuses s WHERE s.id = $1
           ) o ON true
           WHERE m.status_id = $1
           ORDER BY o.ord, m.id"#,
        s.id,
    )
    .fetch_all(&state.db)
    .await?;
    let mut attachment: Vec<Value> = media
        .iter()
        .filter_map(|m| media_attachment_ap(&state.urls, m))
        .collect();

    // FEP-8967: the status's preview card travels as a `Link` attachment, so
    // receivers do not have to scrape the content for a URL and guess. Mastodon
    // 4.7.0 reads the first one it finds.
    if let Some(card_url) = sqlx::query_scalar!(
        r#"SELECT c.url
           FROM preview_cards c
           JOIN preview_cards_statuses cs ON cs.preview_card_id = c.id
           WHERE cs.status_id = $1
           ORDER BY c.id
           LIMIT 1"#,
        s.id,
    )
    .fetch_optional(&state.db)
    .await?
    {
        attachment.push(json!({ "type": "Link", "href": card_url }));
    }

    // ── Content + addressing ────────────────────────────────────────────────
    let content = render_content(&s.text, domain, &mention_map);
    let (to, cc) = vis::audience(s.visibility, &followers_url, &mention_uris);

    let mut note = json!({
        "id": note_uri,
        "type": "Note",
        "summary": if s.spoiler_text.is_empty() { None } else { Some(s.spoiler_text.clone()) },
        "inReplyTo": in_reply_to,
        "published": s.created_at.and_utc().to_rfc3339(),
        "url": note_url,
        "attributedTo": actor_url,
        "to": to.clone(),
        "cc": cc.clone(),
        "sensitive": s.sensitive,
        "content": content,
        "attachment": attachment,
        "tag": tag,
    });

    // Mastodon serializes statuses with polls as ActivityPub Questions. The
    // options expose tallies only once results are visible.
    if let Some(poll) = sqlx::query_as!(
        models::Poll,
        "SELECT * FROM polls WHERE status_id = $1",
        s.id,
    )
    .fetch_optional(&state.db)
    .await?
    {
        let expired = poll
            .expires_at
            .is_some_and(|t| t <= chrono::Utc::now().naive_utc());
        let show_totals = expired || !poll.hide_totals;
        let counts = sqlx::query!(
            "SELECT choice, COUNT(*)::bigint AS \"count!\" FROM poll_votes WHERE poll_id = $1 GROUP BY choice",
            poll.id,
        )
        .fetch_all(&state.db)
        .await?;
        let mut tallies = vec![0_i64; poll.options.len()];
        for row in counts {
            if let Some(slot) = tallies.get_mut(row.choice as usize) {
                *slot = row.count;
            }
        }
        let options: Vec<Value> = poll
            .options
            .iter()
            .enumerate()
            .map(|(idx, option)| {
                json!({
                    "type": "Note",
                    "name": option,
                    "replies": {
                        "type": "Collection",
                        "totalItems": if show_totals { json!(tallies[idx]) } else { Value::Null },
                    },
                })
            })
            .collect();

        note["type"] = json!("Question");
        if poll.multiple {
            note["anyOf"] = json!(options);
        } else {
            note["oneOf"] = json!(options);
        }
        if let Some(expires_at) = poll.expires_at {
            let timestamp = expires_at.and_utc().to_rfc3339();
            note["endTime"] = json!(timestamp);
            if expired {
                note["closed"] = json!(timestamp);
            }
        }
        if let Some(voters_count) = poll.voters_count {
            note["votersCount"] = json!(voters_count);
        }
    }

    if let Some(lang) = s.language.as_deref().filter(|l| !l.is_empty()) {
        note["contentMap"] = json!({ lang: note["content"].clone() });
    }
    if let Some(edited) = s.edited_at {
        note["updated"] = json!(edited.and_utc().to_rfc3339());
    }

    // FEP-044f quote linkage. `quote`/`quoteUrl` declare the quote relationship
    // (present from creation, even while pending); `quoteAuthorization` proves
    // it and is added only once the quoted author's server grants approval.
    if let Some(q) = s.quote_uri.clone().filter(|u| !u.is_empty()) {
        note["quote"] = json!(q);
        note["quoteUrl"] = json!(q);
        note["_misskey_quote"] = json!(q);
        if let Some(auth) = s.quote_authorization_uri.clone().filter(|u| !u.is_empty()) {
            note["quoteAuthorization"] = json!(auth);
        }
    }

    // Quote interaction policy advertisement.
    note["interactionPolicy"] = json!({
        "canQuote": quote_interaction_policy(
            s.quote_approval_policy,
            &followers_url,
            &format!("{actor_url}/following"),
            &actor_url,
        ),
    });

    Ok(Some(NoteBundle {
        note,
        actor_url,
        note_uri,
        to,
        cc,
        created_at: s.created_at.and_utc(),
    }))
}

/// `ActivityPub::NoteSerializer#interaction_policy`: outgoing posts carry the
/// automatic sub-policy only, and name the author alone when it allows no one.
fn quote_interaction_policy(
    policy: i32,
    followers_url: &str,
    following_url: &str,
    actor_url: &str,
) -> Value {
    use crate::db::models::quote_policy;
    const PUBLIC_URI: &str = "https://www.w3.org/ns/activitystreams#Public";
    let automatic = quote_policy::automatic(policy);
    let mut approved: Vec<String> = vec![];
    if automatic & quote_policy::PUBLIC != 0 {
        approved.push(PUBLIC_URI.to_string());
    }
    if automatic & quote_policy::FOLLOWERS != 0 {
        approved.push(followers_url.to_string());
    }
    if automatic & quote_policy::FOLLOWING != 0 {
        approved.push(following_url.to_string());
    }
    if approved.is_empty() {
        approved.push(actor_url.to_string());
    }
    json!({ "automaticApproval": approved })
}

/// Build an AP `attachment` entry for one media attachment, or `None` if it has
/// no resolvable URL.
fn media_attachment_ap(urls: &convert::InstanceUrls, m: &models::MediaAttachment) -> Option<Value> {
    let url = convert::media_url(urls, m)?;
    // Mastodon serializes every attachment as a generic `Document`; the concrete
    // kind is conveyed by `mediaType`.
    let mut obj = json!({
        "type": "Document",
        "url": url,
        "mediaType": m.file_content_type,
        "name": m.description,
        "blurhash": m.blurhash,
    });
    // Surface original width/height/duration when known (helps remote layout).
    if let Some(orig) = m.file_meta.as_ref().and_then(|v| v.get("original")) {
        if let Some(w) = orig.get("width").and_then(Value::as_i64) {
            obj["width"] = json!(w);
        }
        if let Some(h) = orig.get("height").and_then(Value::as_i64) {
            obj["height"] = json!(h);
        }
        if let Some(d) = orig.get("duration").and_then(Value::as_f64) {
            obj["duration"] = json!(d);
        }
    }
    // focalPoint [x, y] when a focus has been set (Mastodon's focal_point).
    if let Some(focus) = m.file_meta.as_ref().and_then(|v| v.get("focus")) {
        if let (Some(x), Some(y)) = (
            focus.get("x").and_then(Value::as_f64),
            focus.get("y").and_then(Value::as_f64),
        ) {
            obj["focalPoint"] = json!([x, y]);
        }
    }
    Some(obj)
}

/// Scan two text sources for `:shortcode:` tokens and emit `Emoji` tags for any
/// matching enabled local custom emoji that has a resolvable image URL. Used for
/// both status content (text + spoiler) and actor profiles (display name + note).
pub(crate) async fn emoji_tags_for(
    state: &AppState,
    text: &str,
    spoiler: &str,
) -> AppResult<Vec<Value>> {
    let mut shortcodes: Vec<String> = Vec::new();
    for src in [text, spoiler] {
        for cap in EMOJI_RE.captures_iter(src) {
            let sc = cap[1].to_string();
            if !shortcodes.contains(&sc) {
                shortcodes.push(sc);
            }
        }
    }
    if shortcodes.is_empty() {
        return Ok(vec![]);
    }

    let rows = sqlx::query!(
        r#"SELECT shortcode, image_remote_url, uri, updated_at
           FROM custom_emojis
           WHERE domain IS NULL AND disabled = false AND shortcode = ANY($1)"#,
        &shortcodes,
    )
    .fetch_all(&state.db)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let image = r.image_remote_url.filter(|u| !u.is_empty())?;
            Some(json!({
                "type": "Emoji",
                "id": r.uri,
                "name": format!(":{}:", r.shortcode),
                "updated": r.updated_at.and_utc().to_rfc3339(),
                "icon": { "type": "Image", "url": image },
            }))
        })
        .collect())
}

static EMOJI_RE: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| regex::Regex::new(r":([a-zA-Z0-9_]+):").unwrap());
