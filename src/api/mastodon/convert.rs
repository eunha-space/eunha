use super::types;
use crate::db::models;
/// Conversions from DB models → Mastodon API types.
/// What an instance's URLs are built from: its domain, and where its media is
/// served. Every [`AppState`](crate::state::AppState) carries its own.
///
/// These were process-wide `OnceLock`s, set by whichever `AppState` was built
/// first, so a second instance in the same process silently served the first
/// one's domain and media URLs.
#[derive(Debug, Clone)]
pub struct InstanceUrls {
    /// The domain local accounts' and statuses' URLs are built on.
    pub local_domain: String,
    /// The public URL media paths hang off, without a trailing slash.
    media_base: String,
    missing_avatar: String,
    missing_header: String,
}

impl InstanceUrls {
    /// `missing_avatar` and `missing_header` are the storage's public URLs for
    /// the default images; the media base is read off the avatar's.
    pub fn new(local_domain: String, missing_avatar: String, missing_header: String) -> Self {
        let media_base = missing_avatar
            .strip_suffix("/avatars/original/missing.png")
            .unwrap_or_default()
            .to_string();
        Self {
            local_domain,
            media_base,
            missing_avatar,
            missing_header,
        }
    }

    /// Paperclip's `default_url` for a media attachment with no file
    /// (`/files/:style/missing.png`), as `full_asset_url` makes it.
    pub fn missing_file_url(&self, style: &str) -> String {
        format!("{}/files/{style}/missing.png", self.media_base)
    }

    fn missing_avatar(&self) -> &str {
        &self.missing_avatar
    }

    fn missing_header(&self) -> &str {
        &self.missing_header
    }
}

pub fn account_avatar_url_for(urls: &InstanceUrls, a: &models::Account) -> String {
    account_avatar_url(urls, a)
}

pub fn account_header_url_for(urls: &InstanceUrls, a: &models::Account) -> String {
    account_header_url(urls, a)
}

fn account_avatar_url(urls: &InstanceUrls, a: &models::Account) -> String {
    account_avatar_url_parts(
        urls,
        a.id,
        a.avatar_file_name.as_deref(),
        a.avatar_remote_url.as_deref(),
    )
}

/// Avatar URL from the minimal columns, for bulk queries that don't hydrate a
/// full [`models::Account`] (e.g. the invite tree).
pub fn account_avatar_url_parts(
    urls: &InstanceUrls,
    id: i64,
    avatar_file_name: Option<&str>,
    avatar_remote_url: Option<&str>,
) -> String {
    if let Some(url) = avatar_remote_url {
        if !url.is_empty() {
            return url.to_string();
        }
    }
    if let Some(filename) = avatar_file_name {
        if !filename.is_empty() {
            return format!(
                "{}/accounts/avatars/{}/original/{}",
                urls.media_base,
                crate::media::int_to_path(id),
                filename
            );
        }
    }
    urls.missing_avatar().to_string()
}

fn account_header_url(urls: &InstanceUrls, a: &models::Account) -> String {
    if !a.header_remote_url.is_empty() {
        return a.header_remote_url.clone();
    }
    if let Some(filename) = &a.header_file_name {
        if !filename.is_empty() {
            return format!(
                "{}/accounts/headers/{}/original/{}",
                urls.media_base,
                crate::media::int_to_path(a.id),
                filename
            );
        }
    }
    urls.missing_header().to_string()
}

