//! Search, as Mastodon's `SearchService` and the services it calls do it.
//!
//! Without Elasticsearch, accounts are found with PostgreSQL's text search,
//! hashtags by name prefix, and posts not at all — only a post's URL resolves.
//! docs/operating/search.md describes both backends.

pub mod accounts;
pub mod peers;
pub mod query;
pub mod tags;

use crate::state::AppState;

/// `SearchService::QUOTE_EQUIVALENT_CHARACTERS`, each read as `"`.
const QUOTE_EQUIVALENTS: [char; 11] = ['“', '”', '„', '«', '»', '「', '」', '『', '』', '《', '》'];

/// `query&.strip&.gsub(QUOTE_EQUIVALENT_CHARACTERS, '"')`.
pub fn normalize_query(query: &str) -> String {
    query
        .trim()
        .chars()
        .map(|c| {
            if QUOTE_EQUIVALENTS.contains(&c) {
                '"'
            } else {
                c
            }
        })
        .collect()
}

/// `TagManager#local_domain?`: this instance's domain, a trailing slash and
/// case aside.
pub fn is_local_domain(state: &AppState, domain: &str) -> bool {
    domain
        .strip_suffix('/')
        .unwrap_or(domain)
        .eq_ignore_ascii_case(&state.instance.domain)
}

/// ActiveRecord's `sanitize_sql_like`: `\`, `%` and `_` escaped with `\`.
pub fn sanitize_sql_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Ruby's `String#to_i`: the leading integer, or zero.
pub fn ruby_to_i(s: &str) -> i64 {
    let s = s.trim_start();
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut value: i64 = 0;
    for (i, c) in digits.chars().enumerate() {
        if c == '_' && i > 0 {
            continue;
        }
        let Some(d) = c.to_digit(10) else { break };
        value = value.saturating_mul(10).saturating_add(d as i64);
    }
    if negative {
        -value
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_equivalents_become_quotes() {
        assert_eq!(normalize_query("  «hello» “world” "), "\"hello\" \"world\"");
    }

    #[test]
    fn like_metacharacters_are_escaped() {
        assert_eq!(sanitize_sql_like(r"a_b%c\d"), r"a\_b\%c\\d");
    }

    #[test]
    fn to_i_reads_like_ruby() {
        assert_eq!(ruby_to_i("12abc"), 12);
        assert_eq!(ruby_to_i("abc"), 0);
        assert_eq!(ruby_to_i("-5"), -5);
        assert_eq!(ruby_to_i(" 7"), 7);
        assert_eq!(ruby_to_i(""), 0);
    }
}
