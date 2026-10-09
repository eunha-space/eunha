//! Shared ActivityPub `Note` construction for locally-authored statuses.
//!
//! Both the outbox and the per-status dereferencing endpoint build their `Note`
//! objects here so the wire representation (content, media `attachment`, and the
//! `tag` array of mentions/hashtags/custom emoji) stays identical regardless of
//! how a remote server discovers the post.

use serde_json::{json, Value};

use crate::api::mastodon::convert;
use crate::db::models::{self, vis};
use crate::formatter::{self, MentionTarget};
use crate::{error::AppResult, state::AppState};

/// JSON-LD `@context` for a `Create(Note)` / `Note` we serve. Declares the Toot,
/// hashtag/emoji, and FEP-044f quote terms used below.
pub fn note_context() -> Value {
    json!([
        "https://www.w3.org/ns/activitystreams",
        {
            // Mastodon's `atom_uri` and `conversation` context extensions.
            "ostatus": "http://ostatus.org#",
            "atomUri": "ostatus:atomUri",
            "inReplyToAtomUri": "ostatus:inReplyToAtomUri",
            "conversation": "ostatus:conversation",
            "sensitive": "as:sensitive",
            "toot": "http://joinmastodon.org/ns#",
            "votersCount": "toot:votersCount",
            "blurhash": "toot:blurhash",
            "Hashtag": "as:Hashtag",
            "Emoji": "toot:Emoji",
            "focalPoint": { "@container": "@list", "@id": "toot:focalPoint" },
            "fep": "https://w3id.org/fep/044f#",
            // Mastodon's `quotes` context extension.
            "quote": { "@id": "fep:quote", "@type": "@id" },
            "quoteUri": "http://fedibird.com/ns#quoteUri",
            "_misskey_quote": "https://misskey-hub.net/ns#_misskey_quote",
            "quoteAuthorization": { "@id": "fep:quoteAuthorization", "@type": "@id" },
            // FEP-7888 / GoToSocial interaction policy terms, so the
            // `interactionPolicy` we emit below survives JSON-LD expansion.
            "gts": "https://gotosocial.org/ns#",
            "interactionPolicy": { "@id": "gts:interactionPolicy", "@type": "@id" },
            // Mastodon's `interaction_policies` extension names FEP-7aa9's
            // `canFeature` alongside `canQuote`.
            "canFeature": { "@id": "https://w3id.org/fep/7aa9#canFeature", "@type": "@id" },
            "canQuote": { "@id": "gts:canQuote", "@type": "@id" },
            "automaticApproval": { "@id": "gts:automaticApproval", "@type": "@id" },
            "manualApproval": { "@id": "gts:manualApproval", "@type": "@id" },
        }
    ])
}