pub fn media_url(urls: &InstanceUrls, m: &models::MediaAttachment) -> Option<String> {
    if let Some(filename) = &m.file_file_name {
        if !filename.is_empty() {
            return Some(format!(
                "{}/media_attachments/files/{}/original/{}",
                urls.media_base,
                crate::media::int_to_path(m.id),
                filename
            ));
        }
    }
    m.remote_url
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `thumbnail.url(:original)`: a custom thumbnail's own file, which
/// `ActivityPub::NoteSerializer` gives as an attachment's `icon`.
pub fn media_thumbnail_original_url(
    urls: &InstanceUrls,
    m: &models::MediaAttachment,
) -> Option<String> {
    let filename = m.thumbnail_file_name.as_deref().filter(|f| !f.is_empty())?;
    Some(format!(
        "{}/media_attachments/thumbnails/{}/original/{filename}",
        urls.media_base,
        crate::media::int_to_path(m.id)
    ))
}

pub fn media_preview_url(urls: &InstanceUrls, m: &models::MediaAttachment) -> Option<String> {
    if let Some(filename) = &m.thumbnail_file_name {
        if !filename.is_empty() {
            return Some(format!(
                "{}/media_attachments/files/{}/small/{}",
                urls.media_base,
                crate::media::int_to_path(m.id),
                filename
            ));
        }
    }
    if let Some(filename) = &m.file_file_name {
        if !filename.is_empty() {
            return Some(format!(
                "{}/media_attachments/files/{}/small/{}",
                urls.media_base,
                crate::media::int_to_path(m.id),
                filename
            ));
        }
    }
    m.thumbnail_remote_url
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `CustomEmoji::SCAN_RE`: the `:shortcode:`s in `text`, each once, in the
/// order they first appear.
pub fn scan_emoji_shortcodes(text: &str) -> Vec<String> {
    static SCAN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r":([a-zA-Z0-9_]{2,}):").expect("valid shortcode pattern")
    });
    let alnum_or_colon = |c: char| c.is_alphanumeric() || c == ':';
    let mut seen = std::collections::HashSet::new();
    let mut codes = Vec::new();
    let mut at = 0;
    while let Some(m) = SCAN.find_at(text, at) {
        let before = text[..m.start()].chars().next_back();
        let after = text[m.end()..].chars().next();
        // `(?<=[^[:alnum:]:]|\n|^)` and `(?=[^[:alnum:]:]|$)`.
        if before.is_none_or(|c| !alnum_or_colon(c)) && after.is_none_or(|c| !alnum_or_colon(c)) {
            let code = &text[m.start() + 1..m.end() - 1];
            if seen.insert(code.to_owned()) {
                codes.push(code.to_owned());
            }
            at = m.end();
        } else {
            at = m.start() + 1;
        }
    }
    codes
}

/// A `custom_emojis` row, as much of it as the entity needs.
pub struct EmojiRow {
    pub id: i64,
    pub shortcode: String,
    pub domain: Option<String>,
    pub image_file_name: Option<String>,
    pub image_remote_url: Option<String>,
    pub image_storage_schema_version: Option<i32>,
    pub visible_in_picker: bool,
}

impl EmojiRow {
    pub fn image(&self) -> crate::custom_emoji::ImageRef<'_> {
        crate::custom_emoji::ImageRef {
            id: self.id,
            domain: self.domain.as_deref(),
            image_file_name: self.image_file_name.as_deref(),
            image_remote_url: self.image_remote_url.as_deref(),
            image_storage_schema_version: self.image_storage_schema_version,
        }
    }
}

/// `REST::CustomEmojiSerializer` for `row`: a local emoji's `original` and
/// `static` styles where Paperclip keeps them, and a remote one's URL as it
/// was stored, since eunha keeps no copy of a remote emoji's image
/// (`remote-account-images-not-downloaded`). `category` and `featured` are
/// there when the category was loaded and the emoji has one — the
/// category's name and its `featured_emoji_id`.
pub fn custom_emoji_entity(
    state: &crate::state::AppState,
    row: &EmojiRow,
    category: Option<(String, Option<i64>)>,
) -> types::CustomEmoji {
    let image = row.image();
    let domain = &state.instance.domain;
    let (category, featured) = match category {
        Some((name, featured_id)) => (Some(name), Some(featured_id == Some(row.id))),
        None => (None, None),
    };
    types::CustomEmoji {
        shortcode: row.shortcode.clone(),
        url: image.url(&state.storage, domain, "original"),
        static_url: image.url(&state.storage, domain, "static"),
        visible_in_picker: row.visible_in_picker,
        category,
        featured,
    }
}

