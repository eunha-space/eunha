//! Mastodon's post search syntax: `SearchQueryParser`, a Parslet grammar,
//! and `SearchQueryTransformer`, which turns what it parses into an
//! Elasticsearch request.
//!
//! The parser is a hand-written PEG with Parslet's semantics: an optional
//! part that matched is never retried empty, and the whole query must be
//! consumed or the parse fails. [`parse`] gives the clauses, [`Query::new`]
//! sorts them by what they do, and [`Query::request`] builds the request body
//! once `from:` handles have been looked up.

use serde_json::{json, Value};

/// A parsed clause, before the transformer reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Clause {
        operator: Option<char>,
        prefix: Option<String>,
        body: Body,
    },
    /// A stray `"` (`quote.as(:junk)`), which the transformer drops.
    Junk,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Term(String),
    /// The words of a quoted phrase, without the quotes.
    Phrase(Vec<String>),
    /// `:name:`, an emoji shortcode.
    Shortcode,
}

/// Why a query was not searched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// `Parslet::ParseFailed`: the grammar did not consume the query. The
    /// search returns nothing, as Mastodon's rescue does.
    ParseFailed,
    /// `Date::Error` from a `before:`, `after:` or `during:` that is not a
    /// date. Mastodon answers 422, "Invalid date supplied".
    InvalidDate,
    /// A query Mastodon's transformer raises on (a bare `:shortcode:`, an
    /// empty `""`), answering 500. Eunha returns nothing instead.
    Unsupported,
}

struct Parser<'a> {
    input: &'a [char],
}

fn is_space(c: char) -> bool {
    c.is_whitespace()
}

impl Parser<'_> {
    /// `term`: `[^\s":]+`.
    fn term(&self, at: usize) -> Option<(String, usize)> {
        let mut end = at;
        while end < self.input.len()
            && !matches!(self.input[end], '"' | ':')
            && !is_space(self.input[end])
        {
            end += 1;
        }
        (end > at).then(|| (self.input[at..end].iter().collect(), end))
    }

    fn char_at(&self, at: usize, c: char) -> Option<usize> {
        (self.input.get(at) == Some(&c)).then_some(at + 1)
    }

    /// `space`: `\s+`.
    fn space(&self, at: usize) -> Option<usize> {
        let mut end = at;
        while end < self.input.len() && is_space(self.input[end]) {
            end += 1;
        }
        (end > at).then_some(end)
    }

    /// `prefix`: `term >> colon`.
    fn prefix(&self, at: usize) -> Option<(String, usize)> {
        let (term, end) = self.term(at)?;
        Some((term, self.char_at(end, ':')?))
    }

    /// `shortcode`: `colon >> term >> colon.maybe`.
    fn shortcode(&self, at: usize) -> Option<(String, usize)> {
        let at = self.char_at(at, ':')?;
        let (term, end) = self.term(at)?;
        Some((term, self.char_at(end, ':').unwrap_or(end)))
    }

    /// `phrase`: `quote >> ([^\s"]+ >> space.maybe).repeat >> quote`.
    fn phrase(&self, at: usize) -> Option<(Vec<String>, usize)> {
        let mut at = self.char_at(at, '"')?;
        let mut words = Vec::new();
        loop {
            let mut end = at;
            while end < self.input.len() && self.input[end] != '"' && !is_space(self.input[end]) {
                end += 1;
            }
            if end == at {
                break;
            }
            words.push(self.input[at..end].iter().collect());
            at = self.space(end).unwrap_or(end);
        }
        Some((words, self.char_at(at, '"')?))
    }

    /// `clause`: `(operator.maybe >> prefix.maybe >> (phrase | term |
    /// shortcode)) | prefix | quote`.
    fn clause(&self, at: usize) -> Option<(Parsed, usize)> {
        if let Some(found) = self.full_clause(at) {
            return Some(found);
        }
        if let Some((term, end)) = self.prefix(at) {
            return Some((
                Parsed::Clause {
                    operator: None,
                    prefix: None,
                    body: Body::Term(term),
                },
                end,
            ));
        }
        self.char_at(at, '"').map(|end| (Parsed::Junk, end))
    }

    fn full_clause(&self, at: usize) -> Option<(Parsed, usize)> {
        let (operator, at) = match self.input.get(at) {
            Some(&c @ ('+' | '-')) => (Some(c), at + 1),
            _ => (None, at),
        };
        let (prefix, at) = match self.prefix(at) {
            Some((prefix, end)) => (Some(prefix), end),
            None => (None, at),
        };
        let (body, end) = if let Some((words, end)) = self.phrase(at) {
            (Body::Phrase(words), end)
        } else if let Some((term, end)) = self.term(at) {
            (Body::Term(term), end)
        } else if let Some((_, end)) = self.shortcode(at) {
            (Body::Shortcode, end)
        } else {
            return None;
        };
        Some((
            Parsed::Clause {
                operator,
                prefix,
                body,
            },
            end,
        ))
    }
}

