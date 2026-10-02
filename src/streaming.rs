//! The streaming bus: what Mastodon does with Redis pub/sub between the Rails
//! app, which publishes `{event, payload}` messages on `timeline:*` channels,
//! and the streaming server, which subscribes to the channels its connections
//! ask for. Eunha serves both sides from one process, so the channels live
//! here, one bus per instance, with the same names and the same messages.
//!
//! A channel exists while something is subscribed to it, which is what the
//! `subscribed:<channel>` keys the streaming server keeps in Redis tell the
//! Rails side: [`StreamBus::is_subscribed`]. A message published on a channel
//! nobody listens to is dropped, as Redis drops it.
//!
//! What publishes status events lives in [`fan_out`]; the streaming server
//! itself in `api::mastodon::streaming`.

pub mod fan_out;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::broadcast;

/// How many messages a subscriber may fall behind on one channel before it
/// misses some. Redis buffers per client instead; a connection this far
/// behind is not keeping up either way.
const CHANNEL_CAPACITY: usize = 256;

type Channels = Arc<Mutex<HashMap<String, broadcast::Sender<Arc<Value>>>>>;

#[derive(Clone, Default)]
pub struct StreamBus {
    channels: Channels,
}

/// One subscriber of one channel; the channel goes away with its last one,
/// as the streaming server unsubscribes from Redis when its last listener
/// leaves.
pub struct Subscription {
    rx: broadcast::Receiver<Arc<Value>>,
    channel: String,
    channels: Channels,
}

impl Subscription {
    /// The next message, or `None` once the bus is gone. Messages missed by
    /// falling behind are skipped.
    pub async fn recv(&mut self) -> Option<Arc<Value>> {
        loop {
            match self.rx.recv().await {
                Ok(message) => return Some(message),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(channel = %self.channel, missed, "streaming subscriber fell behind");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut channels = self.channels.lock().expect("streaming channels lock");
        // This subscription's receiver is still alive here.
        if channels
            .get(&self.channel)
            .is_some_and(|tx| tx.receiver_count() <= 1)
        {
            channels.remove(&self.channel);
        }
    }
}

impl StreamBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// `redis.publish(channel, message)`.
    pub fn publish(&self, channel: &str, message: Value) {
        let tx = self
            .channels
            .lock()
            .expect("streaming channels lock")
            .get(channel)
            .cloned();
        if let Some(tx) = tx {
            let _ = tx.send(Arc::new(message));
        }
    }

    /// `redisSubscribeClient.subscribe(channel)`.
    pub fn subscribe(&self, channel: &str) -> Subscription {
        let rx = self
            .channels
            .lock()
            .expect("streaming channels lock")
            .entry(channel.to_owned())
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe();
        Subscription {
            rx,
            channel: channel.to_owned(),
            channels: self.channels.clone(),
        }
    }

    /// `redis.exists?("subscribed:#{channel}")`.
    pub fn is_subscribed(&self, channel: &str) -> bool {
        self.channels
            .lock()
            .expect("streaming channels lock")
            .get(channel)
            .is_some_and(|tx| tx.receiver_count() > 0)
    }

    /// `NotifyService#subscribed_to_streaming_api?`: a connection is
    /// subscribed to the account's `user` or `user:notification` stream.
    pub fn is_online(&self, account_id: i64) -> bool {
        self.is_subscribed(&format!("timeline:{account_id}"))
            || self.is_subscribed(&format!("timeline:{account_id}:notifications"))
    }

    /// The accounts whose `timeline:<id>` channel, their home timeline, has a
    /// subscriber: everyone a home `update` could reach right now.
    pub fn home_subscribers(&self) -> Vec<i64> {
        self.numbered("timeline:", "")
    }

    /// The lists whose `timeline:list:<id>` channel has a subscriber.
    pub fn list_subscribers(&self) -> Vec<i64> {
        self.numbered("timeline:list:", "")
    }

    fn numbered(&self, prefix: &str, suffix: &str) -> Vec<i64> {
        self.channels
            .lock()
            .expect("streaming channels lock")
            .iter()
            .filter(|(_, tx)| tx.receiver_count() > 0)
            .filter_map(|(name, _)| {
                name.strip_prefix(prefix)?
                    .strip_suffix(suffix)?
                    .parse::<i64>()
                    .ok()
            })
            .collect()
    }

    // ── What the Rails side publishes besides statuses ─────────────────────

    /// `Account#suspend!`, `User#disable!` and the account deletion:
    /// `{event: :kill}` on `timeline:system:<id>` closes every connection of
    /// the account.
    pub fn kill_account(&self, account_id: i64) {
        self.publish(
            &format!("timeline:system:{account_id}"),
            json!({"event": "kill"}),
        );
    }

    /// A revoked access token: `{event: :kill}` on
    /// `timeline:access_token:<id>` closes the connections made with it.
    pub fn kill_tokens(&self, token_ids: &[i64]) {
        for id in token_ids {
            self.publish(
                &format!("timeline:access_token:{id}"),
                json!({"event": "kill"}),
            );
        }
    }

    /// `CustomFilter#invalidate_cache!`. The message has no payload, so the
    /// streaming server never passes it to a client; on the system channel it
    /// drops the connection's cached filters.
    pub fn filters_changed(&self, account_id: i64) {
        let message = json!({"event": "filters_changed"});
        self.publish(&format!("timeline:{account_id}"), message.clone());
        self.publish(&format!("timeline:system:{account_id}"), message);
    }

    /// `NotifyService#push_to_streaming_api!`: the notification as rendered
    /// for its recipient.
    pub fn notification(&self, recipient_id: i64, payload: Value) {
        self.publish(
            &format!("timeline:{recipient_id}:notifications"),
            json!({"event": "notification", "payload": payload}),
        );
    }

    /// `UnfilterNotificationsWorker#push_streaming_event!`, once the last of
    /// the account's unfiltering jobs is done.
    pub fn notifications_merged(&self, recipient_id: i64) {
        if self.is_online(recipient_id) {
            self.publish(
                &format!("timeline:{recipient_id}:notifications"),
                json!({"event": "notifications_merged", "payload": "1"}),
            );
        }
    }

    /// `{event: :delete, payload: id.to_s}` on one channel.
    pub fn delete(&self, channel: &str, status_id: i64) {
        self.publish(
            channel,
            json!({"event": "delete", "payload": status_id.to_string()}),
        );
    }
}
