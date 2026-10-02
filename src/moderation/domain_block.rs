//! `BlockDomainService`, `UnblockDomainService` and `ClearDomainMediaService`:
//! what an admin domain block does to the accounts already known from it.

use anyhow::Result;

use crate::db::models::domain_severity;
use crate::state::AppState;

struct Block {
    id: i64,
    domain: String,
    severity: Option<i32>,
    reject_media: bool,
    created_at: chrono::NaiveDateTime,
}

async fn load(state: &AppState, id: i64) -> Result<Option<Block>> {
    Ok(sqlx::query_as!(
        Block,
        "SELECT id, domain, severity, reject_media, created_at FROM domain_blocks WHERE id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await?)
}

/// `Account.by_domain_and_subdomains(domain)` as a SQL condition on `a.domain`.
const BY_DOMAIN: &str = "(a.domain = $1 OR a.domain LIKE '%.' || $1)";

/// `DomainBlockWorker` → `BlockDomainService#call(domain_block, update:)`.
pub async fn block(state: &AppState, id: i64, update: bool) -> Result<()> {
    let Some(block) = load(state, id).await? else {
        return Ok(());
    };
    let severity = block.severity.unwrap_or(domain_severity::SILENCE);
    let mut event = None;

    // `process_domain_block!`
    if severity == domain_severity::SILENCE {
        sqlx::query(&format!(
            "UPDATE accounts a SET silenced_at = $2 WHERE {BY_DOMAIN} AND a.silenced_at IS NULL"
        ))
        .bind(&block.domain)
        .bind(block.created_at)
        .execute(&state.db)
        .await?;
    } else if severity == domain_severity::SUSPEND {
        event = Some(suspend_accounts(state, &block).await?);
    }

    if update {
        // `process_retroactive_updates!`: undo what an earlier severity did.
        if severity != domain_severity::SILENCE {
            unsilence_from(state, &block).await?;
        }
        if severity != domain_severity::SUSPEND {
            unsuspend_from(state, &block).await?;
        }
    }

    if severity == domain_severity::SUSPEND {
        // `PurgeCustomEmojiWorker`: account images and attachments went with
        // the accounts.
        sqlx::query("DELETE FROM custom_emojis WHERE domain = $1 OR domain LIKE '%.' || $1")
            .bind(&block.domain)
            .execute(&state.db)
            .await?;
    } else if block.reject_media {
        clear_media(state, block.id).await?;
    }

    // `notify_of_severed_relationships!`
    if let Some(event_id) = event {
        super::severance::notify_affected(state, event_id).await?;
    }
    Ok(())
}

/// `suspend_accounts!`: suspend every account from the domain not already
/// unavailable, then purge each, recording the follows it severs.
async fn suspend_accounts(state: &AppState, block: &Block) -> Result<i64> {
    sqlx::query(&format!(
        "UPDATE accounts a SET suspended_at = $2, suspension_origin = 0
         WHERE {BY_DOMAIN} AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL"
    ))
    .bind(&block.domain)
    .bind(block.created_at)
    .execute(&state.db)
    .await?;
    let event_id = super::severance::create(
        &state.db,
        super::severance::kind::DOMAIN_BLOCK,
        &block.domain,
    )
    .await?;
    let accounts: Vec<i64> = sqlx::query_scalar(&format!(
        "SELECT a.id FROM accounts a WHERE {BY_DOMAIN} AND a.suspended_at = $2 ORDER BY a.id"
    ))
    .bind(&block.domain)
    .bind(block.created_at)
    .fetch_all(&state.db)
    .await?;
    for account_id in accounts {
        crate::delete_account::call(
            state,
            account_id,
            crate::delete_account::Options {
                reserve_username: true,
                suspended_at: Some(block.created_at),
                relationship_severance_event: Some(event_id),
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(event_id)
}

async fn unsilence_from(state: &AppState, block: &Block) -> Result<()> {
    sqlx::query(&format!(
        "UPDATE accounts a SET silenced_at = NULL WHERE {BY_DOMAIN} AND a.silenced_at = $2"
    ))
    .bind(&block.domain)
    .bind(block.created_at)
    .execute(&state.db)
    .await?;
    Ok(())
}

async fn unsuspend_from(state: &AppState, block: &Block) -> Result<()> {
    sqlx::query(&format!(
        "UPDATE accounts a SET suspended_at = NULL, suspension_origin = NULL
         WHERE {BY_DOMAIN} AND a.suspended_at = $2"
    ))
    .bind(&block.domain)
    .bind(block.created_at)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `UnblockDomainService#call`: lift what the block did, then remove it.
pub async fn unblock(state: &AppState, id: i64) -> Result<()> {
    let Some(block) = load(state, id).await? else {
        return Ok(());
    };
    let severity = block.severity.unwrap_or(domain_severity::SILENCE);
    if severity != domain_severity::NOOP {
        unsilence_from(state, &block).await?;
    }
    if severity == domain_severity::SUSPEND {
        unsuspend_from(state, &block).await?;
    }
    sqlx::query!("DELETE FROM domain_blocks WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    Ok(())
}

/// `UnallowDomainService#suspend_accounts!`, in limited federation mode: the
/// accounts of a domain taken off the allow list are suspended at once, before
/// [`after_unallow`] deletes them. Only the domain itself, as Mastodon's
/// `Account.where(domain:)` has it, and every account of it, suspended before
/// or not, from now.
pub async fn suspend_unallowed(state: &AppState, domain: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE accounts SET suspended_at = now(), updated_at = now() WHERE domain = $1",
        domain
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `AfterUnallowDomainService`: delete every account of a domain taken off
/// the allow list in limited federation mode, without reserving its username.
pub async fn after_unallow(state: &AppState, domain: &str) -> Result<()> {
    if domain.is_empty() {
        return Ok(());
    }
    let accounts = sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE domain = $1 ORDER BY id",
        domain
    )
    .fetch_all(&state.db)
    .await?;
    for account_id in accounts {
        crate::delete_account::call(
            state,
            account_id,
            crate::delete_account::Options {
                reserve_username: false,
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(())
}

/// `ClearDomainMediaService`: forget the cached images and attachments of the
/// domain's accounts (their remote URLs stay), and its custom emoji.
pub async fn clear_media(state: &AppState, id: i64) -> Result<()> {
    let Some(block) = load(state, id).await? else {
        return Ok(());
    };
    if !block.reject_media {
        return Ok(());
    }
    sqlx::query(&format!(
        "UPDATE accounts a SET avatar_file_name = NULL, avatar_content_type = NULL,
                avatar_file_size = NULL, avatar_updated_at = NULL,
                header_file_name = NULL, header_content_type = NULL,
                header_file_size = NULL, header_updated_at = NULL
         WHERE {BY_DOMAIN}"
    ))
    .bind(&block.domain)
    .execute(&state.db)
    .await?;
    sqlx::query(&format!(
        "UPDATE media_attachments m SET file_file_name = NULL, file_content_type = NULL,
                file_file_size = NULL, file_updated_at = NULL,
                thumbnail_file_name = NULL, thumbnail_content_type = NULL,
                thumbnail_file_size = NULL, thumbnail_updated_at = NULL
         FROM accounts a WHERE m.account_id = a.id AND {BY_DOMAIN}"
    ))
    .bind(&block.domain)
    .execute(&state.db)
    .await?;
    sqlx::query("DELETE FROM custom_emojis WHERE domain = $1 OR domain LIKE '%.' || $1")
        .bind(&block.domain)
        .execute(&state.db)
        .await?;
    Ok(())
}

/// `ProcessAccountService#create_account`: an account first seen from a
/// blocked domain starts out suspended or limited from the block's time.
pub async fn apply_to_new_account(state: &AppState, account_id: i64, domain: &str) -> Result<()> {
    let Some(rule) = crate::federation::moderation::lookup(state, domain).await else {
        return Ok(());
    };
    let created_at: Option<chrono::NaiveDateTime> = sqlx::query_scalar!(
        r#"SELECT created_at FROM domain_blocks
           WHERE domain <> '' AND ($1 = domain OR $1 LIKE '%.' || domain)
           ORDER BY char_length(domain) DESC LIMIT 1"#,
        domain.to_lowercase(),
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(created_at) = created_at else {
        return Ok(());
    };
    if rule.severity == domain_severity::SUSPEND {
        sqlx::query!(
            "UPDATE accounts SET suspended_at = $2, suspension_origin = 0 WHERE id = $1",
            account_id,
            created_at
        )
        .execute(&state.db)
        .await?;
    } else if rule.severity == domain_severity::SILENCE {
        sqlx::query!(
            "UPDATE accounts SET silenced_at = $2 WHERE id = $1",
            account_id,
            created_at
        )
        .execute(&state.db)
        .await?;
    }
    Ok(())
}