/// `SearchQueryParser#parse`: `(clause >> space.maybe).repeat`, consuming
/// all of `query`.
pub fn parse(query: &str) -> Result<Vec<Parsed>, QueryError> {
    let input: Vec<char> = query.chars().collect();
    let parser = Parser { input: &input };
    let mut at = 0;
    let mut clauses = Vec::new();
    while let Some((clause, end)) = parser.clause(at) {
        clauses.push(clause);
        at = parser.space(end).unwrap_or(end);
    }
    if at == input.len() {
        Ok(clauses)
    } else {
        Err(QueryError::ParseFailed)
    }
}

/// `SUPPORTED_PREFIXES`.
const SUPPORTED_PREFIXES: [&str; 8] = [
    "has", "is", "language", "from", "before", "after", "during", "in",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    Must,
    MustNot,
}

impl Operator {
    /// `Operator.symbol`.
    fn from(c: Option<char>) -> Self {
        match c {
            Some('-') => Operator::MustNot,
            _ => Operator::Must,
        }
    }
}

/// `TermClause`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermClause {
    pub operator: Operator,
    pub term: String,
}

impl TermClause {
    fn to_query(&self) -> Value {
        if self.term.starts_with('#') {
            json!({ "match": { "tags": { "query": self.term, "operator": "and" } } })
        } else {
            json!({ "multi_match": {
                "type": "most_fields",
                "query": self.term,
                "fields": ["text", "text.stemmed"],
                "operator": "and",
            } })
        }
    }
}

/// `PhraseClause`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhraseClause {
    pub operator: Operator,
    pub phrase: String,
}

impl PhraseClause {
    fn to_query(&self) -> Value {
        json!({ "match_phrase": { "text": { "query": self.phrase } } })
    }
}

/// What a `PrefixClause` filters on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// `has:` and `is:`, a term on `properties`.
    Property(String),
    /// `language:`, a term on `language`.
    Language(String),
    /// `from:`, a term on `account_id` once the handle is looked up: `me`, or
    /// `user` / `user@domain`.
    From(String),
    /// `before:` (`lt`), `after:` (`gt`) and `during:` (`gte` and `lte`), a
    /// range on `created_at`.
    Date { bounds: Vec<(&'static str, String)> },
}

/// `PrefixClause` with a filtering prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixClause {
    pub prefix: String,
    pub negated: bool,
    pub filter: Filter,
}

