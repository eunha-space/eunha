use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

#[derive(Clone, Debug)]
pub enum Event {
    NewStatus {
        author_id: i64,
        is_public: bool,
        is_direct: bool,
        status_id: i64,
        hashtags: Vec<String>,
        has_media: bool,
        payload: Arc<String>,
    },
    /// Fired when a status is edited. Delivered as a `status.update` wire event.
    StatusUpdate {
        author_id: i64,
        is_public: bool,
        status_id: i64,
        hashtags: Vec<String>,
        has_media: bool,
        payload: Arc<String>,
    },
    Notification {
        for_account_id: i64,
        payload: Arc<String>,
    },
    DeleteStatus {
        status_id: i64,
    },
    /// Sent to a user's streaming connection when their custom filters change.
    FiltersChanged {
        for_account_id: i64,
    },
    /// Terminate every streaming connection belonging to an account, as
    /// Mastodon's `Account#suspend!` does by publishing `{event: :kill}` on
    /// `timeline:system:{id}`.
    Kill {
        account_id: i64,
    },
    /// Terminate the streaming connections made with these access tokens, as
    /// Mastodon publishes `{event: :kill}` on `timeline:access_token:{id}`
    /// when a token is revoked.
    KillTokens {
        token_ids: Vec<i64>,
    },
    /// An announcement published, sent to every signed-in user's stream as
    /// `PublishScheduledAnnouncementWorker` sends it to each active account's
    /// `timeline:{id}`; the payload is the announcement rendered for nobody.
    Announcement {
        payload: Arc<String>,
    },
    /// An announcement unpublished or deleted (`UnpublishAnnouncementWorker`).
    AnnouncementDelete {
        announcement_id: i64,
    },
    /// A reaction's new count (`PublishAnnouncementReactionWorker`).
    AnnouncementReaction {
        payload: Arc<String>,
    },
}

#[derive(Clone)]
pub struct StreamBus {
    tx: broadcast::Sender<Arc<Event>>,
    /// How many connections each account has subscribed to its own stream
    /// (`user` or `user:notification`): Mastodon's
    /// `subscribed:timeline:<id>` keys, which `NotifyService` reads to tell
    /// whether the recipient is online.
    online: Arc<Mutex<HashMap<i64, usize>>>,
}

/// One connection counted in [`StreamBus::is_online`] until it is dropped.
pub struct OnlineGuard {
    online: Arc<Mutex<HashMap<i64, usize>>>,
    account_id: i64,
}

impl Drop for OnlineGuard {
    fn drop(&mut self) {
        let mut online = self.online.lock().expect("online lock");
        if let Some(count) = online.get_mut(&self.account_id) {
            *count -= 1;
            if *count == 0 {
                online.remove(&self.account_id);
            }
        }
    }
}

impl Default for StreamBus {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            tx,
            online: Arc::default(),
        }
    }

    pub fn publish(&self, event: Event) {
        let _ = self.tx.send(Arc::new(event));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.tx.subscribe()
    }

    /// Count a connection subscribed to `account_id`'s own stream until the
    /// guard is dropped.
    pub fn online(&self, account_id: i64) -> OnlineGuard {
        *self
            .online
            .lock()
            .expect("online lock")
            .entry(account_id)
            .or_default() += 1;
        OnlineGuard {
            online: self.online.clone(),
            account_id,
        }
    }

    /// `NotifyService#subscribed_to_streaming_api?`: a connection of this
    /// process is subscribed to the account's `user` or `user:notification`
    /// stream.
    pub fn is_online(&self, account_id: i64) -> bool {
        self.online
            .lock()
            .expect("online lock")
            .contains_key(&account_id)
    }
}
