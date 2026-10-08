//! A status's conversation: Mastodon's `Status#set_conversation` and
//! `#update_conversation`, and `ActivityPub::Activity::Create#
//! conversation_from_uri`.
//!
//! Every status belongs to a conversation. A reply joins its parent's, unless
//! it arrived naming one of its own; anything else, a boost included, starts
//! one. A status that is not a reply becomes the root of the conversation it
//! is in, if that has none yet (`parent_status_id` and `parent_account_id`),
//! which is what names a local conversation's context URL.
//!
//! A remote status names its conversation by `conversation`: a URI of ours
//! names one of ours, any other is found or recorded by its `uri`.

use sqlx::PgPool;

use crate::state::AppState;

/// The status a reply is to (`Status#thread`), as `set_conversation` reads it.
#[derive(Clone, Copy, Debug)]
pub struct Thread {
    pub id: i64,
    pub account_id: i64,
    pub conversation_id: Option<i64>,
    pub in_reply_to_account_id: Option<i64>,
    pub reply: bool,
}

impl Thread {
    /// `carried_over_reply_to_account_id`: a reply to an author's own reply
    /// is to whoever that one answered, so a thread its author continues
    /// stays a reply to the account it began answering.
    #[must_use]
    pub fn reply_to_account_id(&self, account_id: i64) -> Option<i64> {
        if self.account_id == account_id && self.reply {
            self.in_reply_to_account_id
        } else {
            Some(self.account_id)
        }
    }
}

/// The thread of a reply to `in_reply_to_id`, a status not discarded, or the
/// status it boosts when it is a boost (`self.thread = thread.reblog if
/// thread&.reblog?`).
pub async fn thread(db: &PgPool, in_reply_to_id: i64) -> sqlx::Result<Option<Thread>> {
    let row = sqlx::query!(
        r#"SELECT COALESCE(r.id, s.id) AS "id!", COALESCE(r.account_id, s.account_id) AS "account_id!",
                  CASE WHEN r.id IS NULL THEN s.conversation_id ELSE r.conversation_id END AS conversation_id,
                  CASE WHEN r.id IS NULL THEN s.in_reply_to_account_id ELSE r.in_reply_to_account_id END
                    AS in_reply_to_account_id,
                  COALESCE(CASE WHEN r.id IS NULL THEN s.reply ELSE r.reply END, false) AS "reply!"
           FROM statuses s
           LEFT JOIN statuses r ON r.id = s.reblog_of_id AND r.deleted_at IS NULL
           WHERE s.id = $1 AND s.deleted_at IS NULL
             AND (s.reblog_of_id IS NULL OR r.id IS NOT NULL)"#,
        in_reply_to_id,
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| Thread {
        id: r.id,
        account_id: r.account_id,
        conversation_id: r.conversation_id,
        in_reply_to_account_id: r.in_reply_to_account_id,
        reply: r.reply,
    }))
}