impl PrefixClause {
    /// The term as the specs read it: what is matched, or the date bounds.
    pub fn term(&self) -> String {
        match &self.filter {
            Filter::Property(t) | Filter::Language(t) | Filter::From(t) => t.clone(),
            Filter::Date { bounds } => bounds
                .iter()
                .map(|(op, v)| format!("{op}:{v}"))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

/// `SearchQueryTransformer::Query`: the clauses sorted by what they do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub must_terms: Vec<TermClause>,
    pub must_not_terms: Vec<TermClause>,
    pub must_phrases: Vec<PhraseClause>,
    pub must_not_phrases: Vec<PhraseClause>,
    pub filters: Vec<PrefixClause>,
    /// `in:`, the last one given.
    pub scope: Option<String>,
}

/// `PrefixClause::EPOCH_RE`.
fn is_epoch(term: &str) -> bool {
    !term.is_empty() && term.chars().all(|c| c.is_ascii_digit())
}

/// `date_from_term`: the term as given, once `DateTime.iso8601` accepts it
/// or it is all digits.
fn date_from_term(term: &str) -> Result<String, QueryError> {
    if is_epoch(term) || is_iso8601(term) {
        Ok(term.to_string())
    } else {
        Err(QueryError::InvalidDate)
    }
}

/// The forms `DateTime.iso8601` accepts: a calendar (`2022-01-01`,
/// `20220101`), ordinal (`2022-001`) or week (`2022-W01-1`) date, optionally
/// followed by `T` and a time with an optional fraction and zone.
fn is_iso8601(term: &str) -> bool {
    let (date, time) = match term.split_once(['T', 't']) {
        Some((d, t)) => (d, Some(t)),
        None => (term, None),
    };
    let digits = |s: &str, n: usize| s.len() == n && s.chars().all(|c| c.is_ascii_digit());
    let date_ok = {
        let parts: Vec<&str> = date.split('-').collect();
        match parts.as_slice() {
            [y, m, d] if digits(y, 4) && digits(m, 2) && digits(d, 2) => {
                chrono::NaiveDate::from_ymd_opt(
                    y.parse().unwrap_or(0),
                    m.parse().unwrap_or(0),
                    d.parse().unwrap_or(0),
                )
                .is_some()
            }
            [y, o] if digits(y, 4) && digits(o, 3) => {
                chrono::NaiveDate::from_yo_opt(y.parse().unwrap_or(0), o.parse().unwrap_or(0))
                    .is_some()
            }
            [y, w, d]
                if digits(y, 4)
                    && w.len() == 3
                    && w.starts_with('W')
                    && digits(&w[1..], 2)
                    && digits(d, 1) =>
            {
                let weekday = match d.parse::<u32>().unwrap_or(0) {
                    1 => chrono::Weekday::Mon,
                    2 => chrono::Weekday::Tue,
                    3 => chrono::Weekday::Wed,
                    4 => chrono::Weekday::Thu,
                    5 => chrono::Weekday::Fri,
                    6 => chrono::Weekday::Sat,
                    7 => chrono::Weekday::Sun,
                    _ => return false,
                };
                chrono::NaiveDate::from_isoywd_opt(
                    y.parse().unwrap_or(0),
                    w[1..].parse().unwrap_or(0),
                    weekday,
                )
                .is_some()
            }
            [compact] if digits(compact, 8) => {
                chrono::NaiveDate::parse_from_str(compact, "%Y%m%d").is_ok()
            }
            _ => false,
        }
    };
    if !date_ok {
        return false;
    }
    let Some(time) = time else { return true };
    // Split the zone off: `Z`, or `+hh:mm`, `+hhmm`, `+hh`.
    let (clock, zone) = match time.find(['Z', 'z', '+', '-']) {
        Some(i) => (&time[..i], Some(&time[i..])),
        None => (time, None),
    };
    let zone_ok = match zone {
        None | Some("Z" | "z") => true,
        Some(z) => {
            let rest = z[1..].replace(':', "");
            (rest.len() == 2 || rest.len() == 4) && rest.chars().all(|c| c.is_ascii_digit())
        }
    };
    let (clock, fraction) = match clock.split_once(['.', ',']) {
        Some((c, f)) => (c, Some(f)),
        None => (clock, None),
    };
    let fraction_ok =
        fraction.is_none_or(|f| !f.is_empty() && f.chars().all(|c| c.is_ascii_digit()));
    let clock_ok = ["%H:%M:%S", "%H:%M", "%H%M%S", "%H%M"]
        .iter()
        .any(|f| chrono::NaiveTime::parse_from_str(clock, f).is_ok());
    zone_ok && fraction_ok && clock_ok
}

/// `language_code_from_term`: the code as Mastodon knows it, trying it as
/// given, lowercased, then the part before `_` or `-`.
fn language_code_from_term(term: &str) -> String {
    let known = |code: &str| crate::languages::valid_locale(Some(code));
    if known(term) {
        return term.to_string();
    }
    let lower = term.to_lowercase();
    if known(&lower) {
        return lower;
    }
    let base = term.split(['_', '-']).next().unwrap_or("").to_lowercase();
    if known(&base) {
        return base;
    }
    term.to_string()
}

impl Query {
    /// The transform rules, then `Query#initialize`.
    pub fn new(parsed: Vec<Parsed>) -> Result<Self, QueryError> {
        let mut query = Query::default();
        for clause in parsed {
            let Parsed::Clause {
                operator,
                prefix,
                body,
            } = clause
            else {
                continue;
            };
            let prefix = prefix.map(|p| p.to_lowercase());
            let term = match &body {
                Body::Phrase(words) => words.join(" "),
                Body::Term(t) => t.clone(),
                Body::Shortcode => String::new(),
            };
            match prefix {
                Some(prefix) if SUPPORTED_PREFIXES.contains(&prefix.as_str()) => {
                    query.add_prefix(&prefix, operator, term)?;
                }
                Some(prefix) => query.add_term(operator, format!("{prefix} {term}")),
                None => match body {
                    Body::Term(_) => query.add_term(operator, term),
                    Body::Phrase(words) if !words.is_empty() => {
                        let phrase = PhraseClause {
                            operator: Operator::from(operator),
                            phrase: term,
                        };
                        match phrase.operator {
                            Operator::Must => query.must_phrases.push(phrase),
                            Operator::MustNot => query.must_not_phrases.push(phrase),
                        }
                    }
                    _ => return Err(QueryError::Unsupported),
                },
            }
        }
        Ok(query)
    }

    fn add_term(&mut self, operator: Option<char>, term: String) {
        let clause = TermClause {
            operator: Operator::from(operator),
            term,
        };
        match clause.operator {
            Operator::Must => self.must_terms.push(clause),
            Operator::MustNot => self.must_not_terms.push(clause),
        }
    }

    /// `PrefixClause#initialize`.
    fn add_prefix(
        &mut self,
        prefix: &str,
        operator: Option<char>,
        term: String,
    ) -> Result<(), QueryError> {
        let filter = match prefix {
            "has" | "is" => Filter::Property(term),
            "language" => Filter::Language(language_code_from_term(&term)),
            "from" => Filter::From(term),
            "before" => Filter::Date {
                bounds: vec![("lt", date_from_term(&term)?)],
            },
            "after" => Filter::Date {
                bounds: vec![("gt", date_from_term(&term)?)],
            },
            "during" => {
                let date = date_from_term(&term)?;
                Filter::Date {
                    bounds: vec![("gte", date.clone()), ("lte", date)],
                }
            }
            "in" => {
                self.scope = Some(term);
                return Ok(());
            }
            _ => unreachable!("only supported prefixes reach here"),
        };
        self.filters.push(PrefixClause {
            prefix: prefix.to_string(),
            negated: operator == Some('-'),
            filter,
        });
        Ok(())
    }

    /// The `from:` handles that need an account id.
    pub fn from_terms(&self) -> Vec<String> {
        self.filters
            .iter()
            .filter_map(|f| match &f.filter {
                Filter::From(term) => Some(term.clone()),
                _ => None,
            })
            .collect()
    }

    /// `Query#indexes`: which of the two post indexes `in:` names, as base
    /// names.
    pub fn indexes(&self) -> &'static [&'static str] {
        match self.scope.as_deref() {
            Some("library") => &["statuses"],
            Some("public") => &["public_statuses"],
            _ => &["public_statuses", "statuses"],
        }
    }

    /// `Query#request`: the query part of the request body. `index_name`
    /// gives an index's prefixed name, `account_id` looks up a `from:`
    /// handle (`-1` for nobody), and `time_zone` is the searcher's.
    pub fn request(
        &self,
        viewer_id: i64,
        time_zone: &str,
        index_name: impl Fn(&str) -> String,
        account_id: impl Fn(&str) -> i64,
    ) -> Value {
        let default_filter = json!({
            "bool": {
                "should": [
                    { "term": { "_index": index_name("public_statuses") } },
                    { "bool": { "must": [
                        { "term": { "_index": index_name("statuses") } },
                        { "term": { "searchable_by": viewer_id } },
                    ] } },
                ],
                "minimum_should_match": 1,
            }
        });
        let must: Vec<Value> = self
            .must_terms
            .iter()
            .map(TermClause::to_query)
            .chain(self.must_phrases.iter().map(PhraseClause::to_query))
            .collect();
        let must_not: Vec<Value> = self
            .must_not_terms
            .iter()
            .map(TermClause::to_query)
            .chain(self.must_not_phrases.iter().map(PhraseClause::to_query))
            .collect();
        let mut filter = vec![default_filter];
        for clause in &self.filters {
            let inner = match &clause.filter {
                Filter::Property(t) => json!({ "term": { "properties": t } }),
                Filter::Language(t) => json!({ "term": { "language": t } }),
                Filter::From(t) => json!({ "term": { "account_id": account_id(t) } }),
                Filter::Date { bounds } => {
                    let mut range = serde_json::Map::new();
                    for (op, value) in bounds {
                        range.insert((*op).into(), Value::from(value.clone()));
                    }
                    range.insert("time_zone".into(), Value::from(time_zone));
                    json!({ "range": { "created_at": range } })
                }
            };
            filter.push(if clause.negated {
                json!({ "bool": { "must_not": inner } })
            } else {
                inner
            });
        }
        json!({ "bool": { "must": must, "must_not": must_not, "filter": filter } })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parses(s: &str) -> bool {
        parse(s).is_ok()
    }

    fn query(s: &str) -> Query {
        Query::new(parse(s).unwrap()).unwrap()
    }

    fn terms(clauses: &[TermClause]) -> Vec<&str> {
        let mut t: Vec<&str> = clauses.iter().map(|c| c.term.as_str()).collect();
        t.sort();
        t
    }

    // spec/lib/search_query_parser_spec.rb

    #[test]
    fn parser_rules() {
        let input: Vec<char> = "hello".chars().collect();
        assert_eq!(Parser { input: &input }.term(0), Some(("hello".into(), 5)));
        let input: Vec<char> = "foo:".chars().collect();
        assert_eq!(Parser { input: &input }.prefix(0), Some(("foo".into(), 4)));
        let input: Vec<char> = ":foo:".chars().collect();
        assert_eq!(
            Parser { input: &input }.shortcode(0),
            Some(("foo".into(), 5))
        );
        let input: Vec<char> = "\"hello world\"".chars().collect();
        assert_eq!(
            Parser { input: &input }.phrase(0),
            Some((vec!["hello".into(), "world".into()], 13))
        );
    }

    #[test]
    fn parser_clauses() {
        for clause in [
            "foo",
            "-foo",
            "foo:bar",
            "-foo:bar",
            "foo:\"hello world\"",
            "-foo:\"hello world\"",
            "foo:",
            "\"",
            "+",
        ] {
            let input: Vec<char> = clause.chars().collect();
            let parser = Parser { input: &input };
            if clause == "+" {
                assert!(parser.clause(0).is_none(), "{clause}");
                continue;
            }
            let (_, end) = parser.clause(0).unwrap_or_else(|| panic!("{clause}"));
            assert_eq!(end, input.len(), "{clause}");
        }
    }

    #[test]
    fn parser_queries() {
        for q in [
            "hello -world",
            "foo \"hello world\"",
            "foo:bar hello",
            "\"hello\" world \"",
            "foo:bar bar: hello",
        ] {
            assert!(parses(q), "{q}");
        }
        assert!(!parses(": x"));
        assert!(!parses("-"));
    }

    // spec/lib/search_query_transformer_spec.rb

    #[test]
    fn hello_world() {
        let q = query("hello world");
        assert_eq!(terms(&q.must_terms), ["hello", "world"]);
        assert!(q.must_not_terms.is_empty() && q.filters.is_empty());
    }

    #[test]
    fn hello_minus_world() {
        let q = query("hello -world");
        assert_eq!(terms(&q.must_terms), ["hello"]);
        assert_eq!(terms(&q.must_not_terms), ["world"]);
        assert!(q.filters.is_empty());
    }

    #[test]
    fn hello_is_reply() {
        let q = query("hello is:reply");
        assert_eq!(terms(&q.must_terms), ["hello"]);
        assert!(q.must_not_terms.is_empty());
        assert_eq!(
            q.filters.iter().map(PrefixClause::term).collect::<Vec<_>>(),
            ["reply"]
        );
    }

    #[test]
    fn foo_colon_space_bar() {
        let q = query("foo: bar");
        assert_eq!(terms(&q.must_terms), ["bar", "foo"]);
        assert!(q.filters.is_empty());
    }

    #[test]
    fn unknown_prefix_is_a_term() {
        let q = query("foo:bar");
        assert_eq!(terms(&q.must_terms), ["foo bar"]);
        assert!(q.filters.is_empty());
    }

    #[test]
    fn phrase() {
        let q = query("\"hello world\"");
        assert_eq!(q.must_phrases[0].phrase, "hello world");
        assert!(q.must_terms.is_empty() && q.filters.is_empty());
    }

    #[test]
    fn prefix_with_phrase() {
        let q = query("is:\"foo bar\"");
        assert!(q.must_terms.is_empty() && q.must_phrases.is_empty());
        assert_eq!(q.filters[0].term(), "foo bar");
    }

    #[test]
    fn date_operators() {
        for (operator, ops) in [
            ("before", vec!["lt"]),
            ("after", vec!["gt"]),
            ("during", vec!["gte", "lte"]),
        ] {
            for (value, parsed) in [
                ("2022-01-01", "2022-01-01"),
                ("\"2022-01-01\"", "2022-01-01"),
                ("12345678", "12345678"),
                ("\"12345678\"", "12345678"),
                ("\"2024-10-31T23:47:20Z\"", "2024-10-31T23:47:20Z"),
            ] {
                let q = query(&format!("{operator}:{value}"));
                assert!(q.must_terms.is_empty() && q.must_not_terms.is_empty());
                let Filter::Date { bounds } = &q.filters[0].filter else {
                    panic!("{operator}:{value}");
                };
                assert_eq!(
                    bounds.iter().map(|(op, _)| *op).collect::<Vec<_>>(),
                    ops,
                    "{operator}:{value}"
                );
                assert!(
                    bounds.iter().all(|(_, v)| v == parsed),
                    "{operator}:{value}"
                );
                let body = q.request(1, "UTC", str::to_string, |_| 1);
                assert_eq!(
                    body["bool"]["filter"][1]["range"]["created_at"]["time_zone"],
                    "UTC"
                );
            }
            assert_eq!(
                Query::new(parse(&format!("{operator}:\"abc\"")).unwrap()),
                Err(QueryError::InvalidDate)
            );
        }
    }

    #[test]
    fn multiple_prefix_clauses_before_a_term() {
        for s in ["from:me has:media foo", "from:me foo has:media"] {
            let q = query(s);
            assert_eq!(terms(&q.must_terms), ["foo"], "{s}");
            assert!(q.must_not_terms.is_empty());
            let mut prefixes: Vec<&str> = q.filters.iter().map(|f| f.prefix.as_str()).collect();
            prefixes.sort();
            assert_eq!(prefixes, ["from", "has"], "{s}");
        }
    }

    // What the transformer does beyond its specs.

    #[test]
    fn negated_prefixes_and_scopes() {
        let q = query("-is:reply in:library -in:public cats");
        assert!(q.filters[0].negated);
        assert_eq!(q.scope.as_deref(), Some("public"), "the last in: wins");
        assert_eq!(q.indexes(), ["public_statuses"]);
        let body = q.request(7, "UTC", |n| format!("p_{n}"), |_| -1);
        assert_eq!(
            body["bool"]["filter"][1],
            json!({ "bool": { "must_not": { "term": { "properties": "reply" } } } })
        );
        assert_eq!(
            body["bool"]["filter"][0]["bool"]["should"][1]["bool"]["must"][1],
            json!({ "term": { "searchable_by": 7 } })
        );
        assert_eq!(
            body["bool"]["filter"][0]["bool"]["should"][0],
            json!({ "term": { "_index": "p_public_statuses" } })
        );
        assert_eq!(query("cats").indexes(), ["public_statuses", "statuses"]);
        assert_eq!(query("in:library cats").indexes(), ["statuses"]);
    }

    #[test]
    fn hashtags_match_the_tags_field() {
        let body = query("#Cats dogs").request(1, "UTC", str::to_string, |_| 1);
        assert_eq!(
            body["bool"]["must"][0],
            json!({ "match": { "tags": { "query": "#Cats", "operator": "and" } } })
        );
        assert_eq!(
            body["bool"]["must"][1]["multi_match"]["fields"],
            json!(["text", "text.stemmed"])
        );
    }

    #[test]
    fn language_codes() {
        assert_eq!(language_code_from_term("en"), "en");
        assert_eq!(language_code_from_term("EN"), "en");
        assert_eq!(language_code_from_term("en_US"), "en");
        assert_eq!(language_code_from_term("nan-TW"), "nan-TW");
        assert_eq!(language_code_from_term("klingon"), "klingon");
    }

    #[test]
    fn from_terms_are_looked_up() {
        let q = query("from:@alice@example.com from:me");
        assert_eq!(q.from_terms(), ["@alice@example.com", "me"]);
        let body = q.request(1, "UTC", str::to_string, |t| if t == "me" { 1 } else { 42 });
        assert_eq!(body["bool"]["filter"][1]["term"]["account_id"], 42);
        assert_eq!(body["bool"]["filter"][2]["term"]["account_id"], 1);
    }

    #[test]
    fn queries_mastodon_crashes_on_return_nothing() {
        assert_eq!(
            Query::new(parse(":blobcat:").unwrap()),
            Err(QueryError::Unsupported)
        );
        assert_eq!(
            Query::new(parse("\"\"").unwrap()),
            Err(QueryError::Unsupported)
        );
    }

    #[test]
    fn iso8601_forms() {
        for ok in [
            "2022-01-01",
            "20220101",
            "2022-032",
            "2022-W05-3",
            "2022-01-01T10:00",
            "2022-01-01T10:00:00.123+09:00",
            "2022-01-01T100000Z",
        ] {
            assert!(is_iso8601(ok), "{ok}");
        }
        for bad in [
            "abc",
            "2022-13-01",
            "2022-02-30",
            "2022/01/01",
            "2022-01-01T25:00",
        ] {
            assert!(!is_iso8601(bad), "{bad}");
        }
    }
}
