use std::sync::Arc;
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
}

#[derive(Clone)]
pub struct StreamBus {
    tx: broadcast::Sender<Arc<Event>>,
}

impl Default for StreamBus {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self { tx }
    }

    pub fn publish(&self, event: Event) {
        let _ = self.tx.send(Arc::new(event));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.tx.subscribe()
    }
}
