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
    let time = to_datetime;
    match json.get("closed") {
        Some(Value::String(closed)) => time(closed),
        Some(Value::Null | Value::Bool(false)) | None => {
            json.get("endTime").and_then(Value::as_str).and_then(time)
        }
        Some(_) => Some(now),
    }
}

/// ActiveSupport's `String#to_datetime`, `DateTime.parse(self, false)`, in
/// UTC, for the forms a server writes a time in: ISO 8601 in its extended or
/// basic form, with a `T`, a space or nothing between the date and the time,
/// the time to the minute or to the second with any fraction, or a date
/// alone (its midnight), and a zone that is `Z`, `UTC`, `GMT`, `UT`, or an
/// offset of hours with or without minutes, or none (UTC, as `DateTime`
/// takes it); and RFC 2822. Blank is none, as `to_datetime` has it, and so
/// is anything else `DateTime.parse` would raise on. What `DateTime.parse`
/// reads beyond these, such as month names in free text or a time with no
/// date, is not read.
pub(crate) fn to_datetime(value: &str) -> Option<NaiveDateTime> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(time.naive_utc());
    }
    if let Ok(time) = chrono::DateTime::parse_from_rfc2822(value) {
        return Some(time.naive_utc());
    }
    iso8601(value)
}

/// The ISO 8601 forms [`to_datetime`] reads past RFC 3339.
fn iso8601(value: &str) -> Option<NaiveDateTime> {
    let bytes = value.as_bytes();
    let digits = |from: usize, len: usize| -> Option<u32> {
        let part = value.get(from..from + len)?;
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    // The date: `YYYY-MM-DD`, or `YYYYMMDD`.
    let (date, mut at) = if bytes.get(4) == Some(&b'-') {
        if bytes.get(7) != Some(&b'-') {
            return None;
        }
        (
            chrono::NaiveDate::from_ymd_opt(
                i32::try_from(digits(0, 4)?).ok()?,
                digits(5, 2)?,
                digits(8, 2)?,
            )?,
            10,
        )
    } else {
        (
            chrono::NaiveDate::from_ymd_opt(
                i32::try_from(digits(0, 4)?).ok()?,
                digits(4, 2)?,
                digits(6, 2)?,
            )?,
            8,
        )
    };
    // The time, after `T`, `t` or a space: `HH:MM[:SS[.fff]]` or
    // `HHMM[SS[.fff]]`.
    let mut time = chrono::NaiveTime::MIN;
    if matches!(bytes.get(at), Some(b'T' | b't' | b' '))
        && bytes.get(at + 1).is_some_and(u8::is_ascii_digit)
    {
        at += 1;
        let hour = digits(at, 2)?;
        at += 2;
        let extended = bytes.get(at) == Some(&b':');
        if extended {
            at += 1;
        }
        let minute = digits(at, 2)?;
        at += 2;
        let mut second = 0;
        let mut nanos = 0;
        let has_seconds = if extended {
            bytes.get(at) == Some(&b':')
        } else {
            bytes.get(at).is_some_and(u8::is_ascii_digit)
        };
        if has_seconds {
            if extended {
                at += 1;
            }
            second = digits(at, 2)?;
            at += 2;
            if matches!(bytes.get(at), Some(b'.' | b',')) {
                at += 1;
                let start = at;
                while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                    at += 1;
                }
                let fraction = value.get(start..at)?;
                if fraction.is_empty() {
                    return None;
                }
                let padded = format!("{:0<9}", &fraction[..fraction.len().min(9)]);
                nanos = padded.parse().ok()?;
            }
        }
        // `DateTime` takes a leap second as the next minute's start.
        let (second, carry) = if second == 60 { (59, 1) } else { (second, 0) };
        time = chrono::NaiveTime::from_hms_nano_opt(hour, minute, second, nanos)?
            + chrono::Duration::seconds(carry);
    }
    // The zone.
    let rest = value[at..].trim_start();
    let offset_seconds: i64 = match rest {
        "" | "Z" | "z" => 0,
        zone if ["UTC", "GMT", "UT"]
            .iter()
            .any(|name| zone.eq_ignore_ascii_case(name)) =>
        {
            0
        }
        zone => {
            let sign = match zone.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let zone = &zone[1..];
            let (hours, minutes) = match zone.len() {
                2 => (zone, "00"),
                4 => (&zone[..2], &zone[2..]),
                5 if zone.as_bytes()[2] == b':' => (&zone[..2], &zone[3..]),
                _ => return None,
            };
            if !hours
                .bytes()
                .chain(minutes.bytes())
                .all(|b| b.is_ascii_digit())
            {
                return None;
            }
            let hours: i64 = hours.parse().ok()?;
            let minutes: i64 = minutes.parse().ok()?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            sign * (hours * 3600 + minutes * 60)
        }
    };
    Some(date.and_time(time) - chrono::Duration::seconds(offset_seconds))
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
        assert_eq!(end(json!("")), None);
    }

    /// `String#to_datetime` reads the ISO 8601 forms servers write, not
    /// RFC 3339 alone, and RFC 2822; a missing zone is UTC.
    #[test]
    fn reads_times_as_to_datetime_does() {
        let at = |value: &str| to_datetime(value).map(|time| time.to_string());
        let cases = [
            ("2026-01-02T03:04:05Z", "2026-01-02 03:04:05"),
            ("2026-01-02T03:04:05.250+09:00", "2026-01-01 18:04:05.250"),
            ("2026-01-02T03:04:05", "2026-01-02 03:04:05"),
            ("2026-01-02T03:04", "2026-01-02 03:04:00"),
            ("2026-01-02T03:04Z", "2026-01-02 03:04:00"),
            ("2026-01-02 03:04:05 +0900", "2026-01-01 18:04:05"),
            ("2026-01-02T03:04:05-05", "2026-01-02 08:04:05"),
            ("2026-01-02T03:04:05 UTC", "2026-01-02 03:04:05"),
            ("2026-01-02", "2026-01-02 00:00:00"),
            ("20260102T030405Z", "2026-01-02 03:04:05"),
            ("20260102", "2026-01-02 00:00:00"),
            ("Fri, 02 Jan 2026 03:04:05 +0000", "2026-01-02 03:04:05"),
            ("  2026-01-02T03:04:05Z  ", "2026-01-02 03:04:05"),
        ];
        for (value, expected) in cases {
            assert_eq!(at(value).as_deref(), Some(expected), "{value}");
        }
        for value in [
            "",
            "  ",
            "soon",
            "2026-13-01",
            "2026-01-02T25:00",
            "2026-01-02Tx",
        ] {
            assert_eq!(at(value), None, "{value}");
        }
    }
}
