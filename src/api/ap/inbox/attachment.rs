//! Pure helpers for parsing ActivityPub attachment/media JSON into the shapes
//! Eunha stores and serves. No I/O or database access — every function here is
//! a pure transformation of `serde_json` values.

use serde_json::Value;

/// Extract a usable `(href, media_type)` from an ActivityPub `url` value, which
/// may be a bare string, a `Link` object, or an array of either. When given an
/// array, prefers the first image/video/audio link, falling back to the first
/// link of any type.
pub(super) fn attachment_url(value: &Value) -> Option<(String, Option<String>)> {
    match value {
        Value::String(s) if !s.is_empty() => Some((s.clone(), None)),
        Value::Object(o) => {
            let href = o
                .get("href")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())?;
            let media_type = o
                .get("mediaType")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            Some((href.to_string(), media_type))
        }
        Value::Array(arr) => {
            let mut fallback: Option<(String, Option<String>)> = None;
            for el in arr {
                if let Some((href, media_type)) = attachment_url(el) {
                    let is_media = media_type.as_deref().is_some_and(|m| {
                        m.starts_with("image/")
                            || m.starts_with("video/")
                            || m.starts_with("audio/")
                    });
                    if is_media {
                        return Some((href, media_type));
                    }
                    if fallback.is_none() {
                        fallback = Some((href, media_type));
                    }
                }
            }
            fallback
        }
        _ => None,
    }
}

/// An attachment of a remote status, as `MediaAttachmentParser` reads it
/// for `Create` and `Update` alike.
pub(super) struct RemoteMedia {
    pub remote_url: String,
    /// Eunha's type code, from [`classify_attachment_type`].
    pub kind: i32,
    pub description: Option<String>,
    pub blurhash: Option<String>,
    pub thumbnail_remote_url: Option<String>,
    pub file_content_type: Option<String>,
    pub file_meta: Option<Value>,
}

/// `as_array(json['attachment'])`: an object's attachments, one that is not
/// in an array included.
pub(super) fn attachments_of(object: &Value) -> Vec<Value> {
    match object.get("attachment") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Null) | None => Vec::new(),
        Some(item) => vec![item.clone()],
    }
}

impl RemoteMedia {
    /// The row as Mastodon stores an attachment it does not download
    /// (`skip_download`, a domain blocked with `reject_media`): no file, so
    /// `set_unknown_type` leaves it `unknown` with no content type, and its
    /// `file_meta` is the focus alone, which `focus=` writes.
    pub fn not_downloaded(self) -> Self {
        let file_meta = self
            .file_meta
            .as_ref()
            .and_then(|meta| meta.get("focus"))
            .map(|focus| serde_json::json!({ "focus": focus }));
        Self {
            kind: 4,
            file_content_type: None,
            file_meta,
            ..self
        }
    }
}

/// `MediaAttachment::MAX_DESCRIPTION_HARD_LENGTH_LIMIT`.
const MAX_DESCRIPTION_HARD_LENGTH_LIMIT: usize = 10_000;

/// `MediaAttachmentParser#description`: the attachment's `summary`, or else
/// its `name`, the first that is not blank, stripped and cut to
/// `MAX_DESCRIPTION_HARD_LENGTH_LIMIT` characters.
fn description(att: &Value) -> Option<String> {
    use ojak_vocab::json_ld_helper::first_lang_string;
    let text = first_lang_string(att, "summary")
        .filter(|text| !text.trim().is_empty())
        .or_else(|| first_lang_string(att, "name").filter(|text| !text.trim().is_empty()))?;
    Some(
        text.trim()
            .chars()
            .take(MAX_DESCRIPTION_HARD_LENGTH_LIMIT)
            .collect(),
    )
}

