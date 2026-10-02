//! Every worker a queued job may name, by its class name.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use super::{Discard, Exhausted, Job, Perform};

pub(crate) struct Entry {
    pub kind: &'static str,
    pub perform: Perform,
    pub retry_in: fn(u32) -> Option<Duration>,
    pub exhausted: Exhausted,
}

fn entry<J: Job>() -> Entry {
    Entry {
        kind: J::KIND,
        perform: |state, args| {
            Box::pin(async move {
                let job: J = serde_json::from_value(args).map_err(|e| {
                    anyhow::Error::new(Discard(format!("its arguments do not parse: {e}")))
                })?;
                job.perform(&state).await
            })
        },
        retry_in: J::retry_in,
        exhausted: |state, args, error| {
            Box::pin(async move {
                if let Ok(job) = serde_json::from_value::<J>(args) {
                    job.retries_exhausted(&state, &error).await;
                }
            })
        },
    }
}

/// The workers, in no particular order.
fn workers() -> Vec<Entry> {
    vec![
        entry::<super::Probe>(),
        entry::<super::UniqueProbe>(),
        entry::<crate::email::MailDeliveryJob>(),
        entry::<crate::notification_mail::NotificationMailJob>(),
        entry::<crate::terms_of_service::DistributeNotificationWorker>(),
        entry::<crate::api::mastodon::admin::announcements::DistributeAnnouncementNotificationWorker>(
        ),
    ]
}

static REGISTRY: LazyLock<HashMap<&'static str, Entry>> =
    LazyLock::new(|| workers().into_iter().map(|e| (e.kind, e)).collect());

pub(crate) fn get(kind: &str) -> Option<&'static Entry> {
    REGISTRY.get(kind)
}

#[cfg(test)]
pub(crate) fn check_unique() {
    let all = workers();
    assert_eq!(all.len(), REGISTRY.len(), "two workers share a name");
}
