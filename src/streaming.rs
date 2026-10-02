//! The streaming bus: Redis pub/sub between what publishes `{event,
//! payload}` messages on `timeline:*` channels, as Mastodon's Rails side
//! does, and the streaming server, which subscribes to the channels its
//! connections ask for, as Mastodon's Node server does. Channels and the
//! `subscribed:<channel>` keys go under the instance's Redis key prefix, as
//! Mastodon's go under `REDIS_NAMESPACE`, so instances sharing a Redis never
//! hear each other, and any process serving or acting for the instance —
//! `eunha accounts`, another server — reaches the same streams.
//!
//! Each process keeps one subscribing connection per instance, opened when
//! its first stream is, and subscribes to a channel while it has a listener
//! for it. A message published on a channel nobody listens to is dropped, as
//! Redis drops it.
//!
//! Whether a channel has a listener anywhere is what the
//! `subscribed:<channel>` keys tell: set for eighteen minutes when a stream
//! subscribes, and again every six minutes while it lasts, as the streaming
//! server's `subscriptionHeartbeat` sets them ([`StreamBus::is_subscribed`]).
//!
//! What publishes status events lives in [`fan_out`]; the streaming server
//! itself in `api::mastodon::streaming`.

pub mod fan_out;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt as _;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::redis_keys::RedisKeyspace;

/// How many messages a listener may fall behind on one channel before it
/// misses some. A connection this far behind is not keeping up.
const CHANNEL_CAPACITY: usize = 256;

/// `subscriptionHeartbeat`'s interval: how often a stream's
/// `subscribed:<channel>` keys are set again.
const HEARTBEAT: Duration = Duration::from_secs(6 * 60);

/// How long a `subscribed:<channel>` key lives: three heartbeats.
const SUBSCRIBED_TTL: u64 = 3 * 6 * 60;

/// The most keys one `MGET` asks about.
const MGET_CHUNK: usize = 1000;

type Channels = Arc<Mutex<HashMap<String, broadcast::Sender<Arc<Value>>>>>;

enum Command {
    Subscribe(String, oneshot::Sender<()>),
    Unsubscribe(String),
}

#[derive(Clone)]
pub struct StreamBus {
    inner: Arc<Inner>,
}

struct Inner {
    client: redis::Client,
    redis: redis::aio::ConnectionManager,
    keys: RedisKeyspace,
    channels: Channels,
    /// The subscribing connection's task, once a stream has opened it.
    commands: Mutex<Option<mpsc::UnboundedSender<Command>>>,
    stop: tokio_util::sync::CancellationToken,
}

/// One listener of one channel; the process unsubscribes from the channel
/// with its last, as the streaming server does.
pub struct Subscription {
    rx: broadcast::Receiver<Arc<Value>>,
    channel: String,
    bus: StreamBus,
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
        let mut channels = self.bus.inner.channels.lock().expect("streaming channels");
        // This subscription's receiver is still alive here.
        if channels
            .get(&self.channel)
            .is_some_and(|tx| tx.receiver_count() <= 1)
        {
            channels.remove(&self.channel);
            drop(channels);
            self.bus
                .command(Command::Unsubscribe(std::mem::take(&mut self.channel)));
        }
    }
}

