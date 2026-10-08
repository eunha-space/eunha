//! `ActivityPub::Parser::PollParser`: a remote `Question`'s poll, as both
//! `Create` (`process_poll`) and `Update` (`update_poll!`) read it.

use chrono::NaiveDateTime;
use serde_json::Value;

/// `MAX_ITEMS`: a poll's options past the 500th are left off, rather than
/// the post refused.
const MAX_ITEMS: usize = 500;

/// A poll as `PollParser` reads it.
#[derive(Debug, PartialEq)]
pub struct PollParser {
    /// `multiple`: `anyOf` is an array.
    pub multiple: bool,
    /// `options`: each item's `name` when it is present, else its `content`;
    /// an item with neither is left out.
    pub options: Vec<String>,
    /// `cached_tallies`: each item's `replies.totalItems`, `0` without one,
    /// for every item, those without an option included.
    pub cached_tallies: Vec<i64>,
    /// `expires_at`.
    pub expires_at: Option<NaiveDateTime>,
    /// `voters_count`.
    pub voters_count: Option<i64>,
}

impl PollParser {
    /// The poll `json` is, if it is one (`valid?`): a `Question` with its
    /// items, `anyOf` first and then `oneOf`, in an array.
    #[must_use]
    pub fn parse(json: &Value) -> Option<Self> {
        if !ojak_vocab::json_ld_helper::type_is(json, "Question") {
            return None;
        }
        // `@json['anyOf'] || @json['oneOf']`: an `anyOf` that is there at all
        // is the one read.
        let items = match json.get("anyOf").filter(|v| !v.is_null()) {
            Some(any_of) => any_of,
            None => json.get("oneOf")?,
        };
        let items: Vec<&Value> = items.as_array()?.iter().take(MAX_ITEMS).collect();
        let options = items
            .iter()
            .filter_map(|item| {
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty());
                name.or_else(|| item.get("content").and_then(Value::as_str))
                    .map(str::to_owned)
            })
            .collect();
        let cached_tallies = items
            .iter()
            .map(|item| {
                item.get("replies")
                    .and_then(|replies| replies.get("totalItems"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
            })
            .collect();
        Some(Self {
            multiple: json.get("anyOf").is_some_and(Value::is_array),
            options,
            cached_tallies,
            expires_at: expires_at(json, chrono::Utc::now().naive_utc()),
            voters_count: json.get("votersCount").and_then(Value::as_i64),
        })
    }

    /// `votes_count`, as `Poll#prepare_votes_count` sets it from the
    /// tallies.
    #[must_use]
    pub fn votes_count(&self) -> i64 {
        self.cached_tallies.iter().sum()
    }
}

/// `PollParser#expires_at`: `closed` when it is a time, now when it is any
/// other value but `false`, or else `endTime`. A time that does not parse is
/// none.
fn expires_at(json: &Value, now: NaiveDateTime) -> Option<NaiveDateTime> {
    let time = |value: &str| {
        chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|time| time.naive_utc())
    };
    match json.get("closed") {
        Some(Value::String(closed)) => time(closed),
        Some(Value::Null | Value::Bool(false)) | None => {
            json.get("endTime").and_then(Value::as_str).and_then(time)
        }
        Some(_) => Some(now),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_a_question_as_poll_parser_does() {
        let poll = PollParser::parse(&json!({
            "type": "Question",
            "oneOf": [
                {"name": "yes", "replies": {"totalItems": 3}},
                {"name": " ", "content": "from content"},
                {"content": ""},
                {"type": "Note"},
            ],
            "endTime": "2026-01-02T03:04:05Z",
            "votersCount": 3,
        }))
        .unwrap();
        assert!(!poll.multiple);
        assert_eq!(poll.options, ["yes", "from content", ""]);
        assert_eq!(poll.cached_tallies, [3, 0, 0, 0]);
        assert_eq!(poll.votes_count(), 3);
        assert_eq!(poll.voters_count, Some(3));
        assert_eq!(poll.expires_at.unwrap().to_string(), "2026-01-02 03:04:05");

        // `anyOf` is read before `oneOf`; and a Note is no poll.
        let poll = PollParser::parse(&json!({
            "type": ["Question"],
            "oneOf": [{"name": "one"}],
            "anyOf": [{"name": "any"}],
        }))
        .unwrap();
        assert!(poll.multiple);
        assert_eq!(poll.options, ["any"]);
        assert_eq!(
            PollParser::parse(&json!({"type": "Note", "oneOf": [{"name": "a"}]})),
            None
        );
        assert_eq!(
            PollParser::parse(&json!({"type": "Question", "oneOf": "a"})),
            None
        );
    }

    #[test]
    fn closed_overrides_end_time() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-06T07:08:09Z")
            .unwrap()
            .naive_utc();
        let end = |closed: Value| {
            expires_at(
                &json!({"closed": closed, "endTime": "2026-01-01T00:00:00Z"}),
                now,
            )
            .map(|time| time.to_string())
        };
        assert_eq!(
            end(json!("2025-12-31T00:00:00Z")).as_deref(),
            Some("2025-12-31 00:00:00")
        );
        assert_eq!(end(json!(true)).as_deref(), Some("2026-05-06 07:08:09"));
        assert_eq!(end(json!(false)).as_deref(), Some("2026-01-01 00:00:00"));
        assert_eq!(end(json!(null)).as_deref(), Some("2026-01-01 00:00:00"));
        assert_eq!(end(json!("not a time")), None);
    }
}