/// `Time#iso8601`, as Mastodon's serializers write a timestamp: whole
/// seconds, in UTC, with a `Z`.
#[must_use]
pub fn iso8601(time: chrono::DateTime<chrono::Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// `ActiveSupport::Duration#iso8601` of `n.seconds`, as
/// `MediaAttachmentSerializer#duration` writes a length: the seconds as
/// Ruby prints the number (`12.0`, `75.5`, `12`), never carried into
/// minutes.
fn iso8601_seconds(seconds: &Value) -> Option<String> {
    let text = if let Some(n) = seconds.as_i64() {
        if n == 0 {
            return Some("PT0S".into());
        }
        n.to_string()
    } else {
        let n = seconds.as_f64()?;
        if n == 0.0 {
            return Some("PT0S".into());
        }
        format!("{n:?}")
    };
    Some(format!("PT{text}S"))
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
            "published": iso8601(self.created_at),
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
                  s.created_at, s.edited_at, s.uri, s.in_reply_to_id, s.language,
                  s.quote_approval_policy,
                  a.username, a.uri AS account_uri, a.id_scheme,
                  qr.id AS "quote_id?",
                  quoted_s.uri AS "quote_uri?"
           FROM statuses s
           JOIN accounts a ON a.id = s.account_id
           -- `quote?`: a quote in whatever state. A quoted post that is gone
           -- is a Tombstone, and the stamp is named as
           -- `TagManager#approval_uri_for` names it.
           LEFT JOIN quotes qr ON qr.status_id = s.id
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
    // `TagManager#url_for` on a local status: `short_account_status_url`,
    // whatever the `url` column holds.
    let note_url = format!("https://{domain}/@{}/{}", s.username, s.id);
    let followers_url = format!("{actor_url}/followers");

    // ── inReplyTo ───────────────────────────────────────────────────────────
    // `NoteSerializer#in_reply_to`: none once the post replied to is
    // discarded (`Status`'s `default_scope` keeps only the kept ones); a URI
    // that is not HTTP gives way to the post's `url`; otherwise
    // `TagManager#uri_for`, which names a local post by its account's
    // scheme, whatever its `uri` holds.
    let in_reply_to: Option<String> = match s.in_reply_to_id {
        None => None,
        Some(parent) => sqlx::query!(
            r#"SELECT t.id, t.uri, t.url, t.reblog_of_id,
                      (COALESCE(t.local, false) OR t.uri IS NULL) AS "local!",
                      a.id AS account_id, a.id_scheme, a.username
               FROM statuses t JOIN accounts a ON a.id = t.account_id
               WHERE t.id = $1 AND t.deleted_at IS NULL"#,
            parent,
        )
        .fetch_optional(&state.db)
        .await?
        .and_then(|thread| match thread.uri {
            Some(uri) if !uri.starts_with("http") => thread.url,
            _ if thread.local => {
                let uri = crate::federation::tag::status_uri(
                    domain,
                    thread.account_id,
                    thread.id_scheme,
                    &thread.username,
                    thread.id,
                );
                Some(if thread.reblog_of_id.is_some() {
                    format!("{uri}/activity")
                } else {
                    uri
                })
            }
            uri => uri,
        }),
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
           -- `active_mentions`: a silent one is neither addressed nor tagged.
           -- `virtual_tags` sorts them by id.
           WHERE m.status_id = $1 AND NOT m.silent
           ORDER BY m.id"#,
        s.id,
        s.account_id,
    )
    .fetch_all(&state.db)
    .await?;

    // `preloaded_accounts` for the content: the author, then the mentioned.
    let mut preloaded = vec![MentionTarget {
        username: s.username.clone(),
        domain: None,
        url: format!("https://{domain}/@{}", s.username),
    }];
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
        preloaded.push(MentionTarget {
            username: m.username.clone(),
            domain: m.domain.clone(),
            url: match &m.domain {
                None => format!("https://{domain}/@{}", m.username),
                Some(_) => m.url.clone().unwrap_or_default(),
            },
        });
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
    // `object.tags`, in no order of Mastodon's choosing: the habtm query has
    // no `ORDER BY`, and neither does this one.
    let hashtag_rows = sqlx::query!(
        r#"SELECT t.name FROM statuses_tags st JOIN tags t ON t.id = st.tag_id
           WHERE st.status_id = $1"#,
        s.id,
    )
    .fetch_all(&state.db)
    .await?;
    let hashtag_tags: Vec<Value> = hashtag_rows
        .iter()
        .map(|t| {
            json!({
                "type": "Hashtag",
                "href": crate::formatter::text::tag_url(domain, &t.name),
                "name": format!("#{}", t.name),
            })
        })
        .collect();

    // ── Custom emoji: `Status#emojis`, the poll's options included ─────────
    let poll_options: Vec<String> =
        sqlx::query_scalar!("SELECT options FROM polls WHERE status_id = $1", s.id)
            .fetch_optional(&state.db)
            .await?
            .unwrap_or_default();
    let mut emojifiable: Vec<&str> = vec![&s.spoiler_text, &s.text];
    emojifiable.extend(poll_options.iter().map(String::as_str));
    let emoji_tags = emoji_tags_for(state, &emojifiable).await?;

    let mut tag: Vec<Value> = mention_tags;
    tag.extend(hashtag_tags);
    tag.extend(emoji_tags);

    // ── Media attachments ───────────────────────────────────────────────────
    // `object.ordered_media_attachments`.
    let media = crate::api::mastodon::status_serialize::fetch_status_media(state, s.id).await?;
    let mut attachment: Vec<Value> = media
        .iter()
        .filter_map(|m| media_attachment_ap(&state.urls, m))
        .collect();

    // FEP-8967: the status's preview card travels as a `Link` attachment, so
    // receivers do not have to scrape the content for a URL and guess. Mastodon
    // 4.7.0 reads the first one it finds.
    // `href` is `original_url.presence || url`: the link as the post gave it.
    if let Some(card_url) = sqlx::query_scalar!(
        r#"SELECT COALESCE(NULLIF(btrim(cs.url), ''), c.url) AS "url!"
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
    // `status_content_format`, with the quote fallback for the quoted post.
    let quote_url = crate::api::mastodon::formatting::quote_fallback_urls(state, &[s.id])
        .await
        .remove(&s.id);
    let content = formatter::html_aware(
        &s.text,
        true,
        &formatter::Options {
            preloaded_accounts: &preloaded,
            quoted_status_url: quote_url.as_deref(),
            ..formatter::Options::new(domain)
        },
    );
    let (to, cc) = vis::audience(s.visibility, &followers_url, &mention_uris);

    let mut note = json!({
        "id": note_uri,
        "type": "Note",
        "summary": if s.spoiler_text.is_empty() { None } else { Some(s.spoiler_text.clone()) },
        "inReplyTo": in_reply_to,
        "published": iso8601(s.created_at.and_utc()),
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
        // `loaded_options`: the tallies the poll keeps.
        let tallies: Vec<i64> = (0..poll.options.len())
            .map(|i| poll.cached_tallies.get(i).copied().unwrap_or(0))
            .collect();
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
            let timestamp = iso8601(expires_at.and_utc());
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
        note["updated"] = json!(iso8601(edited.and_utc()));
    }

    // FEP-044f quote linkage, as `ActivityPub::NoteSerializer` writes it:
    // `quote` whatever the quote's state (a Tombstone for a quoted post that
    // is gone), `_misskey_quote` and `quoteUri` for one that is there, and
    // `quoteAuthorization` once there is a stamp to name.
    if let Some(quote_id) = s.quote_id {
        match s.quote_uri.clone().filter(|u| !u.is_empty()) {
            Some(q) => {
                note["quote"] = json!(q);
                note["_misskey_quote"] = json!(q);
                note["quoteUri"] = json!(q);
            }
            None => note["quote"] = json!({ "type": "Tombstone" }),
        }
        if let Some(quote) = crate::quotes::find(&state.db, quote_id).await? {
            if let Some(stamp) = crate::quotes::approval_uri_for(state, &quote, true).await? {
                note["quoteAuthorization"] = json!(stamp);
            }
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

    // `atomUri`, `inReplyToAtomUri`, `conversation`, `context`, and the
    // post's `replies`, `likes` and `shares`, which eunha serves.
    for (key, value) in serializer_extras(state, domain, status_id).await? {
        note[key] = value;
    }

    Ok(Some(NoteBundle {
        note,
        actor_url,
        note_uri,
        to,
        cc,
        created_at: s.created_at.and_utc(),
    }))
}

/// `OStatus::TagManager#unique_tag`.
fn unique_tag(domain: &str, date: chrono::NaiveDateTime, id: i64, kind: &str) -> String {
    format!(
        "tag:{domain},{}:objectId={id}:objectType={kind}",
        date.format("%Y-%m-%d")
    )
}

/// The members of `ActivityPub::NoteSerializer` that name where the post
/// is in its thread and the collections eunha serves of it: `atomUri`,
/// `inReplyToAtomUri`, `conversation`, `context`, and a local post's
/// `replies`, `likes` and `shares`.
pub async fn serializer_extras(
    state: &AppState,
    domain: &str,
    status_id: i64,
) -> AppResult<serde_json::Map<String, Value>> {
    let mut extras = serde_json::Map::new();
    let Some(s) = sqlx::query!(
        r#"SELECT s.id, s.uri, s.created_at, s.account_id, a.id_scheme, a.username,
                  t.id AS "thread_id?", t.uri AS thread_uri, t.created_at AS "thread_created_at?",
                  c.id AS "conversation_id?", c.uri AS conversation_uri,
                  c.created_at AS "conversation_created_at?",
                  c.parent_account_id, c.parent_status_id,
                  COALESCE(GREATEST(st.favourites_count, 0), 0) AS "favourites_count!",
                  COALESCE(GREATEST(st.reblogs_count, 0), 0) AS "reblogs_count!"
           FROM statuses s
           JOIN accounts a ON a.id = s.account_id
           LEFT JOIN statuses t ON t.id = s.in_reply_to_id AND t.deleted_at IS NULL
           LEFT JOIN conversations c ON c.id = s.conversation_id
           LEFT JOIN status_stats st ON st.status_id = s.id
           WHERE s.id = $1"#,
        status_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(extras);
    };

    // `atom_uri`: `OStatus::TagManager#uri_for` on a local post.
    let atom_uri = s
        .uri
        .clone()
        .unwrap_or_else(|| unique_tag(domain, s.created_at, s.id, "Status"));
    extras.insert("atomUri".into(), json!(atom_uri));
    // A post without a `uri` is a local one, named by its tag.
    let in_reply_to_atom_uri = s.thread_id.map(|thread_id| {
        s.thread_uri.clone().unwrap_or_else(|| {
            unique_tag(
                domain,
                s.thread_created_at.unwrap_or_default(),
                thread_id,
                "Status",
            )
        })
    });
    extras.insert("inReplyToAtomUri".into(), json!(in_reply_to_atom_uri));

    // `conversation` and `context`. A conversation of our own (no `uri`) is
    // named by its context URL once it knows its parent post.
    let context_url = match (s.parent_account_id, s.parent_status_id) {
        (Some(account), Some(status)) => {
            Some(format!("https://{domain}/contexts/{account}-{status}"))
        }
        _ => None,
    };
    let conversation_uri = s.conversation_uri.clone().filter(|u| !u.trim().is_empty());
    let (conversation, context) = match s.conversation_id {
        None => (None, None),
        Some(id) => {
            let conversation = conversation_uri
                .clone()
                .or_else(|| context_url.clone())
                .or_else(|| {
                    s.conversation_created_at
                        .map(|at| unique_tag(domain, at, id, "Conversation"))
                });
            // `uri_for(conversation)`: a remote one's `uri`, a local one's
            // context URL; `unsupported_uri_scheme?` keeps out anything
            // that is not HTTP.
            let context = match s.conversation_uri.clone() {
                Some(uri) => Some(uri),
                None => context_url,
            }
            .filter(|uri| uri.starts_with("http://") || uri.starts_with("https://"));
            (conversation, context)
        }
    };
    extras.insert("conversation".into(), json!(conversation));
    extras.insert("context".into(), json!(context));

    // `replies`, `likes` and `shares`, the collections of a local post.
    let note_uri =
        crate::federation::tag::status_uri(domain, s.account_id, s.id_scheme, &s.username, s.id);
    let replies_uri = format!("{note_uri}/replies");
    // `self_replies(5)`: the author's own public and unlisted answers.
    let replies = sqlx::query!(
        r#"SELECT r.id, r.uri FROM statuses r
           WHERE r.account_id = $1 AND r.in_reply_to_id = $2 AND r.deleted_at IS NULL
             AND r.visibility IN (0, 1) /* vis::PUBLIC, vis::UNLISTED */
           ORDER BY r.id LIMIT 5"#,
        s.account_id,
        s.id,
    )
    .fetch_all(&state.db)
    .await?;
    let next = match replies.last() {
        Some(last) => format!("{replies_uri}?min_id={}&page=true", last.id),
        None => format!("{replies_uri}?only_other_accounts=true&page=true"),
    };
    let items: Vec<Value> = replies
        .iter()
        .map(|r| {
            json!(r
                .uri
                .clone()
                .unwrap_or_else(|| crate::federation::tag::status_uri(
                    domain,
                    s.account_id,
                    s.id_scheme,
                    &s.username,
                    r.id
                )))
        })
        .collect();
    extras.insert(
        "replies".into(),
        json!({
            "id": replies_uri,
            "type": "Collection",
            "first": {
                "type": "CollectionPage",
                "next": next,
                "partOf": replies_uri,
                "items": items,
            },
        }),
    );
    extras.insert(
        "likes".into(),
        json!({
            "id": format!("{note_uri}/likes"),
            "type": "Collection",
            "totalItems": s.favourites_count,
        }),
    );
    extras.insert(
        "shares".into(),
        json!({
            "id": format!("{note_uri}/shares"),
            "type": "Collection",
            "totalItems": s.reblogs_count,
        }),
    );
    Ok(extras)
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
        if let Some(d) = orig.get("duration").and_then(iso8601_seconds) {
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
    // `has_one :icon, if: :thumbnail?`: a custom thumbnail, through
    // `ImageSerializer`.
    if let Some(icon_url) = convert::media_thumbnail_original_url(urls, m) {
        obj["icon"] = json!({
            "type": "Image",
            "mediaType": m.thumbnail_content_type,
            "url": icon_url,
        });
    }
    Some(obj)
}

/// `CustomEmoji.from_text` on `fields.join(' ')` for a local author: an
/// `Emoji` tag for each enabled local custom emoji whose `:shortcode:` the
/// text names (`CustomEmoji::SCAN_RE`), in the order they are first named.
/// `Status#emojis` gives it the content warning, the text and the poll's
/// options; `Account#emojis` the profile's `emojifiable_text`.
pub(crate) async fn emoji_tags_for(state: &AppState, fields: &[&str]) -> AppResult<Vec<Value>> {
    let shortcodes = convert::scan_emoji_shortcodes(&fields.join(" "));
    if shortcodes.is_empty() {
        return Ok(vec![]);
    }

    let rows = sqlx::query!(
        r#"SELECT id, shortcode, image_file_name, image_content_type,
                  image_storage_schema_version, updated_at
           FROM custom_emojis
           WHERE domain IS NULL AND disabled = false AND shortcode = ANY($1)
           ORDER BY array_position($1, shortcode::text)"#,
        &shortcodes,
    )
    .fetch_all(&state.db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            emoji_document(
                state,
                r.id,
                &r.shortcode,
                r.updated_at,
                crate::custom_emoji::ImageRef {
                    id: r.id,
                    domain: None,
                    image_file_name: r.image_file_name.as_deref(),
                    image_remote_url: None,
                    image_storage_schema_version: r.image_storage_schema_version,
                },
                r.image_content_type.as_deref(),
            )
        })
        .collect())
}

/// The document `/emojis/:id` serves: `CustomEmoji.local.find(id)` through
/// `ActivityPub::EmojiSerializer` and the adapter's `@context`.
pub(crate) async fn emoji_object(state: &AppState, id: i64) -> AppResult<Value> {
    let row = sqlx::query!(
        r#"SELECT id, shortcode, image_file_name, image_content_type,
                  image_storage_schema_version, updated_at
           FROM custom_emojis WHERE id = $1 AND domain IS NULL"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(crate::error::AppError::NotFound)?;
    let mut document = emoji_document(
        state,
        row.id,
        &row.shortcode,
        row.updated_at,
        crate::custom_emoji::ImageRef {
            id: row.id,
            domain: None,
            image_file_name: row.image_file_name.as_deref(),
            image_remote_url: None,
            image_storage_schema_version: row.image_storage_schema_version,
        },
        row.image_content_type.as_deref(),
    );
    let mut with_context = serde_json::Map::new();
    with_context.insert(
        "@context".into(),
        super::context_helper::serialized_context(&["activitystreams"], &["emoji", "focal_point"]),
    );
    if let Value::Object(fields) = document.take() {
        with_context.extend(fields);
    }
    Ok(Value::Object(with_context))
}

/// `ActivityPub::EmojiSerializer` for a local emoji, without the adapter's
/// `@context`: its id is `emoji_url`, and its icon the original image, as
/// `ActivityPub::ImageSerializer` writes one.
pub(crate) fn emoji_document(
    state: &AppState,
    id: i64,
    shortcode: &str,
    updated_at: chrono::NaiveDateTime,
    image: crate::custom_emoji::ImageRef<'_>,
    content_type: Option<&str>,
) -> Value {
    let domain = &state.instance.domain;
    json!({
        "id": format!("https://{domain}/emojis/{id}"),
        "type": "Emoji",
        "name": format!(":{shortcode}:"),
        "updated": iso8601(updated_at.and_utc()),
        "icon": {
            "type": "Image",
            "mediaType": content_type,
            "url": image.url(&state.storage, domain, "original"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_are_written_as_active_support_writes_them() {
        assert_eq!(iso8601_seconds(&json!(12.0)).as_deref(), Some("PT12.0S"));
        assert_eq!(iso8601_seconds(&json!(75.5)).as_deref(), Some("PT75.5S"));
        assert_eq!(iso8601_seconds(&json!(12)).as_deref(), Some("PT12S"));
        assert_eq!(iso8601_seconds(&json!(0.0)).as_deref(), Some("PT0S"));
        assert_eq!(iso8601_seconds(&json!("x")), None);
    }
}