/// `set_conversation` and `update_conversation` for a status just inserted:
/// the conversation it named, else its thread's, else, with no thread, a new
/// one; then, for a status that is not a reply, itself as the root of a
/// conversation that has none. Returns the conversation, which a reply to a
/// status without one does not have.
pub async fn assign(db: &PgPool, status_id: i64) -> sqlx::Result<Option<i64>> {
    let Some(status) = sqlx::query!(
        r#"SELECT account_id, conversation_id, in_reply_to_id, COALESCE(reply, false) AS "reply!"
           FROM statuses WHERE id = $1"#,
        status_id,
    )
    .fetch_optional(db)
    .await?
    else {
        return Ok(None);
    };
    let thread = match status.in_reply_to_id {
        Some(parent) => thread(db, parent).await?,
        None => None,
    };
    // `self.reply = !(in_reply_to_id.nil? && thread.nil?) unless reply`.
    let reply = status.reply || status.in_reply_to_id.is_some();

    let conversation_id = match (status.conversation_id, thread) {
        (Some(id), _) => Some(id),
        // `self.conversation_id = thread.conversation_id`, for a reply in a
        // thread we hold: none, for a thread written without one.
        (None, Some(thread)) => thread.conversation_id,
        // `build_conversation`.
        (None, None) => Some(
            sqlx::query_scalar!(
                "INSERT INTO conversations (created_at, updated_at) VALUES (now(), now()) RETURNING id"
            )
            .fetch_one(db)
            .await?,
        ),
    };
    let Some(conversation_id) = conversation_id else {
        return Ok(None);
    };
    sqlx::query!(
        "UPDATE statuses SET conversation_id = $2 WHERE id = $1 AND conversation_id IS DISTINCT FROM $2",
        status_id,
        conversation_id,
    )
    .execute(db)
    .await?;

    // `update_conversation`: `return if reply?`, then the root, once.
    if !reply {
        sqlx::query!(
            r#"UPDATE conversations
               SET parent_status_id = $2, parent_account_id = $3, updated_at = now()
               WHERE id = $1 AND parent_status_id IS NULL"#,
            conversation_id,
            status_id,
            status.account_id,
        )
        .execute(db)
        .await?;
    }
    Ok(Some(conversation_id))
}

/// `conversation_from_uri`: the conversation a remote status names. A tag
/// URI of ours (`tag:{domain},…:objectId={id}:objectType=Conversation`) and
/// a context URL of ours name one of ours, which is never created from here;
/// any other URI is found by its `uri`, or recorded.
pub async fn from_uri(state: &AppState, uri: &str) -> sqlx::Result<Option<i64>> {
    // `OStatus::TagManager#local_id?`.
    if uri.starts_with(&format!("tag:{}", state.instance.domain)) {
        let id = uri
            .split_once("objectId=")
            .and_then(|(_, rest)| rest.strip_suffix(":objectType=Conversation"))
            .and_then(|id| id.parse::<i64>().ok());
        let Some(id) = id else {
            return Ok(None);
        };
        return sqlx::query_scalar!("SELECT id FROM conversations WHERE id = $1", id)
            .fetch_optional(&state.db)
            .await;
    }
    // `ActivityPub::TagManager#uri_to_resource(uri, Conversation)`, which
    // recognises `/contexts/{account_id}-{id}`. Upstream looks the second
    // half up as the conversation's id, while the URL it serves carries the
    // root status's id there (`Conversation#to_param`); this does as
    // upstream does, so such a URL rarely finds one, and the reply then
    // joins its thread's conversation.
    if crate::federation::local_uri::is_local(state, uri) {
        let Some(param) = url::Url::parse(uri).ok().and_then(|url| {
            let segments: Vec<String> = url.path_segments()?.map(str::to_owned).collect();
            match segments.as_slice() {
                [contexts, param] if contexts == "contexts" => Some(param.clone()),
                _ => None,
            }
        }) else {
            return Ok(None);
        };
        let Some((account_id, id)) = param.split_once('-') else {
            return Ok(None);
        };
        let (Ok(account_id), Ok(id)) = (account_id.parse::<i64>(), id.parse::<i64>()) else {
            return Ok(None);
        };
        return sqlx::query_scalar!(
            "SELECT id FROM conversations WHERE parent_account_id = $1 AND id = $2",
            account_id,
            id,
        )
        .fetch_optional(&state.db)
        .await;
    }
    // `Conversation.find_or_create_by!(uri:)`, retried on a race.
    if let Some(id) = sqlx::query_scalar!(
        r#"INSERT INTO conversations (uri, created_at, updated_at) VALUES ($1, now(), now())
           ON CONFLICT (uri) WHERE uri IS NOT NULL DO NOTHING
           RETURNING id"#,
        uri,
    )
    .fetch_optional(&state.db)
    .await?
    {
        return Ok(Some(id));
    }
    sqlx::query_scalar!("SELECT id FROM conversations WHERE uri = $1", uri)
        .fetch_optional(&state.db)
        .await
}
