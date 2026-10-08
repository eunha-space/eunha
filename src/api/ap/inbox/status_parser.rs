//! What `ActivityPub::Parser::StatusParser` reads from a status object that
//! a `Create` and an `Update` both need: which objects are statuses at all,
//! how one of the converted kinds (an `Article`, a `Video`, …) becomes a
//! status's text, and the counts a remote server reports for it.

use serde_json::Value;

use crate::state::AppState;

/// `ActivityPub::Activity::SUPPORTED_TYPES`.
pub const SUPPORTED_TYPES: [&str; 2] = ["Note", "Question"];
/// `ActivityPub::Activity::CONVERTED_TYPES`.
pub const CONVERTED_TYPES: [&str; 6] = ["Image", "Audio", "Video", "Article", "Page", "Event"];

/// `supported_object_type? || converted_object_type?`.
pub fn is_status_type(object: &Value) -> bool {
    use crate::federation::fetch_resource::type_matches;
    type_matches(object, &SUPPORTED_TYPES) || type_matches(object, &CONVERTED_TYPES)
}

/// `converted_object_type?`.
pub fn is_converted(object: &Value) -> bool {
    crate::federation::fetch_resource::type_matches(object, &CONVERTED_TYPES)
}

/// `StatusParser#uri`, which reads the `u` of a `bear:` id.
pub fn uri(object: &Value) -> Option<String> {
    let id = object.get("id").and_then(Value::as_str)?;
    if id.starts_with("bear:") {
        if let Some(u) = url::Url::parse(id).ok().and_then(|url| {
            url.query_pairs()
                .find(|(k, _)| k == "u")
                .map(|(_, v)| v.into_owned())
        }) {
            return Some(u);
        }
    }
    Some(id.to_owned())
}

fn present_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

fn first_of_map(object: &Value, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_object)
        .and_then(|map| map.values().next())
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// `StatusParser#text`: `content`, or the first of `contentMap`.
pub fn text(object: &Value) -> Option<String> {
    present_str(object.get("content"))
        .map(str::to_owned)
        .or_else(|| first_of_map(object, "contentMap"))
}

/// `StatusParser#spoiler_text`: `summary`, or the first of `summaryMap`.
pub fn spoiler_text(object: &Value) -> Option<String> {
    present_str(object.get("summary"))
        .map(str::to_owned)
        .or_else(|| first_of_map(object, "summaryMap"))
}

/// `StatusParser#title`: `name`, or the first of `nameMap`.
pub fn title(object: &Value) -> Option<String> {
    present_str(object.get("name"))
        .map(str::to_owned)
        .or_else(|| first_of_map(object, "nameMap"))
}

/// `StatusParser#language`: the first language of `contentMap`, `nameMap`
/// or `summaryMap`, in the spelling of the supported locale it names.
pub fn language(object: &Value) -> Option<String> {
    let first_key = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_object)
            .filter(|map| !map.is_empty())
            .and_then(|map| map.keys().next().cloned())
    };
    let raw = first_key("contentMap")
        .or_else(|| first_key("nameMap"))
        .or_else(|| first_key("summaryMap"))?;
    (!raw.trim().is_empty()).then(|| crate::languages::normalized_locale_name(&raw))
}

/// `StatusParser#url`: the `text/html` link among `url`, if it is `http` or
/// `https`.
pub fn url(object: &Value) -> Option<String> {
    let value = object
        .get("url")
        .filter(|v| crate::federation::json_ld::is_present(v))?;
    ojak_vocab::json_ld_helper::url_to_href(value, Some("text/html"))
        .map(str::to_owned)
        .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
}