/// The media `att` names, or `None` when it has no `url`
/// (`remote_url.blank?`).
pub(super) fn remote_media(att: &Value) -> Option<RemoteMedia> {
    let att_type_str = att.get("type").and_then(|v| v.as_str()).unwrap_or("");
    // `url` may be a string, a Link object (`{href, mediaType}`), or an
    // array of links — Mastodon resolves all of these.
    let (remote_url, link_media_type) = att.get("url").and_then(attachment_url)?;
    // mediaType: explicit, else from the chosen Link, else guessed from the
    // URL's extension (matches Mastodon's `mediaType || url_to_media_type`).
    let media_type_str = att
        .get("mediaType")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .or(link_media_type)
        .or_else(|| {
            let path = remote_url.split(['?', '#']).next().unwrap_or(&remote_url);
            mime_guess::from_path(path).first_raw().map(str::to_owned)
        })
        .unwrap_or_default();
    // Classify from mediaType — Mastodon serializes `type: "Document"` for
    // everything — falling back to the AP `type` hint for odd peers.
    let kind = classify_attachment_type(att_type_str, &media_type_str);
    Some(RemoteMedia {
        kind,
        description: description(att),
        blurhash: att
            .get("blurhash")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        thumbnail_remote_url: att
            .get("icon")
            .and_then(|i| if i.is_object() { i.get("url") } else { None })
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        file_content_type: (!media_type_str.is_empty()).then_some(media_type_str),
        file_meta: ap_attachment_file_meta(att),
        remote_url,
    })
}

/// Build a Mastodon-style `file_meta` (`{"original": {...}, "focus": {...}}`)
/// from an ActivityPub attachment's `width`/`height`/`duration`/`focalPoint`.
/// Returns `None` when the attachment carries no geometry.
///
/// This must run on every media-ingestion path: the official iOS client sizes
/// its image grid by dividing the container width by the sum of the images'
/// aspect ratios, and an image with no dimensions contributes nothing — so a
/// post whose images all lack `meta.original.{width,height}` divides by zero,
/// producing a NaN layout that aborts the app (`CALayer position contains NaN`).
/// FEP-8967: the `href` of the first `Link` in an object's `attachment`, which
/// names the status's preview card.
///
/// Mastodon 4.7.0 uses the first `Link` it finds and ignores the rest, and
/// falls back to scanning the content when there is none.
pub(super) fn preview_card_link(attachments: &[Value]) -> Option<&str> {
    attachments
        .iter()
        .find(|att| att.get("type").and_then(|t| t.as_str()) == Some("Link"))
        .and_then(|att| att.get("href").and_then(|h| h.as_str()))
        .filter(|href| !href.is_empty())
}

pub(super) fn ap_attachment_file_meta(att: &serde_json::Value) -> Option<serde_json::Value> {
    let width = att.get("width").and_then(|v| v.as_i64());
    let height = att.get("height").and_then(|v| v.as_i64());
    // Mastodon serializes a video's `duration` as an ISO8601 duration string
    // (e.g. "PT25.4S"); Misskey/others may send a number. `size`/`aspect` are
    // added at serialization (image only, mirroring Mastodon's meta shape).
    let duration = att.get("duration").and_then(parse_ap_duration);
    // focalPoint [x, y] -> meta.focus { x, y } (Mastodon's focus).
    let focus = att
        .get("focalPoint")
        .and_then(|v| v.as_array())
        .and_then(|a| Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?)));
    if width.is_none() && height.is_none() && duration.is_none() && focus.is_none() {
        return None;
    }
    let mut meta = serde_json::Map::new();
    if width.is_some() || height.is_some() || duration.is_some() {
        let mut orig = serde_json::Map::new();
        if let Some(w) = width {
            orig.insert("width".into(), w.into());
        }
        if let Some(h) = height {
            orig.insert("height".into(), h.into());
        }
        if let Some(d) = duration {
            orig.insert("duration".into(), d.into());
        }
        meta.insert("original".into(), serde_json::Value::Object(orig));
    }
    if let Some((x, y)) = focus {
        meta.insert("focus".into(), serde_json::json!({ "x": x, "y": y }));
    }
    Some(serde_json::Value::Object(meta))
}

/// Parse an ActivityPub attachment `duration` into seconds. Accepts a plain
/// number or an ISO8601 duration's time part (`PT[nH][nM][nS]`, e.g. "PT25.4S").
pub(super) fn parse_ap_duration(v: &serde_json::Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    let rest = v.as_str()?.strip_prefix("PT")?;
    let mut total = 0.0;
    let mut num = String::new();
    for c in rest.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else {
            let val: f64 = num.parse().ok()?;
            total += match c {
                'H' => val * 3600.0,
                'M' => val * 60.0,
                'S' => val,
                _ => return None,
            };
            num.clear();
        }
    }
    (total > 0.0).then_some(total)
}

