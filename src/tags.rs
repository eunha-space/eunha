//! `Tag.find_or_create_by_names`: a hashtag found by its normalized name, or
//! created with that name and the spelling it was written with as its
//! `display_name`.

use sqlx::PgPool;

/// `normalizes :display_name`: the spelling as written, without what
/// `HASHTAG_INVALID_CHARS_RE` rejects.
fn display_name(written: &str) -> String {
    const SEPARATORS: [char; 4] = ['_', '\u{00B7}', '\u{30FB}', '\u{200C}'];
    written
        .chars()
        .filter(|&c| {
            c.is_alphanumeric() || ('\u{0E47}'..='\u{0E4E}').contains(&c) || SEPARATORS.contains(&c)
        })
        .collect()
}

/// The id of the tag `written` names (a leading `#` is ignored), creating it
/// if there is none. `None` when nothing of the name survives normalization.
pub async fn find_or_create(db: &PgPool, written: &str) -> sqlx::Result<Option<i64>> {
    let written = written.trim().trim_start_matches('#');
    let name = crate::search::tags::normalize(written);
    if name.is_empty() {
        return Ok(None);
    }
    let display = display_name(written);
    let id = sqlx::query_scalar!(
        r#"WITH created AS (
             INSERT INTO tags (name, display_name, created_at, updated_at)
             VALUES ($1, $2, now(), now())
             ON CONFLICT ((lower(name))) DO NOTHING
             RETURNING id
           )
           SELECT id AS "id!" FROM created
           UNION ALL
           SELECT id FROM tags WHERE lower(name) = lower($1)
           LIMIT 1"#,
        name,
        display,
    )
    .fetch_optional(db)
    .await?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    #[test]
    fn keeps_the_spelling_without_invalid_characters() {
        assert_eq!(super::display_name("Café-Rust!"), "CaféRust");
        assert_eq!(super::display_name("日本_語"), "日本_語");
    }
}