impl StreamBus {
    pub fn new(
        client: redis::Client,
        redis: redis::aio::ConnectionManager,
        keys: RedisKeyspace,
        stop: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                client,
                redis,
                keys,
                channels: Arc::default(),
                commands: Mutex::new(None),
                stop,
            }),
        }
    }

    fn command(&self, command: Command) {
        if let Some(tx) = self
            .inner
            .commands
            .lock()
            .expect("streaming commands")
            .as_ref()
        {
            let _ = tx.send(command);
        }
    }

    /// The subscribing connection's task, started on first use.
    fn commands(&self) -> mpsc::UnboundedSender<Command> {
        let mut commands = self.inner.commands.lock().expect("streaming commands");
        if let Some(tx) = commands.as_ref().filter(|tx| !tx.is_closed()) {
            return tx.clone();
        }
        let (tx, rx) = mpsc::unbounded_channel();
        *commands = Some(tx.clone());
        let inner = Arc::downgrade(&self.inner);
        crate::tenants::spawn(run_subscriber(inner, rx));
        tx
    }

    /// `redis.publish(channel, message)`.
    pub async fn publish(&self, channel: &str, message: Value) {
        let mut redis = self.inner.redis.clone();
        let published: redis::RedisResult<i64> = redis::cmd("PUBLISH")
            .arg(self.inner.keys.key(channel))
            .arg(message.to_string())
            .query_async(&mut redis)
            .await;
        if let Err(error) = published {
            tracing::warn!(channel, %error, "could not publish a streaming message");
        }
    }

    /// `redisSubscribeClient.subscribe(channel)`, and the first
    /// `subscribed:<channel>` of its heartbeat. Returns once Redis has
    /// confirmed the subscription, so that nothing published after it is
    /// missed.
    pub async fn subscribe(&self, channel: &str) -> Subscription {
        let (rx, new) = {
            let mut channels = self.inner.channels.lock().expect("streaming channels");
            let new = !channels.contains_key(channel);
            let rx = channels
                .entry(channel.to_owned())
                .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
                .subscribe();
            (rx, new)
        };
        if new {
            let (ack, acked) = oneshot::channel();
            let _ = self
                .commands()
                .send(Command::Subscribe(channel.to_owned(), ack));
            if tokio::time::timeout(Duration::from_secs(5), acked)
                .await
                .is_err()
            {
                tracing::warn!(channel, "Redis did not confirm a streaming subscription");
            }
        }
        self.tell_subscribed(&[channel.to_owned()]).await;
        Subscription {
            rx,
            channel: channel.to_owned(),
            bus: self.clone(),
        }
    }

    /// `tellSubscribed`: `SET subscribed:<channel> 1 EX 1080` for each.
    async fn tell_subscribed(&self, channels: &[String]) {
        tell_subscribed(&self.inner, channels).await;
    }

    /// `redis.exists?("subscribed:#{channel}")`.
    pub async fn is_subscribed(&self, channel: &str) -> bool {
        self.subscribed(&[channel.to_owned()])
            .await
            .first()
            .copied()
            .unwrap_or(false)
    }

    /// [`StreamBus::is_subscribed`] for each of `channels`, in order.
    pub async fn subscribed(&self, channels: &[String]) -> Vec<bool> {
        let mut answers = Vec::with_capacity(channels.len());
        let mut redis = self.inner.redis.clone();
        for chunk in channels.chunks(MGET_CHUNK) {
            let mut cmd = redis::cmd("MGET");
            for channel in chunk {
                cmd.arg(self.inner.keys.key(format!("subscribed:{channel}")));
            }
            let values: Vec<Option<String>> =
                cmd.query_async(&mut redis).await.unwrap_or_else(|error| {
                    tracing::warn!(%error, "could not ask Redis who is streaming");
                    vec![None; chunk.len()]
                });
            answers.extend(values.into_iter().map(|v| v.is_some()));
        }
        answers
    }

    /// Of `ids`, those whose channel `format!("{prefix}{id}")` has a
    /// listener somewhere.
    pub async fn subscribed_ids(&self, prefix: &str, ids: Vec<i64>) -> Vec<i64> {
        let channels: Vec<String> = ids.iter().map(|id| format!("{prefix}{id}")).collect();
        let answers = self.subscribed(&channels).await;
        ids.into_iter()
            .zip(answers)
            .filter_map(|(id, yes)| yes.then_some(id))
            .collect()
    }

    /// `NotifyService#subscribed_to_streaming_api?`: a stream is subscribed
    /// to the account's `user` or `user:notification` channel.
    pub async fn is_online(&self, account_id: i64) -> bool {
        self.subscribed(&[
            format!("timeline:{account_id}"),
            format!("timeline:{account_id}:notifications"),
        ])
        .await
        .into_iter()
        .any(|yes| yes)
    }

    // ── What the Rails side publishes besides statuses ─────────────────────

    /// `Account#suspend!`, `User#disable!` and the account deletion:
    /// `{event: :kill}` on `timeline:system:<id>` closes every connection of
    /// the account.
    pub async fn kill_account(&self, account_id: i64) {
        self.publish(
            &format!("timeline:system:{account_id}"),
            json!({"event": "kill"}),
        )
        .await;
    }

    /// A revoked access token: `{event: :kill}` on
    /// `timeline:access_token:<id>` closes the connections made with it.
    pub async fn kill_tokens(&self, token_ids: &[i64]) {
        for id in token_ids {
            self.publish(
                &format!("timeline:access_token:{id}"),
                json!({"event": "kill"}),
            )
            .await;
        }
    }

    /// `CustomFilter#invalidate_cache!`. The message has no payload, so the
    /// streaming server never passes it to a client; on the system channel it
    /// drops the connection's cached filters.
    pub async fn filters_changed(&self, account_id: i64) {
        let message = json!({"event": "filters_changed"});
        self.publish(&format!("timeline:{account_id}"), message.clone())
            .await;
        self.publish(&format!("timeline:system:{account_id}"), message)
            .await;
    }

    /// `NotifyService#push_to_streaming_api!`: the notification as rendered
    /// for its recipient.
    pub async fn notification(&self, recipient_id: i64, payload: Value) {
        self.publish(
            &format!("timeline:{recipient_id}:notifications"),
            json!({"event": "notification", "payload": payload}),
        )
        .await;
    }

    /// `UnfilterNotificationsWorker#push_streaming_event!`, once the last of
    /// the account's unfiltering jobs is done.
    pub async fn notifications_merged(&self, recipient_id: i64) {
        if self.is_online(recipient_id).await {
            self.publish(
                &format!("timeline:{recipient_id}:notifications"),
                json!({"event": "notifications_merged", "payload": "1"}),
            )
            .await;
        }
    }

    /// `{event: :delete, payload: id.to_s}` on one channel.
    pub async fn delete(&self, channel: &str, status_id: i64) {
        self.publish(
            channel,
            json!({"event": "delete", "payload": status_id.to_string()}),
        )
        .await;
    }
}