/// Map an attachment's `type`/`mediaType` to Eunha's media-attachment type code
/// (0 image, 1 gifv, 2 video, 3 audio, 4 unknown).
pub(super) fn classify_attachment_type(att_type_str: &str, media_type_str: &str) -> i32 {
    if media_type_str == "image/gif" {
        1
    } else if media_type_str.starts_with("image/") {
        0
    } else if media_type_str.starts_with("video/") {
        2
    } else if media_type_str.starts_with("audio/") {
        3
    } else {
        match att_type_str {
            "Image" => 0,
            "Video" => {
                if media_type_str.contains("gif") {
                    1
                } else {
                    2
                }
            }
            "Audio" => 3,
            _ => 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ap_duration_handles_iso8601_and_numbers() {
        use serde_json::json;
        // Mastodon sends ISO8601 time durations.
        assert_eq!(parse_ap_duration(&json!("PT25.4S")), Some(25.4));
        assert_eq!(parse_ap_duration(&json!("PT1M5S")), Some(65.0));
        assert_eq!(parse_ap_duration(&json!("PT1H2M3S")), Some(3723.0));
        // Misskey/others may send a plain number.
        assert_eq!(parse_ap_duration(&json!(12.5)), Some(12.5));
        // Garbage → None.
        assert_eq!(parse_ap_duration(&json!("nonsense")), None);
    }

    #[test]
    fn attachment_type_prefers_media_type_for_document_attachments() {
        assert_eq!(classify_attachment_type("Document", "image/webp"), 0);
        assert_eq!(classify_attachment_type("Document", "image/jpeg"), 0);
        assert_eq!(classify_attachment_type("Document", "image/gif"), 1);
        assert_eq!(classify_attachment_type("Document", "video/mp4"), 2);
        assert_eq!(classify_attachment_type("Document", "audio/mpeg"), 3);
    }

    #[test]
    fn preview_card_link_takes_the_first_link_attachment() {
        let attachments = vec![
            serde_json::json!({"type": "Document", "url": "https://example.test/a.png"}),
            serde_json::json!({"type": "Link", "href": "https://example.test/article"}),
            serde_json::json!({"type": "Link", "href": "https://example.test/other"}),
        ];
        assert_eq!(
            preview_card_link(&attachments),
            Some("https://example.test/article")
        );
    }

    #[test]
    fn preview_card_link_ignores_media_and_empty_hrefs() {
        let media_only =
            vec![serde_json::json!({"type": "Document", "url": "https://example.test/a.png"})];
        assert_eq!(preview_card_link(&media_only), None);

        let empty = vec![serde_json::json!({"type": "Link", "href": ""})];
        assert_eq!(preview_card_link(&empty), None);

        let no_href = vec![serde_json::json!({"type": "Link"})];
        assert_eq!(preview_card_link(&no_href), None);
    }

    #[test]
    fn attachment_type_falls_back_to_activitypub_type() {
        assert_eq!(classify_attachment_type("Image", ""), 0);
        assert_eq!(classify_attachment_type("Video", ""), 2);
        assert_eq!(classify_attachment_type("Audio", ""), 3);
        assert_eq!(classify_attachment_type("Document", ""), 4);
    }
}

#[cfg(test)]
mod media_parser_tests {
    use serde_json::json;

    use super::*;

    /// `summary` before `name`, a blank one passed over, stripped.
    #[test]
    fn a_description_is_its_summary_then_its_name() {
        assert_eq!(
            description(&json!({"summary": " alt ", "name": "name"})).as_deref(),
            Some("alt")
        );
        assert_eq!(
            description(&json!({"summary": "  ", "name": "name"})).as_deref(),
            Some("name")
        );
        assert_eq!(
            description(&json!({"nameMap": {"en": "mapped"}})).as_deref(),
            Some("mapped")
        );
        assert_eq!(description(&json!({"name": ""})), None);
        assert_eq!(
            description(&json!({"name": "x".repeat(10_005)}))
                .unwrap()
                .chars()
                .count(),
            10_000
        );
    }
}
