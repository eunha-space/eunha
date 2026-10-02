//! Mastodon's `TagSearchService`: hashtags by name, from Elasticsearch when
//! the instance has it, otherwise (or when it fails) `Tag.search_for`.

use unicode_normalization::UnicodeNormalization;

use crate::error::AppResult;
use crate::state::AppState;

/// A hashtag as a search returns it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FoundTag {
    pub id: i64,
    pub name: String,
    /// `Tag#display_name`: the capitalisation it was first used with.
    pub display_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub limit: i64,
    pub offset: i64,
    pub exclude_unreviewed: bool,
}

/// `TagSearchService#call`.
pub async fn search(state: &AppState, query: &str, options: &Options) -> AppResult<Vec<FoundTag>> {
    let query = query.trim();
    let query = query.strip_prefix('#').unwrap_or(query);
    search_for(state, query, options).await
}

/// `Tag.search_for`: listable tags whose normalized name starts with the
/// normalized term, shortest first; with `exclude_unreviewed`, only reviewed
/// ones unless the name is the term itself.
pub async fn search_for(
    state: &AppState,
    term: &str,
    options: &Options,
) -> AppResult<Vec<FoundTag>> {
    let normalized = normalize(term.trim());
    // `sanitize_sql_like`, then `matches(..., nil, true)`: a case-sensitive
    // LIKE on `lower(name)`, so that the B-tree index serves it.
    let pattern = format!("{}%", crate::search::sanitize_sql_like(&normalized));
    Ok(sqlx::query_as::<_, FoundTag>(
        "SELECT id, name, coalesce(display_name, name) AS display_name FROM tags \
         WHERE lower(name) LIKE lower($1) \
           AND (listable = TRUE OR listable IS NULL) \
           AND (NOT $4 OR lower(name) = lower($5) OR reviewed_at IS NOT NULL) \
         ORDER BY LENGTH(name) ASC, name ASC \
         LIMIT $2 OFFSET $3",
    )
    .bind(pattern)
    .bind(options.limit)
    .bind(options.offset)
    .bind(options.exclude_unreviewed)
    .bind(&normalized)
    .fetch_all(&state.db)
    .await?)
}

/// `Tag.find_normalized`: the tag this name names.
pub async fn find_normalized(state: &AppState, name: &str) -> AppResult<Option<FoundTag>> {
    Ok(sqlx::query_as::<_, FoundTag>(
        "SELECT id, name, coalesce(display_name, name) AS display_name FROM tags WHERE lower(name) = lower($1) LIMIT 1",
    )
    .bind(normalize(name))
    .fetch_optional(&state.db)
    .await?)
}

/// `Tag::HASHTAG_SEPARATORS`.
const SEPARATORS: [char; 4] = ['_', '\u{00B7}', '\u{30FB}', '\u{200C}'];

/// `HashtagNormalizer#normalize`: NFKC, lowercase, folded to ASCII where a
/// character has an ASCII form, and stripped of whatever
/// `HASHTAG_INVALID_CHARS_RE` rejects.
pub fn normalize(s: &str) -> String {
    let lowered = s.nfkc().collect::<String>().to_lowercase();
    let folded = ascii_fold(&lowered);
    folded
        .chars()
        .filter(|&c| {
            c.is_alphanumeric() || ('\u{0E47}'..='\u{0E4E}').contains(&c) || SEPARATORS.contains(&c)
        })
        .collect()
}

/// Lucene's `ASCIIFoldingFilter`, which Mastodon's `ASCIIFolding` ports, for
/// the characters a hashtag is likely to hold: a letter whose canonical
/// decomposition is an ASCII letter and combining marks folds to the letter,
/// and the ligatures and stroked letters that do not decompose are spelled
/// out. Anything else, every non-Latin script included, is left as it is.
fn ascii_fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
            continue;
        }
        if let Some(spelled) = fold_special(c) {
            out.push_str(spelled);
            continue;
        }
        let mut decomposed = std::iter::once(c).nfd();
        match decomposed.next() {
            Some(base)
                if base.is_ascii_alphabetic()
                    && decomposed.all(unicode_normalization::char::is_combining_mark) =>
            {
                out.push(base)
            }
            _ => out.push(c),
        }
    }
    out
}

fn fold_special(c: char) -> Option<&'static str> {
    Some(match c {
        'ß' => "ss",
        'æ' => "ae",
        'œ' => "oe",
        'ø' => "o",
        'đ' | 'ð' => "d",
        'ł' => "l",
        'þ' => "th",
        'ħ' => "h",
        'ı' => "i",
        'ŀ' => "l",
        'ŧ' => "t",
        'ĳ' => "ij",
        'ﬀ' => "ff",
        'ﬁ' => "fi",
        'ﬂ' => "fl",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_as_hashtag_normalizer() {
        // spec/lib/hashtag_normalizer_spec.rb
        assert_eq!(normalize("Ｓｙｎｔｈｗａｖｅ"), "synthwave");
        assert_eq!(normalize("ｼｰｻｲﾄﾞﾗｲﾅｰ"), "シーサイドライナー");
        assert_eq!(normalize("#foo"), "foo");
        assert_eq!(normalize("BLÅHAJ"), "blahaj");
        assert_eq!(normalize("ⓢｙｎｔｈｗａｖｅ"), "synthwave");
        assert_eq!(normalize("a·b"), "a·b");
        assert_eq!(normalize("foo bar!"), "foobar");
        assert_eq!(normalize("한국어"), "한국어");
        assert_eq!(normalize("Straße"), "strasse");
    }
}
