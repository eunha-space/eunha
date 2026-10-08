//! Admin webhooks: `TriggerWebhookWorker`, `WebhookService` and
//! `Webhooks::DeliveryWorker`. An enabled `webhooks` row subscribed to an
//! event gets a signed POST of the event and the object it is about.

use serde_json::{json, Value};

use crate::state::AppState;

/// What an event is about, as `REST::Admin::WebhookEventSerializer` picks
/// the object's serializer.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum Object {
    /// `REST::Admin::AccountSerializer`.
    Account(i64),
    /// `REST::Admin::ReportSerializer`.
    Report(i64),
    /// `REST::StatusSerializer`.
    Status(i64),
}

/// `TriggerWebhookWorker.perform_async(event, class_name, id)`: deliver, from
/// the job queue, to whichever enabled webhooks subscribe to `event`.
pub async fn trigger(state: &AppState, event: &'static str, object: Object) {
    crate::jobs::push(
        state,
        TriggerWebhookWorker {
            event: event.to_owned(),
            object,
        },
    )
    .await;
}

/// `Status`'s `after_update_commit :trigger_update_webhooks`: `status.updated`
/// for a local status (`local?`, which a status without a `uri` is too). Call
/// it wherever Mastodon saves a change to a status through its model, not
/// where it writes the column directly (`update_column`, `update_all`).
pub async fn status_updated(state: &AppState, status_id: i64) {
    let local = sqlx::query_scalar!(
        r#"SELECT (local IS TRUE OR uri IS NULL) AS "local!" FROM statuses WHERE id = $1"#,
        status_id
    )
    .fetch_optional(&state.db)
    .await;
    match local {
        Ok(Some(true)) => trigger(state, "status.updated", Object::Status(status_id)).await,
        Ok(_) => {}
        Err(error) => tracing::warn!(status_id, %error, "could not read a status for webhooks"),
    }
}

/// `Account`'s `after_update_commit :trigger_update_webhooks`:
/// `account.updated` for a local account. As with [`status_updated`], only
/// where Mastodon saves the account through its model.
pub async fn account_updated(state: &AppState, account_id: i64) {
    let local = sqlx::query_scalar!(
        r#"SELECT (domain IS NULL) AS "local!" FROM accounts WHERE id = $1"#,
        account_id
    )
    .fetch_optional(&state.db)
    .await;
    match local {
        Ok(Some(true)) => trigger(state, "account.updated", Object::Account(account_id)).await,
        Ok(_) => {}
        Err(error) => tracing::warn!(account_id, %error, "could not read an account for webhooks"),
    }
}

/// `TriggerWebhookWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct TriggerWebhookWorker {
    pub event: String,
    pub object: Object,
}

impl crate::jobs::Job for TriggerWebhookWorker {
    const KIND: &'static str = "TriggerWebhookWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT;

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        call(state, &self.event, self.object).await
    }
}

/// `Webhooks::DeliveryWorker`: one signed POST, the template applied.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct DeliveryWorker {
    pub webhook_id: i64,
    pub body: String,
}

impl crate::jobs::Job for DeliveryWorker {
    const KIND: &'static str = "Webhooks::DeliveryWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Push)
        .retry(16)
        .dead(false);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        // `Webhook.find(webhook_id)`, else nothing to do.
        let Some(hook) = sqlx::query!(
            "SELECT url, secret, template FROM webhooks WHERE id = $1",
            self.webhook_id
        )
        .fetch_optional(&state.db)
        .await?
        else {
            return Ok(());
        };
        let body = match hook.template.as_deref().filter(|t| !t.is_empty()) {
            Some(template) => match serde_json::from_str::<Value>(&self.body) {
                Ok(parsed) => render(&parsed, template),
                Err(_) => self.body,
            },
            None => self.body,
        };
        deliver(state, &hook.url, &hook.secret, body).await
    }
}

