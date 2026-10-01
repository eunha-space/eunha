//! `Rule`: the server rules a report can cite, as `REST::RuleSerializer`
//! renders them.

use crate::api::mastodon::types::Rule;
use crate::error::AppResult;
use crate::state::AppState;

/// The rules with these ids, discarded ones included (`Rule.with_discarded`),
/// or, with `None`, the rules in force in their order (`Rule.ordered`).
pub async fn serialize(state: &AppState, ids: Option<&[i64]>) -> AppResult<Vec<Rule>> {
    let rows = sqlx::query!(
        r#"SELECT r.id, r.text, r.hint,
                  COALESCE(
                    (SELECT jsonb_object_agg(t.language, jsonb_build_object('text', t.text, 'hint', t.hint))
                     FROM rule_translations t WHERE t.rule_id = r.id),
                    '{}'::jsonb
                  ) AS "translations!"
           FROM rules r
           WHERE ($1::bigint[] IS NULL AND r.deleted_at IS NULL) OR r.id = ANY($1)
           ORDER BY r.priority, r.id"#,
        ids,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Rule {
            id: r.id.to_string(),
            text: r.text,
            hint: r.hint,
            translations: r.translations,
        })
        .collect())
}

/// `Report#validate_rule_ids`: every id names a rule, discarded or not.
pub async fn all_exist(state: &AppState, ids: &[i64]) -> AppResult<bool> {
    let found: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM rules WHERE id = ANY($1)"#,
        ids,
    )
    .fetch_one(&state.db)
    .await?;
    Ok(found as usize == ids.len())
}