async fn tell_subscribed(inner: &Inner, channels: &[String]) {
    if channels.is_empty() {
        return;
    }
    let mut pipe = redis::pipe();
    for channel in channels {
        pipe.cmd("SET")
            .arg(inner.keys.key(format!("subscribed:{channel}")))
            .arg("1")
            .arg("EX")
            .arg(SUBSCRIBED_TTL)
            .ignore();
    }
    let mut redis = inner.redis.clone();
    if let Err(error) = pipe.query_async::<()>(&mut redis).await {
        tracing::warn!(%error, "could not tell Redis what is streaming");
    }
}

/// The subscribing connection: subscribe and unsubscribe as streams come and
/// go, hand each message to the channel's listeners in this process, keep
/// the `subscribed:` keys alive, and reconnect, subscribing again to every
/// channel listened to, when Redis goes away. Ends with the instance, or
/// once the bus is gone.
async fn run_subscriber(
    inner: std::sync::Weak<Inner>,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    let mut waiting: Vec<oneshot::Sender<()>> = Vec::new();
    loop {
        let Some(bus) = inner.upgrade() else {
            return;
        };
        let stop = bus.stop.clone();
        if stop.is_cancelled() {
            return;
        }
        let pubsub = tokio::select! {
            () = stop.cancelled() => return,
            pubsub = bus.client.get_async_pubsub() => pubsub,
        };
        let (mut sink, mut stream) = match pubsub {
            Ok(pubsub) => pubsub.split(),
            Err(error) => {
                tracing::warn!(%error, "could not open the streaming connection to Redis");
                drop(bus);
                crate::background::rest(&stop, Duration::from_secs(1)).await;
                continue;
            }
        };
        let listened: Vec<String> = bus
            .channels
            .lock()
            .expect("streaming channels")
            .keys()
            .cloned()
            .collect();
        let mut failed = false;
        for channel in &listened {
            if let Err(error) = sink.subscribe(bus.keys.key(channel)).await {
                tracing::warn!(%error, "could not subscribe to a streaming channel");
                failed = true;
                break;
            }
        }
        if failed {
            drop(bus);
            crate::background::rest(&stop, Duration::from_secs(1)).await;
            continue;
        }
        for ack in waiting.drain(..) {
            let _ = ack.send(());
        }
        let keys = bus.keys.clone();
        let channels = bus.channels.clone();
        drop(bus);
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        heartbeat.tick().await;
        loop {
            tokio::select! {
                () = stop.cancelled() => return,
                message = stream.next() => {
                    let Some(message) = message else {
                        tracing::warn!("the streaming connection to Redis closed; reconnecting");
                        break;
                    };
                    dispatch(&keys, &channels, &message);
                }
                command = commands.recv() => {
                    let Some(command) = command else {
                        return;
                    };
                    let done = match command {
                        Command::Subscribe(channel, ack) => {
                            let done = sink.subscribe(keys.key(&channel)).await;
                            if done.is_ok() {
                                let _ = ack.send(());
                            } else {
                                waiting.push(ack);
                            }
                            done
                        }
                        Command::Unsubscribe(channel) => {
                            // Listened to again since: keep it.
                            if channels.lock().expect("streaming channels").contains_key(&channel) {
                                Ok(())
                            } else {
                                sink.unsubscribe(keys.key(&channel)).await
                            }
                        }
                    };
                    if let Err(error) = done {
                        tracing::warn!(%error, "the streaming connection to Redis failed; reconnecting");
                        break;
                    }
                }
                _ = heartbeat.tick() => {
                    let Some(bus) = inner.upgrade() else {
                        return;
                    };
                    let listened: Vec<String> =
                        channels.lock().expect("streaming channels").keys().cloned().collect();
                    tell_subscribed(&bus, &listened).await;
                }
            }
        }
        crate::background::rest(&stop, Duration::from_secs(1)).await;
    }
}

/// `onRedisMessage`: a message on a channel goes to its listeners here.
fn dispatch(keys: &RedisKeyspace, channels: &Channels, message: &redis::Msg) {
    let channel = message.get_channel_name();
    let Some(channel) = keys.strip(channel) else {
        return;
    };
    let Ok(payload) = message.get_payload::<String>() else {
        return;
    };
    let Ok(json) = serde_json::from_str::<Value>(&payload) else {
        return;
    };
    let tx = channels
        .lock()
        .expect("streaming channels")
        .get(channel)
        .cloned();
    if let Some(tx) = tx {
        let _ = tx.send(Arc::new(json));
    }
}
