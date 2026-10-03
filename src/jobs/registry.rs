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
        entry::<crate::accounts::WelcomeMailJob>(),
        entry::<crate::email_subscriptions::EmailDistributionWorker>(),
        entry::<crate::fasp::workers::AnnounceAccountLifecycleEventWorker>(),
        entry::<crate::fasp::workers::AnnounceContentLifecycleEventWorker>(),
        entry::<crate::fasp::workers::AnnounceTrendWorker>(),
        entry::<crate::fasp::workers::BackfillWorker>(),
        entry::<crate::fasp::workers::AccountSearchWorker>(),
        entry::<crate::fasp::workers::FollowRecommendationWorker>(),
        entry::<crate::federation::process_account::AccountRefreshWorker>(),
        entry::<crate::federation::process_account::RefollowWorker>(),
        entry::<crate::federation::process_account::AccountMergingWorker>(),
        entry::<crate::federation::featured::SynchronizeFeaturedCollectionWorker>(),
        entry::<crate::federation::featured::SynchronizeFeaturedTagsCollectionWorker>(),
        entry::<crate::federation::featured::SynchronizeFeaturedCollectionsCollectionWorker>(),
        entry::<crate::link_verification::VerifyAccountLinksWorker>(),
        entry::<crate::api::ap::inbox::quote::RefetchAndVerifyQuoteWorker>(),
        entry::<crate::federation::replies::FetchAllRepliesWorker>(),
        entry::<crate::federation::replies::FetchRepliesWorker>(),
        entry::<crate::moderation::webhooks::TriggerWebhookWorker>(),
        entry::<crate::moderation::webhooks::DeliveryWorker>(),
        entry::<crate::moderation::suspension::SuspensionWorker>(),
        entry::<crate::moderation::suspension::UnsuspensionWorker>(),
        entry::<crate::preview_card::LinkCrawlWorker>(),
        entry::<crate::announcements::PublishScheduledAnnouncementWorker>(),
        entry::<crate::announcements::PublishAnnouncementReactionWorker>(),
        entry::<crate::portability::import::BulkImportWorker>(),
        entry::<crate::portability::import::RowWorker>(),
        entry::<crate::home_feed::RegenerationWorker>(),
        entry::<crate::home_feed::MergeWorker>(),
        entry::<crate::home_feed::UnmergeWorker>(),
        entry::<crate::moves::MoveWorker>(),
        entry::<crate::moves::UnfollowMigratedWorker>(),
        entry::<crate::push::PushNotificationWorker>(),
        entry::<crate::api::ap::inbox::create::ThreadResolveWorker>(),
        entry::<crate::api::mastodon::annual_reports::GenerateAnnualReportWorker>(),
        entry::<crate::delete_account::AccountDeletionWorker>(),
        entry::<crate::delete_account::AdminAccountDeletionWorker>(),
        entry::<crate::moderation::domain_block::DomainBlockWorker>(),
        entry::<crate::moderation::domain_block::AfterUnallowDomainWorker>(),
        entry::<crate::api::mastodon::admin::instances::DomainPurgeWorker>(),
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
