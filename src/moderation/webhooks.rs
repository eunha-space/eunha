//! Admin webhooks: `TriggerWebhookWorker`, `WebhookService` and
//! `Webhooks::DeliveryWorker`. An enabled `webhooks` row subscribed to an
//! event gets a signed POST of the event and the object it is about.

use serde_json::{json, Value};

use crate::state::AppState;

/// What an event is about, as `REST::Admin::WebhookEventSerializer` picks
/// the object's serializer.
#[derive(Debug, Clone, Copy)]
pub enum Object {
    /// `REST::Admin::AccountSerializer`.
    Account(i64),
    /// `REST::Admin::ReportSerializer`.
    Report(i64),
    /// `REST::StatusSerializer`.
    Status(i64),
}

/// `TriggerWebhookWorker.perform_async(event, class_name, id)`: deliver in the
/// background, to whichever enabled webhooks subscribe to `event`.
pub fn trigger(state: &AppState, event: &'static str, object: Object) {
    let state = state.clone();
    crate::tenants::spawn(async move {
        if let Err(error) = call(&state, event, object).await {
            tracing::warn!(event, %error, "could not trigger webhooks");
        }
    });
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
        let payload = match hook.template.as_deref().filter(|t| !t.is_empty()) {
            Some(template) => render(&body, template),
            None => body_text.clone(),
        };
        let state = state.clone();
        let url = hook.url.clone();
        let secret = hook.secret.clone();
        let id = hook.id;
        crate::tenants::spawn(async move {
            deliver(&state, id, &url, &secret, payload).await;
        });
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
        Object::Status(id) => {
            let Some(status) = sqlx::query_as!(
                crate::db::models::Status,
                "SELECT * FROM statuses WHERE id = $1",
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
async fn deliver(state: &AppState, id: i64, url: &str, secret: &str, body: String) {
    use hmac::{Hmac, Mac};
    let signature = {
        let Ok(mut mac) = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()) else {
            return;
        };
        mac.update(body.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    };
    for attempt in 0..=16u32 {
        let result = state
            .http
            .post(url)
            .header("Content-Type", "application/json")
            .header("X-Hub-Signature", format!("sha256={signature}"))
            .body(body.clone())
            .send()
            .await;
        match result {
            Ok(response)
                if response.status().is_success()
                    || crate::federation::delivery::unsalvageable(response.status().as_u16()) =>
            {
                return;
            }
            Ok(response) => {
                tracing::debug!(webhook = id, status = %response.status(), "webhook delivery failed");
            }
            Err(error) => tracing::debug!(webhook = id, %error, "webhook delivery failed"),
        }
        if attempt == 16 {
            break;
        }
        // Sidekiq's default backoff: count⁴ + 15 + rand(10)·(count + 1) seconds.
        let jitter = u64::from(rand::random::<u8>() % 10) * u64::from(attempt + 1);
        let wait = u64::from(attempt).pow(4) + 15 + jitter;
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
    }
    tracing::warn!(webhook = id, url, "webhook delivery gave up");
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