/// `CustomEmoji.from_text` for many texts at once: each text's emojis, the
/// enabled ones its owner's domain has under the shortcodes the text uses,
/// keyed by the caller's key.
pub async fn emojis_from_texts<K: std::hash::Hash + Eq + Clone>(
    state: &crate::state::AppState,
    texts: &[(K, Option<String>, String)],
) -> std::collections::HashMap<K, Vec<types::CustomEmoji>> {
    let mut wanted: Vec<(K, Option<String>, Vec<String>)> = Vec::new();
    let mut codes: Vec<String> = Vec::new();
    let mut domains: Vec<String> = Vec::new();
    for (key, domain, text) in texts {
        let found = scan_emoji_shortcodes(text);
        for code in &found {
            codes.push(code.clone());
            domains.push(domain.clone().unwrap_or_default());
        }
        if !found.is_empty() {
            wanted.push((key.clone(), domain.clone(), found));
        }
    }
    let mut result = std::collections::HashMap::new();
    if codes.is_empty() {
        return result;
    }
    let rows = sqlx::query_as!(
        EmojiRow,
        r#"SELECT DISTINCT ce.id, ce.shortcode, ce.domain, ce.image_file_name, ce.image_remote_url,
                  ce.image_storage_schema_version, ce.visible_in_picker
           FROM custom_emojis ce
           JOIN unnest($1::text[], $2::text[]) AS q(shortcode, domain)
             ON ce.shortcode = q.shortcode
            AND ce.domain IS NOT DISTINCT FROM NULLIF(q.domain, '')
           WHERE NOT ce.disabled"#,
        &codes,
        &domains,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let by_key: std::collections::HashMap<(Option<&str>, &str), &EmojiRow> = rows
        .iter()
        .map(|r| ((r.domain.as_deref(), r.shortcode.as_str()), r))
        .collect();
    for (key, domain, found) in wanted {
        let emojis: Vec<types::CustomEmoji> = found
            .iter()
            .filter_map(|code| by_key.get(&(domain.as_deref(), code.as_str())))
            .map(|row| custom_emoji_entity(state, row, None))
            .collect();
        if !emojis.is_empty() {
            result.insert(key, emojis);
        }
    }
    result
}

pub trait MastodonTimestamp {
    fn format_mastodon(self) -> String;
}

impl MastodonTimestamp for chrono::NaiveDateTime {
    fn format_mastodon(self) -> String {
        self.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
    }
}

impl MastodonTimestamp for chrono::DateTime<chrono::Utc> {
    fn format_mastodon(self) -> String {
        self.naive_utc().format_mastodon()
    }
}

/// Format a timestamp in the Mastodon-standard format: `YYYY-MM-DDTHH:MM:SS.mmmZ`.
/// Mastodon stores UTC timestamps without time zones and serializes them with a `Z` suffix.
pub fn mastodon_date<T: MastodonTimestamp>(t: T) -> String {
    t.format_mastodon()
}

fn status_url_from_uri(uri: &str) -> Option<String> {
    let (base, rest) = uri.split_once("/users/")?;
    let (username, id) = rest.split_once("/statuses/")?;
    Some(format!("{}/@{}/{}", base, username, id))
}

/// Render an account bio to the HTML the API's top-level `note` field serves,
/// Mastodon's `account_bio_format` (`html_aware_format(note, local?)`): a local
/// bio is stored as plain text and goes through `TextFormatter`, a remote one
/// arrived as HTML and is sanitized with `MASTODON_STRICT`. Mentions in a
/// local bio are left unlinked here, where there is no database to look them
/// up in; [`super::formatting::link_profile_mentions`] links them.
fn render_account_note(urls: &InstanceUrls, a: &models::Account) -> String {
    crate::formatter::html_aware(
        &a.note,
        a.domain.is_none(),
        &crate::formatter::Options::new(&urls.local_domain),
    )
}

/// What a viewing account knows about its relationship to the account being
/// serialized, which is what `feature_approval.current_user` turns on.
#[derive(Debug, Clone, Copy, Default)]
pub struct AccountViewerContext {
    /// The viewer is the account being serialized.
    pub is_self: bool,
    /// The account being serialized follows the viewer.
    pub follows_viewer: bool,
    /// The viewer follows the account being serialized.
    pub followed_by_viewer: bool,
}

/// Mastodon's `feature_approval`, from `Account::InteractionPolicyConcern`.
///
/// A local account's policy is implied by its own settings rather than stored:
/// an undiscoverable account allows nobody, a locked one allows its followers,
/// and any other allows everyone — and Mastodon never offers local accounts the
/// manual path. A remote account's comes from the bitmap it federated.
fn build_feature_approval(
    a: &models::Account,
    viewer: Option<&AccountViewerContext>,
) -> types::FeatureApproval {
    use crate::db::models::feature_policy;

    let local = a.domain.is_none();
    let discoverable = a.discoverable.unwrap_or(false);

    let (automatic, manual): (Vec<String>, Vec<String>) = if local {
        let automatic = if !discoverable {
            vec![]
        } else if a.locked {
            vec!["followers".to_string()]
        } else {
            vec!["public".to_string()]
        };
        (automatic, vec![])
    } else {
        let policy = a.feature_approval_policy;
        (
            feature_policy::as_keys(feature_policy::automatic(policy))
                .into_iter()
                .map(str::to_owned)
                .collect(),
            feature_policy::as_keys(feature_policy::manual(policy))
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
    };

    let current_user = match viewer {
        // Mastodon answers `denied` when nobody is asking.
        None => "denied".to_string(),
        Some(ctx) if local => {
            // Two ways to be refused, and they give the same answer: an account
            // nobody can discover, or a locked one being read by someone who
            // neither follows it nor is it.
            let refused = !discoverable || (a.locked && !ctx.follows_viewer && !ctx.is_self);
            if refused { "denied" } else { "automatic" }.to_string()
        }
        Some(ctx) => {
            let policy = a.feature_approval_policy;
            let automatic_policy = feature_policy::automatic(policy);
            let manual_policy = feature_policy::manual(policy);
            let allows = |sub_policy: i32| {
                sub_policy & feature_policy::PUBLIC != 0
                    || (sub_policy & feature_policy::FOLLOWERS != 0 && ctx.follows_viewer)
                    || (sub_policy & feature_policy::FOLLOWING != 0 && ctx.followed_by_viewer)
            };

            if ctx.is_self {
                // An author may always feature themselves.
                "automatic".to_string()
            } else if policy == 0 {
                // Nothing federated yet, so nothing can be said.
                "missing".to_string()
            } else if allows(automatic_policy) {
                "automatic".to_string()
            } else if allows(manual_policy) {
                "manual".to_string()
            } else if (automatic_policy | manual_policy) & feature_policy::UNSUPPORTED != 0 {
                // A flag from a newer or different implementation: it may well
                // permit this viewer, and saying `denied` would overstate what
                // we know.
                "unknown".to_string()
            } else {
                "denied".to_string()
            }
        }
    };

    types::FeatureApproval {
        automatic,
        manual,
        current_user,
    }
}

pub fn account_from_db(urls: &InstanceUrls, a: &models::Account) -> types::Account {
    account_from_db_for_viewer(urls, a, None)
}

/// As [`account_from_db`], for a request whose viewer is known.
pub fn account_from_db_for_viewer(
    urls: &InstanceUrls,
    a: &models::Account,
    viewer: Option<&AccountViewerContext>,
) -> types::Account {
    let (url, uri) = if a.domain.is_none() {
        // Local accounts: the human url is /@username; the AP uri follows the
        // account's id_scheme (/users/{username} or /ap/users/{id}).
        (
            format!("https://{}/@{}", urls.local_domain, a.username),
            crate::federation::tag::account_uri(&urls.local_domain, a.id, a.id_scheme, &a.username),
        )
    } else {
        (
            a.url.clone().unwrap_or_default(),
            a.uri.clone().unwrap_or_default(),
        )
    };

    // Mastodon's `unavailable?`: a moderator suspension or the owner's own
    // deletion request both blank the profile out.
    let suspended = a.is_unavailable();

    types::Account {
        id: a.id.to_string(),
        username: a.pretty_username(),
        acct: a.acct(),
        display_name: if suspended {
            String::new()
        } else {
            a.display_name.clone()
        },
        locked: if suspended { false } else { a.locked },
        bot: !suspended && a.actor_type.as_deref() == Some("Service"),
        group: !suspended && a.actor_type.as_deref() == Some("Group"),
        discoverable: if suspended {
            Some(false)
        } else {
            a.discoverable
        },
        indexable: !suspended && a.indexable,
        hide_collections: a.hide_collections,
        created_at: a.created_at.format("%Y-%m-%dT00:00:00.000Z").to_string(),
        note: if suspended {
            String::new()
        } else {
            render_account_note(urls, a)
        },
        url,
        uri,
        avatar: if suspended {
            urls.missing_avatar().to_string()
        } else {
            account_avatar_url(urls, a)
        },
        avatar_static: if suspended {
            urls.missing_avatar().to_string()
        } else {
            account_avatar_url(urls, a)
        },
        header: if suspended {
            urls.missing_header().to_string()
        } else {
            account_header_url(urls, a)
        },
        header_static: if suspended {
            urls.missing_header().to_string()
        } else {
            account_header_url(urls, a)
        },
        // Mastodon blanks the alt text along with the image it describes.
        avatar_description: if suspended {
            String::new()
        } else {
            a.avatar_description.clone()
        },
        header_description: if suspended {
            String::new()
        } else {
            a.header_description.clone()
        },
        show_media: a.show_media,
        show_media_replies: a.show_media_replies,
        show_featured: a.show_featured,
        feature_approval: build_feature_approval(a, viewer),
        followers_count: 0,
        following_count: 0,
        statuses_count: 0,
        last_status_at: None,
        emojis: vec![],
        fields: if suspended {
            vec![]
        } else {
            // `account_field_value_format`.
            super::formatting::field_values(
                &urls.local_domain,
                fields_from_db(
                    a.fields.as_ref().unwrap_or(&serde_json::json!([])),
                    a.domain.is_none(),
                ),
                a.domain.is_none(),
                &[],
            )
        },
        roles: vec![],
        moved: None,
        suspended: if suspended { Some(true) } else { None },
        limited: if a.silenced_at.is_some() {
            Some(true)
        } else {
            None
        },
        // Mastodon reads the user's `noindex` setting, which asks search
        // engines to stay away, and it defaults to `Setting.noindex`.
        // `indexable` is a different question — whether this account's posts
        // may appear in *Mastodon's* search. The setting itself is filled in
        // by `batch_noindex` where the serializers can query; this is the
        // default until then.
        noindex: if a.domain.is_none() {
            Some(false)
        } else {
            None
        },
        memorial: if a.memorial { Some(true) } else { None },
        invalid_handle: a.has_invalid_handle().then_some(true),
        email_subscriptions: None,
        mute_expires_at: None,
        source: None,
        role: None,
    }
}

/// `Account::Field#sanitize`: stripped, then cut to
/// `MAX_CHARACTERS_LOCAL` (255) or, for a remote account,
/// `MAX_CHARACTERS_COMPAT` (2,047) characters.
pub fn sanitize_field(text: &str, local: bool) -> String {
    let limit = if local { 255 } else { 2_047 };
    text.trim().chars().take(limit).collect()
}

/// `Account#fields`: each stored field as `Account::Field` reads it,
/// sanitized; one without a name or a value is dropped.
pub fn fields_from_db(fields: &serde_json::Value, local: bool) -> Vec<types::Field> {
    fields
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    Some(types::Field {
                        name: sanitize_field(f["name"].as_str()?, local),
                        value: sanitize_field(f["value"].as_str()?, local),
                        verified_at: f["verified_at"].as_str().map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Serialize a media attachment's `meta`, guaranteeing that visual media always
/// carry `original.{width,height}` and a `small` block, mirroring Mastodon's
/// stored `file.meta`. The official iOS client sizes its image grid by dividing
/// the row width by the sum of the images' aspect ratios; a visual attachment
/// with no dimensions contributes a zero, and a post whose images are all
/// dimensionless divides by zero and aborts (`CALayer position contains NaN`).
fn media_meta_for_serialization(m: &models::MediaAttachment) -> serde_json::Value {
    ensure_media_dims(m.file_meta.clone(), super::media::media_type_str(m.r#type))
}

/// `{width, height, size: "WxH", aspect: width/height}`, matching Mastodon's
/// `MediaAttachment#image_geometry`.
fn image_geometry(w: i64, h: i64) -> serde_json::Value {
    serde_json::json!({
        "width": w,
        "height": h,
        "size": format!("{w}x{h}"),
        "aspect": w as f64 / h as f64,
    })
}

/// Scale `(w, h)` to fit within `max_px` pixels preserving aspect, never
/// enlarging — Mastodon's Paperclip `pixels:` geometry (`small` = 230_400).
fn scaled_to_pixels(w: i64, h: i64, max_px: i64) -> (i64, i64) {
    let area = w as f64 * h as f64;
    if area <= max_px as f64 {
        return (w, h);
    }
    let scale = (max_px as f64 / area).sqrt();
    (
        ((w as f64 * scale).round() as i64).max(1),
        ((h as f64 * scale).round() as i64).max(1),
    )
}

/// Guarantee the `meta` shape Mastodon stores for visual media, per type:
/// - **image**: `original` and `small` are both `image_geometry`
///   (`{width,height,size,aspect}`).
/// - **gifv/video**: `original` is `video_metadata`
///   (`{width,height,[frame_rate],[duration],[bitrate]}` — no size/aspect),
///   and `small` is the `image_geometry` of a 230_400px thumbnail.
/// - **audio/unknown**: untouched.
///
/// Existing blocks are preserved (so real imported/ffmpeg metadata is kept);
/// missing pieces are filled, and `original` falls back to a neutral 1:1 when
/// dimensions are unknown so the iOS image grid never divides by zero.
fn ensure_media_dims(file_meta: Option<serde_json::Value>, media_type: &str) -> serde_json::Value {
    let mut meta = file_meta.unwrap_or_else(|| serde_json::json!({}));
    if !meta.is_object() {
        meta = serde_json::json!({});
    }
    let is_image = media_type == "image";
    let is_video = matches!(media_type, "gifv" | "video");
    if !is_image && !is_video {
        return meta;
    }
    let dims = meta
        .get("original")
        .and_then(|o| Some((o.get("width")?.as_i64()?, o.get("height")?.as_i64()?)))
        .filter(|&(w, h)| w > 0 && h > 0);
    let obj = meta.as_object_mut().unwrap();
    let (w, h) = match dims {
        Some(d) => d,
        None => {
            // Image originals carry size/aspect; video originals never do.
            let orig = if is_image {
                image_geometry(1, 1)
            } else {
                serde_json::json!({ "width": 1, "height": 1 })
            };
            obj.insert("original".into(), orig);
            (1, 1)
        }
    };
    // Only image originals get size/aspect (Mastodon's image_geometry); video
    // originals mirror video_metadata, which has neither. Older federated image
    // rows stored only width/height — backfill the pair without disturbing any
    // extra keys.
    if is_image {
        if let Some(orig) = obj.get_mut("original").and_then(|o| o.as_object_mut()) {
            orig.entry("size")
                .or_insert_with(|| format!("{w}x{h}").into());
            orig.entry("aspect")
                .or_insert_with(|| (w as f64 / h as f64).into());
        }
    }
    // Both images and videos carry a 230_400px `small` (image_geometry of the
    // thumbnail).
    if !obj.contains_key("small") {
        let (sw, sh) = scaled_to_pixels(w, h, 230_400);
        obj.insert("small".into(), image_geometry(sw, sh));
    }
    meta
}

pub fn media_from_db(urls: &InstanceUrls, m: &models::MediaAttachment) -> types::MediaAttachment {
    // An attachment Mastodon never fetched and cannot show, of a type it does
    // not know (`unknown`): from a domain blocked with `reject_media`, or of a
    // type it does not take. `REST::MediaAttachmentSerializer` points its
    // `url` and `preview_url` at the media proxy (`needs_redownload?`) and
    // gives `meta` as it is stored. Eunha shows other remote attachments from
    // their own servers, keeping no copies (a recorded divergence); one of
    // these it must not, and the proxy has nothing to give for it.
    let unfetched = m.r#type == Some(4)
        && m.file_file_name.as_deref().is_none_or(str::is_empty)
        && m.remote_url.as_deref().is_some_and(|u| !u.is_empty());
    let proxy = |style: &str| format!("https://{}/media_proxy/{}/{style}", urls.local_domain, m.id);
    types::MediaAttachment {
        id: m.id.to_string(),
        media_type: super::media::media_type_str(m.r#type).to_string(),
        url: if unfetched {
            Some(proxy("original"))
        } else {
            media_url(urls, m)
        },
        preview_url: if unfetched {
            Some(proxy("small"))
        } else {
            media_preview_url(urls, m)
        },
        remote_url: m
            .remote_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        preview_remote_url: m
            .thumbnail_remote_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        text_url: None,
        description: m.description.clone(),
        blurhash: m.blurhash.clone(),
        meta: if unfetched {
            m.file_meta.clone()
        } else {
            Some(media_meta_for_serialization(m))
        },
    }
}

fn build_quote_approval(
    s: &models::Status,
    viewer: Option<&StatusViewerContext>,
) -> types::QuoteApproval {
    use crate::db::models::quote_policy;
    let policy = s.quote_approval_policy;
    let keys = |sub: i32| -> Vec<String> {
        quote_policy::as_keys(sub)
            .into_iter()
            .map(str::to_owned)
            .collect()
    };
    // `quote_policy_for_account`: no one to ask, a direct post or a reblog is
    // `denied`.
    let current_user = match viewer {
        Some(ctx) if s.visibility != crate::db::models::vis::DIRECT && s.reblog_of_id.is_none() => {
            quote_policy::for_account(
                policy,
                ctx.account_id == s.account_id,
                ctx.follows_author,
                ctx.author_follows,
            )
        }
        _ => quote_policy::ForAccount::Denied,
    };
    types::QuoteApproval {
        automatic: keys(quote_policy::automatic(policy)),
        manual: keys(quote_policy::manual(policy)),
        current_user: current_user.as_str().to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn status_from_db(
    urls: &InstanceUrls,
    s: &models::Status,
    account: &models::Account,
    media: Vec<models::MediaAttachment>,
    reblog: Option<(
        models::Status,
        models::Account,
        Vec<models::MediaAttachment>,
    )>,
    viewer_context: Option<StatusViewerContext>,
    mentions: &[types::StatusMention],
    reblog_mentions: &[types::StatusMention],
) -> types::Status {
    status_from_db_with_app(
        urls,
        s,
        account,
        media,
        reblog,
        viewer_context,
        None,
        mentions,
        reblog_mentions,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn status_from_db_with_app(
    urls: &InstanceUrls,
    s: &models::Status,
    account: &models::Account,
    media: Vec<models::MediaAttachment>,
    reblog: Option<(
        models::Status,
        models::Account,
        Vec<models::MediaAttachment>,
    )>,
    viewer_context: Option<StatusViewerContext>,
    application: Option<types::Application>,
    mentions: &[types::StatusMention],
    reblog_mentions: &[types::StatusMention],
) -> types::Status {
    let content = super::formatting::status_content(&urls.local_domain, &s.text, account, mentions);
    let reblog_status = reblog.map(|(rs, ra, rm)| {
        Box::new(status_from_db(
            urls,
            &rs,
            &ra,
            rm,
            None,
            viewer_context.clone(),
            reblog_mentions,
            &[],
        ))
    });

    // Mastodon: the author always sees their own raw `sensitive` flag; sensitization
    // from account-level flags is only applied to other viewers.
    // `object.proper.quote_policy_for_account`: a boost answers for the post
    // it boosts.
    let quote_approval = match &reblog_status {
        Some(original) => original.quote_approval.clone(),
        None => build_quote_approval(s, viewer_context.as_ref()),
    };
    let is_author = viewer_context.as_ref().map(|c| c.account_id) == Some(account.id);
    let sensitive = if is_author {
        s.sensitive
    } else {
        s.sensitive || account.sensitized_at.is_some()
    };

    // Mastodon omits viewer-dependent fields entirely for unauthenticated responses.
    // `pinned` is further restricted to the author's own view.
    let (favourited, reblogged, muted, bookmarked, pinned, filtered) =
        if let Some(ref ctx) = viewer_context {
            (
                Some(ctx.favourited),
                Some(ctx.reblogged),
                Some(ctx.muted),
                Some(ctx.bookmarked),
                if is_author
                    && s.reblog_of_id.is_none()
                    && matches!(
                        s.visibility,
                        crate::db::models::vis::PUBLIC
                            | crate::db::models::vis::UNLISTED
                            | crate::db::models::vis::PRIVATE
                    )
                {
                    Some(ctx.pinned)
                } else {
                    None
                },
                Some(vec![]),
            )
        } else {
            (None, None, None, None, None, None)
        };

    types::Status {
        id: s.id.to_string(),
        created_at: s.created_at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        in_reply_to_id: s.in_reply_to_id.map(|i| i.to_string()),
        in_reply_to_account_id: s.in_reply_to_account_id.map(|i| i.to_string()),
        sensitive,
        spoiler_text: s.spoiler_text.clone(),
        visibility: crate::db::models::vis::to_str(s.visibility).to_owned(),
        language: s.language.clone(),
        uri: if account.domain.is_none() {
            // Local status: the canonical AP uri is always computed from the
            // account's id_scheme, matching Mastodon's `TagManager#uri_for`
            // (`.../statuses/{id}` for posts, `.../statuses/{id}/activity` for
            // boosts). Local rows may store a NULL `uri`, so never fall back to
            // the bare id.
            let base = crate::federation::tag::status_uri(
                &urls.local_domain,
                account.id,
                account.id_scheme,
                &account.username,
                s.id,
            );
            if s.reblog_of_id.is_some() {
                format!("{base}/activity")
            } else {
                base
            }
        } else {
            s.uri.clone().unwrap_or_else(|| s.id.to_string())
        },
        url: if account.domain.is_none() {
            // Local status: human permalink is /@username/{id}; prefer a stored
            // non-AP url, otherwise derive from the (id_scheme-independent) username.
            Some(
                s.url
                    .clone()
                    .filter(|u| !u.is_empty() && Some(u.as_str()) != s.uri.as_deref())
                    .unwrap_or_else(|| {
                        format!(
                            "https://{}/@{}/{}",
                            urls.local_domain, account.username, s.id
                        )
                    }),
            )
        } else {
            let uri_str = s.uri.as_deref();
            s.url
                .as_deref()
                .filter(|&u| uri_str != Some(u))
                .map(String::from)
                .or_else(|| status_url_from_uri(uri_str?))
        },
        replies_count: 0,
        reblogs_count: 0,
        favourites_count: 0,
        quotes_count: 0,
        edited_at: s
            .edited_at
            .map(|t| t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()),
        content,
        reblog: reblog_status,
        application,
        account: account_from_db(urls, account),
        media_attachments: media
            .iter()
            .map(|m| media_from_db(urls, m))
            .filter(|m| m.url.is_some() || m.remote_url.as_deref().is_some_and(|u| !u.is_empty()))
            .collect(),
        mentions: mentions.to_vec(),
        tags: vec![],
        emojis: vec![],
        card: None,
        poll: None,
        quote: None,
        quote_approval,
        tagged_collections: vec![],
        favourited,
        reblogged,
        muted,
        bookmarked,
        pinned,
        filtered,
        text: None,
    }
}

#[derive(Clone)]
pub struct StatusViewerContext {
    pub account_id: i64,
    pub follows_author: bool,
    pub author_follows: bool,
    pub favourited: bool,
    pub reblogged: bool,
    pub muted: bool,
    pub bookmarked: bool,
    pub pinned: bool,
}

#[cfg(test)]
mod tests {
    use super::ensure_media_dims;
    use serde_json::json;

    #[test]
    fn injects_default_dims_for_dimensionless_images() {
        // NULL meta on an image → neutral 1:1 original so the iOS grid never
        // divides by a zero aspect-ratio sum, plus a matching small.
        let meta = ensure_media_dims(None, "image");
        assert_eq!(meta["original"]["width"], json!(1));
        assert_eq!(meta["original"]["height"], json!(1));
        assert_eq!(meta["small"]["width"], json!(1));

        // Empty object likewise.
        let meta = ensure_media_dims(Some(json!({})), "image");
        assert_eq!(meta["original"]["height"], json!(1));
    }

    #[test]
    fn image_matches_mastodon_geometry() {
        // Matches Mastodon's image_geometry + 230_400px small exactly:
        // baram.me serves original 1206x1706 with small 404x571.
        let meta = ensure_media_dims(
            Some(json!({ "original": { "width": 1206, "height": 1706 } })),
            "image",
        );
        assert_eq!(meta["original"]["size"], json!("1206x1706"));
        assert_eq!(meta["small"]["width"], json!(404));
        assert_eq!(meta["small"]["height"], json!(571));
    }

    #[test]
    fn video_original_has_no_size_or_aspect() {
        // Mastodon's video_metadata is {width,height,frame_rate,duration,bitrate}
        // with no size/aspect; small is the 230_400px image_geometry.
        // 720x1280 -> small 360x640.
        let meta = ensure_media_dims(
            Some(json!({
                "original": { "width": 720, "height": 1280, "frame_rate": "30/1", "duration": 25.4, "bitrate": 3266734 },
            })),
            "video",
        );
        assert!(meta["original"].get("size").is_none());
        assert!(meta["original"].get("aspect").is_none());
        assert_eq!(meta["original"]["frame_rate"], json!("30/1"));
        assert_eq!(meta["original"]["bitrate"], json!(3266734));
        assert_eq!(
            meta["small"],
            json!({ "width": 360, "height": 640, "size": "360x640", "aspect": 0.5625 })
        );
    }

    #[test]
    fn video_without_dims_gets_bare_original() {
        let meta = ensure_media_dims(None, "video");
        assert_eq!(meta["original"]["width"], json!(1));
        assert!(meta["original"].get("size").is_none());
        assert_eq!(meta["small"]["width"], json!(1));
    }

    #[test]
    fn preserves_existing_small() {
        // Imported Mastodon media already carry a real small — don't clobber it.
        let meta = ensure_media_dims(
            Some(json!({
                "original": { "width": 100, "height": 100 },
                "small": { "width": 50, "height": 50, "size": "50x50", "aspect": 1.0 },
            })),
            "image",
        );
        assert_eq!(meta["small"]["width"], json!(50));
    }

    #[test]
    fn leaves_non_visual_media_untouched() {
        // Audio has no dimensions and needs none — don't fabricate them.
        let meta = ensure_media_dims(None, "audio");
        assert!(meta.get("original").is_none());
        assert!(meta.get("small").is_none());
    }
}
