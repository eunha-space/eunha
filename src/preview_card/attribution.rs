//! Crediting a local account with the link cards it could not be credited
//! with before: when its attribution domains change, the cards that named
//! it as their author from a domain it did not list
//! (`unverified_author_account_id`) are its own if it lists that domain
//! now (`UpdateAccountService#process_attribution_domains`,
//! `UpdateLinkCardAttributionWorker`).

use crate::state::AppState;

/// `UpdateAccountService::PREVIEW_CARD_REATTRIBUTION_LIMIT`: how many of
/// the newest cards are looked at straight away; the rest are left to
/// [`UpdateLinkCardAttributionWorker`].
const REATTRIBUTION_LIMIT: i64 = 1_000;

/// How many cards the worker looks at a time
/// (`active_record_batches_enumerator`'s batch).
const BATCH: i64 = 100;

/// The host of a card's URL (`PreviewCard#domain`).
fn domain(url: &str) -> Option<String> {
    url::Url::parse(url).ok()?.host_str().map(str::to_lowercase)
}

/// Credit `account_id` with those of `cards` (id and URL) it can be
/// attributed from, as long as they still name it unverified.
async fn credit(
    state: &AppState,
    account_id: i64,
    attribution_domains: &[String],
    cards: &[(i64, String)],
) -> sqlx::Result<()> {
    let ids: Vec<i64> = cards
        .iter()
        .filter(|(_, url)| {
            domain(url)
                .is_some_and(|domain| super::can_be_attributed_from(attribution_domains, &domain))
        })
        .map(|(id, _)| *id)
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        r#"UPDATE preview_cards SET author_account_id = $2, unverified_author_account_id = NULL
           WHERE id = ANY($1) AND unverified_author_account_id = $2"#,
        &ids,
        account_id,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `UpdateAccountService#process_attribution_domains`, after the local
/// account `account_id`'s attribution domains changed: its thousand newest
/// unverified cards now, and the rest in [`UpdateLinkCardAttributionWorker`]
/// when there may be more.
pub async fn attribution_domains_changed(state: &AppState, account_id: i64) -> sqlx::Result<()> {
    let attribution_domains: Vec<String> = sqlx::query_scalar!(
        "SELECT attribution_domains FROM accounts WHERE id = $1",
        account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .flatten()
    .unwrap_or_default();
    let cards: Vec<(i64, String)> = sqlx::query!(
        r#"SELECT id, url FROM preview_cards WHERE unverified_author_account_id = $1
           ORDER BY id DESC LIMIT $2"#,
        account_id,
        REATTRIBUTION_LIMIT,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| (r.id, r.url))
    .collect();
    let more = cards.len() as i64 == REATTRIBUTION_LIMIT;
    credit(state, account_id, &attribution_domains, &cards).await?;
    if more {
        crate::jobs::push(state, UpdateLinkCardAttributionWorker { account_id }).await;
    }
    Ok(())
}

/// `UpdateLinkCardAttributionWorker`: every card that names the account as
/// its unverified author, a batch at a time.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct UpdateLinkCardAttributionWorker {
    pub account_id: i64,
}

impl crate::jobs::Job for UpdateLinkCardAttributionWorker {
    const KIND: &'static str = "UpdateLinkCardAttributionWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT;

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        let Some(attribution_domains) = sqlx::query_scalar!(
            "SELECT attribution_domains FROM accounts WHERE id = $1",
            self.account_id,
        )
        .fetch_optional(&state.db)
        .await?
        else {
            return Ok(());
        };
        let attribution_domains = attribution_domains.unwrap_or_default();
        let mut after = 0_i64;
        loop {
            let cards: Vec<(i64, String)> = sqlx::query!(
                r#"SELECT id, url FROM preview_cards
                   WHERE unverified_author_account_id = $1 AND id > $2
                   ORDER BY id LIMIT $3"#,
                self.account_id,
                after,
                BATCH,
            )
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .map(|r| (r.id, r.url))
            .collect();
            let Some(&(last, _)) = cards.last() else {
                return Ok(());
            };
            credit(state, self.account_id, &attribution_domains, &cards).await?;
            after = last;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_card_is_from_its_urls_host() {
        assert_eq!(
            super::domain("https://Blog.Example.com/post").as_deref(),
            Some("blog.example.com")
        );
        assert_eq!(super::domain("not a url"), None);
    }
}