/// `WebhookService#call`.
async fn call(state: &AppState, event: &str, object: Object) -> anyhow::Result<()> {
    let hooks = sqlx::query!(
        "SELECT id, url, secret, template FROM webhooks WHERE enabled AND $1 = ANY(events)",
        event,
    )
    .fetch_all(&state.db)
    .await?;
    if hooks.is_empty() {
        return Ok(());
    }
    let Some(serialized) = serialize(state, object).await? else {
        return Ok(());
    };
    let body = json!({
        "event": event,
        "created_at": crate::api::mastodon::convert::mastodon_date(chrono::Utc::now().naive_utc()),
        "object": serialized,
    });
    let body_text = body.to_string();
    for hook in hooks {
        crate::jobs::perform_async(
            state,
            DeliveryWorker {
                webhook_id: hook.id,
                body: body_text.clone(),
            },
        )
        .await?;
    }
    Ok(())
}

async fn serialize(state: &AppState, object: Object) -> anyhow::Result<Option<Value>> {
    use crate::api::mastodon::admin;
    Ok(match object {
        Object::Account(id) => {
            let Some(account) = sqlx::query_as!(
                crate::db::models::Account,
                "SELECT * FROM accounts WHERE id = $1",
                id
            )
            .fetch_optional(&state.db)
            .await?
            else {
                return Ok(None);
            };
            let entity = admin::build_admin_account(state, &account)
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            Some(serde_json::to_value(entity)?)
        }
        Object::Report(id) => admin::admin_report_entity(state, id)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
            .map(serde_json::to_value)
            .transpose()?,
        // `Status.find(id)`, under the default scope: a discarded status is
        // not found, and its event goes nowhere.
        Object::Status(id) => {
            let Some(status) = sqlx::query_as!(
                crate::db::models::Status,
                "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
                id
            )
            .fetch_optional(&state.db)
            .await?
            else {
                return Ok(None);
            };
            let entity = crate::api::mastodon::statuses::serialize_status(state, &status, None)
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            Some(serde_json::to_value(entity)?)
        }
    })
}

/// `Webhooks::PayloadRenderer#render`: each `{{path.to.value}}` replaced by
/// the JSON of what it names, a string without its quotes.
pub fn render(document: &Value, template: &str) -> String {
    static EXPRESSION: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?i)\{\{[a-z_]+(\.([a-z_]+|[0-9]+))*\}\}").expect("valid regex")
    });
    EXPRESSION
        .replace_all(template, |caps: &regex::Captures| {
            let path = &caps[0][2..caps[0].len() - 2];
            let mut value = document;
            for segment in path.split('.').filter(|s| !s.is_empty()) {
                value = match segment.parse::<usize>() {
                    Ok(i) => value.get(i).unwrap_or(&Value::Null),
                    Err(_) => value.get(segment).unwrap_or(&Value::Null),
                };
            }
            match value {
                Value::String(s) => {
                    let quoted = Value::String(s.clone()).to_string();
                    quoted[1..quoted.len() - 1].to_owned()
                }
                other => other.to_string(),
            }
        })
        .into_owned()
}

/// `Webhooks::DeliveryWorker`: POST with `X-Hub-Signature`, retried on
/// Sidekiq's schedule (16 retries) unless the answer says retrying is
/// pointless.
async fn deliver(state: &AppState, url: &str, secret: &str, body: String) -> anyhow::Result<()> {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|e| crate::jobs::Discard(e.to_string()))?;
    mac.update(body.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());
    let response = state
        .http
        .post(url)
        .header("Content-Type", "application/json")
        .header("X-Hub-Signature", format!("sha256={signature}"))
        .body(body)
        .send()
        .await?;
    let status = response.status();
    anyhow::ensure!(
        status.is_success() || crate::federation::delivery::unsalvageable(status.as_u16()),
        "webhook answered {status}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn renders_templates_like_mastodon() {
        let doc = serde_json::json!({"event": "report.created", "object": {"id": "7", "statuses": [{"id": "9"}], "n": 3}});
        assert_eq!(
            super::render(
                &doc,
                r#"{"text": "Report {{object.id}} ({{event}}), {{object.n}} {{object.statuses.0.id}}"}"#
            ),
            r#"{"text": "Report 7 (report.created), 3 9"}"#
        );
    }
}