/// `StatusParser#processed_text`: the text, or for a converted object its
/// title, summary and a link to it.
pub fn processed_text(state: &AppState, object: &Value) -> String {
    if !is_converted(object) {
        return text(object).unwrap_or_default();
    }
    let link = url(object).or_else(|| uri(object)).unwrap_or_default();
    [
        title(object).map(|title| format!("<h2>{title}</h2>")),
        spoiler_text(object),
        Some(crate::formatter::text::format(
            &link,
            &crate::formatter::Options::new(&state.instance.domain),
        )),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n\n")
}

/// `StatusParser#processed_spoiler_text`: nothing for a converted object.
pub fn processed_spoiler_text(object: &Value) -> String {
    if is_converted(object) {
        return String::new();
    }
    spoiler_text(object).unwrap_or_default()
}

/// `StatusStat::MAX_UNTRUSTED_COUNT`.
const MAX_UNTRUSTED_COUNT: i64 = 100_000_000;

/// A `totalItems` read as Ruby's `to_i` reads it, and clamped as
/// `StatusStat#clamp_untrusted_counts` clamps it; `None` when Rails would
/// leave the count unset.
fn untrusted_count(collection: Option<&Value>) -> Option<i64> {
    let total = collection.filter(|c| c.is_object())?.get("totalItems")?;
    let count = match total {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Value::String(s) if s.trim().is_empty() => return None,
        Value::String(s) => {
            let s = s.trim_start();
            let (sign, digits) = match s.strip_prefix('-') {
                Some(rest) => (-1, rest),
                None => (1, s.strip_prefix('+').unwrap_or(s)),
            };
            let digits: String = digits.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<i64>().map(|n| sign * n).unwrap_or(0)
        }
        Value::Null => return None,
        Value::Bool(false) => return None,
        _ => 0,
    };
    Some(count.clamp(0, MAX_UNTRUSTED_COUNT))
}

/// `StatusParser#favourites_count`: `likes.totalItems`, when `likes` is an
/// object.
pub fn favourites_count(object: &Value) -> Option<i64> {
    untrusted_count(object.get("likes"))
}

/// `StatusParser#reblogs_count`: `shares.totalItems`, when `shares` is an
/// object.
pub fn reblogs_count(object: &Value) -> Option<i64> {
    untrusted_count(object.get("shares"))
}

/// `attach_counts` and `update_counts!`: the counts the status's server
/// reports, kept beside ours in `status_stats` and served in their place.
/// A count the object does not report is left as it was.
pub async fn store_untrusted_counts(
    state: &AppState,
    status_id: i64,
    object: &Value,
) -> Result<(), sqlx::Error> {
    let likes = favourites_count(object);
    let shares = reblogs_count(object);
    if likes.is_none() && shares.is_none() {
        return Ok(());
    }
    sqlx::query!(
        r#"INSERT INTO status_stats
             (status_id, untrusted_favourites_count, untrusted_reblogs_count, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())
           ON CONFLICT (status_id) DO UPDATE SET
             untrusted_favourites_count =
               COALESCE($2, status_stats.untrusted_favourites_count),
             untrusted_reblogs_count = COALESCE($3, status_stats.untrusted_reblogs_count),
             updated_at = CASE
               WHEN status_stats.untrusted_favourites_count IS DISTINCT FROM
                      COALESCE($2, status_stats.untrusted_favourites_count)
                 OR status_stats.untrusted_reblogs_count IS DISTINCT FROM
                      COALESCE($3, status_stats.untrusted_reblogs_count)
               THEN now() ELSE status_stats.updated_at END"#,
        status_id,
        likes,
        shares,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn counts_are_read_as_ruby_reads_them() {
        assert_eq!(
            favourites_count(&json!({"likes": {"totalItems": 7}})),
            Some(7)
        );
        assert_eq!(
            favourites_count(&json!({"likes": {"totalItems": "12x"}})),
            Some(12)
        );
        assert_eq!(
            favourites_count(&json!({"likes": {"totalItems": -3}})),
            Some(0)
        );
        assert_eq!(
            reblogs_count(&json!({"shares": {"totalItems": 1_000_000_000}})),
            Some(MAX_UNTRUSTED_COUNT)
        );
        assert_eq!(
            favourites_count(&json!({"likes": "https://a.example/likes"})),
            None
        );
        assert_eq!(favourites_count(&json!({"likes": {}})), None);
    }

    #[test]
    fn the_language_is_the_first_of_a_map() {
        let object = json!({"contentMap": {"ja": "x", "en": "y"}});
        assert_eq!(language(&object).as_deref(), Some("ja"));
        let object = json!({"contentMap": {}, "nameMap": {"ZH-cn": "t"}});
        assert_eq!(language(&object).as_deref(), Some("zh-CN"));
        assert_eq!(language(&json!({"content": "x"})), None);
    }

    #[test]
    fn the_url_is_the_html_link() {
        let object = json!({"url": [
            {"type": "Link", "mimeType": "video/mp4", "href": "https://a.example/v.mp4"},
            {"type": "Link", "href": "https://a.example/v"},
        ]});
        assert_eq!(url(&object).as_deref(), Some("https://a.example/v"));
        assert_eq!(url(&json!({"url": "ftp://a.example/p"})), None);
    }
}
