//! The streaming API as Mastodon's streaming server serves it
//! (`streaming/index.js`): every connection authenticated, `stream` naming
//! the channel as the client asked for it, and the public streams filtered
//! for the viewer. See docs/mastodon/streaming.md.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest, Message};

use crate::helpers::{seed_token_with_scopes, set_setting, TestContext};

// ── helpers ───────────────────────────────────────────────────────────────

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn ws_request(ctx: &TestContext, query: &str) -> tungstenite::handshake::client::Request {
    let url = format!(
        "{}/api/v1/streaming?{query}",
        ctx.api.base_url.replace("http://", "ws://")
    );
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("host", ctx.domain.parse().unwrap());
    request
}

/// Open a WebSocket with `access_token` and, if given, an initial `stream`,
/// then give the server a moment to subscribe.
async fn ws_connect(ctx: &TestContext, stream: &str, token: &str) -> Ws {
    let mut query = format!("access_token={token}");
    if !stream.is_empty() {
        query.push_str(&format!("&stream={}", urlencoding::encode(stream)));
    }
    let (ws, _) = tokio_tungstenite::connect_async(ws_request(ctx, &query))
        .await
        .expect("WebSocket connection failed");
    settle().await;
    ws
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

async fn send(ws: &mut Ws, message: Value) {
    ws.send(Message::Text(message.to_string().into()))
        .await
        .unwrap();
    settle().await;
}

/// The next text frame as JSON, skipping pings; `None` after `wait`.
async fn next_frame_within(ws: &mut Ws, wait: Duration) -> Option<Value> {
    loop {
        match timeout(wait, ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => return serde_json::from_str(&text).ok(),
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            _ => return None,
        }
    }
}

async fn next_event(ws: &mut Ws) -> Option<Value> {
    next_frame_within(ws, Duration::from_secs(3)).await
}

/// Nothing arrives for a while.
async fn quiet(ws: &mut Ws) -> bool {
    next_frame_within(ws, Duration::from_millis(800))
        .await
        .is_none()
}

fn payload(event: &Value) -> Value {
    serde_json::from_str(event["payload"].as_str().expect("a string payload")).unwrap()
}

/// `User.signed_in_recently`: home and list updates go only to those.
async fn signed_in(ctx: &TestContext, account_id: &str) {
    sqlx::query("UPDATE users SET current_sign_in_at = now() WHERE account_id = $1")
        .bind(account_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
}

// ── authentication ────────────────────────────────────────────────────────

/// A WebSocket without a token is refused before the upgrade, with the
/// reason in `X-Error-Message`.
#[tokio::test]
async fn test_websocket_requires_a_token() {
    let ctx = TestContext::new("stream-ws-auth").await;
    for (query, message) in [
        ("stream=public", "Missing access token"),
        ("stream=public&access_token=nope", "Invalid access token"),
    ] {
        match tokio_tungstenite::connect_async(ws_request(&ctx, query)).await {
            Err(tungstenite::Error::Http(response)) => {
                assert_eq!(response.status().as_u16(), 401);
                assert_eq!(response.headers()["x-error-message"], message);
            }
            other => panic!("expected a refusal, got {:?}", other.map(|_| ())),
        }
    }

    // A disabled user's token is refused too.
    sqlx::query("UPDATE users SET disabled = true WHERE account_id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let query = format!("stream=public&access_token={}", ctx.alice_token);
    assert!(matches!(
        tokio_tungstenite::connect_async(ws_request(&ctx, &query)).await,
        Err(tungstenite::Error::Http(r)) if r.status().as_u16() == 401
    ));
}

/// The token may come as the `Sec-WebSocket-Protocol`, which is then echoed
/// back, or as a bearer `Authorization` header.
#[tokio::test]
async fn test_websocket_token_in_headers() {
    let ctx = TestContext::new("stream-ws-headers").await;
    let mut request = ws_request(&ctx, "stream=public:local");
    request
        .headers_mut()
        .insert("sec-websocket-protocol", ctx.bob_token.parse().unwrap());
    let (mut ws, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(
        response.headers()["sec-websocket-protocol"],
        ctx.bob_token.as_str()
    );
    settle().await;
    ctx.api
        .post_status(&ctx.alice_token, "hello", "public")
        .await;
    assert_eq!(next_event(&mut ws).await.unwrap()["event"], "update");

    let mut request = ws_request(&ctx, "stream=public:local");
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", ctx.bob_token).parse().unwrap(),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    settle().await;
    ctx.api
        .post_status(&ctx.alice_token, "again", "public")
        .await;
    assert_eq!(next_event(&mut ws).await.unwrap()["event"], "update");
}

/// A subscription the token's scopes do not cover is answered with an error
/// frame; `read:notifications` reaches only the notifications stream.
#[tokio::test]
async fn test_subscription_scopes() {
    let ctx = TestContext::new("stream-scopes").await;
    let token =
        seed_token_with_scopes(&ctx.db, ctx.alice_id.parse().unwrap(), "read:notifications").await;
    let mut ws = ws_connect(&ctx, "public", &token).await;
    assert_eq!(
        next_event(&mut ws).await.unwrap(),
        json!({"error": "Access token does not have the required scopes", "status": 401})
    );
    send(
        &mut ws,
        json!({"type": "subscribe", "stream": "user:notification"}),
    )
    .await;
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    let event = next_event(&mut ws).await.unwrap();
    assert_eq!(event["event"], "notification");
    assert_eq!(event["stream"], json!(["user:notification"]));
}

/// Revoking the token closes its connections.
#[tokio::test]
async fn test_revoked_token_is_killed() {
    let ctx = TestContext::new("stream-kill").await;
    let mut ws = ws_connect(&ctx, "public", &ctx.alice_token).await;
    let revoked = ctx
        .api
        .post_json("/oauth/revoke", None, &json!({"token": ctx.alice_token}))
        .await;
    assert_eq!(revoked.status().as_u16(), 200);
    let closed = timeout(Duration::from_secs(3), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => continue,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the connection was not closed");
}

// ── public and hashtag streams ────────────────────────────────────────────

/// A public status reaches `public`, rendered for nobody, with the viewer's
/// keyword filter results; `stream` is the channel name alone.
#[tokio::test]
async fn test_public_stream() {
    let ctx = TestContext::new("stream-public").await;
    let mut ws = ws_connect(&ctx, "public", &ctx.bob_token).await;

    ctx.api
        .post_status(&ctx.alice_token, "Hello streaming world", "public")
        .await;
    let event = next_event(&mut ws).await.expect("an update");
    assert_eq!(event["event"], "update");
    assert_eq!(event["stream"], json!(["public"]));
    let status = payload(&event);
    assert!(status["content"]
        .as_str()
        .unwrap()
        .contains("Hello streaming world"));
    assert!(status.get("favourited").is_none(), "rendered for a viewer");
    assert_eq!(status["filtered"], json!([]));

    // Unlisted and private posts stay off it.
    ctx.api
        .post_status(&ctx.alice_token, "unlisted", "unlisted")
        .await;
    ctx.api
        .post_status(&ctx.alice_token, "private", "private")
        .await;
    assert!(quiet(&mut ws).await);
}

/// `public:local` names itself; `public:remote` hears nothing local.
#[tokio::test]
async fn test_public_local_and_remote_streams() {
    let ctx = TestContext::new("stream-local").await;
    let mut local = ws_connect(&ctx, "public:local", &ctx.bob_token).await;
    let mut remote = ws_connect(&ctx, "public:remote", &ctx.bob_token).await;
    ctx.api
        .post_status(&ctx.alice_token, "Local hello", "public")
        .await;
    let event = next_event(&mut local).await.expect("an update");
    assert_eq!(event["stream"], json!(["public:local"]));
    assert!(quiet(&mut remote).await);
}

/// Editing and deleting reach the public stream as `status.update` and
/// `delete`, the latter with the id as the payload.
#[tokio::test]
async fn test_status_update_and_delete() {
    let ctx = TestContext::new("stream-edit").await;
    let mut ws = ws_connect(&ctx, "public", &ctx.bob_token).await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "Original text", "public")
        .await;
    let id = status["id"].as_str().unwrap();
    assert_eq!(next_event(&mut ws).await.unwrap()["event"], "update");

    let edited = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.alice_token),
            &json!({"status": "Edited text"}),
        )
        .await;
    assert_eq!(edited.status().as_u16(), 200);
    let event = next_event(&mut ws).await.expect("a status.update");
    assert_eq!(event["event"], "status.update");
    assert!(payload(&event)["content"]
        .as_str()
        .unwrap()
        .contains("Edited text"));

    let deleted = ctx
        .api
        .delete(&format!("/api/v1/statuses/{id}"), &ctx.alice_token)
        .await;
    assert_eq!(deleted.status().as_u16(), 200);
    let event = next_event(&mut ws).await.expect("a delete");
    assert_eq!(event["event"], "delete");
    assert_eq!(event["payload"], id);
}

/// A hashtag stream is subscribed to by name with `tag`, matched normalized,
/// and names the tag as the client gave it.
#[tokio::test]
async fn test_hashtag_stream() {
    let ctx = TestContext::new("stream-tag").await;
    let mut ws = ws_connect(&ctx, "", &ctx.bob_token).await;
    send(
        &mut ws,
        json!({"type": "subscribe", "stream": "hashtag", "tag": "RustEunha"}),
    )
    .await;
    ctx.api
        .post_status(&ctx.alice_token, "Hello #rusteunha world", "public")
        .await;
    let event = next_event(&mut ws).await.expect("a hashtag update");
    assert_eq!(event["stream"], json!(["hashtag", "RustEunha"]));
    ctx.api
        .post_status(&ctx.alice_token, "No tag here", "public")
        .await;
    assert!(quiet(&mut ws).await);

    send(&mut ws, json!({"type": "subscribe", "stream": "hashtag"})).await;
    assert_eq!(
        next_event(&mut ws).await.unwrap(),
        json!({"error": "Missing tag name parameter", "status": 400})
    );
}

/// The viewer's blocks, mutes and languages keep posts off a public stream.
#[tokio::test]
async fn test_public_stream_filters_for_the_viewer() {
    let ctx = TestContext::new("stream-filters").await;
    let mut ws = ws_connect(&ctx, "public:local", &ctx.bob_token).await;

    let blocked = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.alice_id),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(blocked.status().as_u16(), 200);
    ctx.api
        .post_status(&ctx.alice_token, "blocked", "public")
        .await;
    assert!(quiet(&mut ws).await);
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/unblock", ctx.alice_id),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    sqlx::query("UPDATE users SET chosen_languages = '{de}' WHERE account_id = $1")
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    // The connection read the languages when it was opened.
    let mut ws = ws_connect(&ctx, "public:local", &ctx.bob_token).await;
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "in English", "language": "en"}),
        )
        .await;
    assert!(quiet(&mut ws).await);
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "auf Deutsch", "language": "de"}),
        )
        .await;
    assert_eq!(next_event(&mut ws).await.unwrap()["event"], "update");
}

/// A keyword filter marks a public post with what the streaming server
/// knows of the filter, and a change to the filters is not sent on.
#[tokio::test]
async fn test_keyword_filters_on_public_streams() {
    let ctx = TestContext::new("stream-keywords").await;
    let mut ws = ws_connect(&ctx, "public:local", &ctx.bob_token).await;
    let created = ctx
        .api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.bob_token),
            &json!({
                "title": "no spoilers",
                "context": ["public"],
                "filter_action": "hide",
                "keywords_attributes": [{"keyword": "spoiler", "whole_word": true}],
            }),
        )
        .await;
    assert_eq!(created.status().as_u16(), 200);
    let filter: Value = created.json().await.unwrap();
    // `filters_changed` has no payload, so it never reaches the client.
    assert!(quiet(&mut ws).await);

    ctx.api
        .post_status(&ctx.alice_token, "A big Spoiler ahead", "public")
        .await;
    let status = payload(&next_event(&mut ws).await.unwrap());
    assert_eq!(
        status["filtered"],
        json!([{
            "filter": {
                "id": filter["id"],
                "title": "no spoilers",
                "context": ["public"],
                "expires_at": null,
                "filter_action": "hide",
            },
            "keyword_matches": ["Spoiler"],
            "status_matches": null,
        }])
    );

    ctx.api
        .post_status(&ctx.alice_token, "spoilers are words too", "public")
        .await;
    assert_eq!(
        payload(&next_event(&mut ws).await.unwrap())["filtered"],
        json!([])
    );
}

/// A feed whose access setting is `disabled` is filtered out of the stream.
#[tokio::test]
async fn test_disabled_feed_is_filtered() {
    let ctx = TestContext::new("stream-feed-access").await;
    set_setting(&ctx.db, "local_live_feed_access", "disabled").await;
    let mut ws = ws_connect(&ctx, "public", &ctx.bob_token).await;
    ctx.api
        .post_status(&ctx.alice_token, "local", "public")
        .await;
    assert!(quiet(&mut ws).await);
}

// ── the user's own streams ────────────────────────────────────────────────

/// `user` carries the home timeline rendered for the viewer: one's own posts
/// and those of whom one follows.
#[tokio::test]
async fn test_user_stream() {
    let ctx = TestContext::new("stream-user").await;
    signed_in(&ctx, &ctx.alice_id).await;
    signed_in(&ctx, &ctx.bob_id).await;
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    let mut alice = ws_connect(&ctx, "user", &ctx.alice_token).await;
    let mut bob = ws_connect(&ctx, "user", &ctx.bob_token).await;

    ctx.api
        .post_status(&ctx.alice_token, "User stream test", "private")
        .await;
    for ws in [&mut alice, &mut bob] {
        let event = next_event(ws).await.expect("a home update");
        assert_eq!(event["event"], "update");
        assert_eq!(event["stream"], json!(["user"]));
        let status = payload(&event);
        assert_eq!(status["favourited"], false);
        assert_eq!(status["reblogged"], false);
        assert_eq!(status["bookmarked"], false);
        assert_eq!(status["filtered"], json!([]));
    }

    // Nobody follows bob.
    ctx.api
        .post_status(&ctx.bob_token, "only mine", "public")
        .await;
    assert_eq!(next_event(&mut bob).await.unwrap()["event"], "update");
    assert!(quiet(&mut alice).await);
}

/// Home updates go only to users who signed in recently. An API request
/// signs its user in again (`UserTrackingConcern`), so bob's sign-in is
/// taken back after he follows; streaming itself signs no one in.
#[tokio::test]
async fn test_user_stream_needs_a_recent_sign_in() {
    let ctx = TestContext::new("stream-user-inactive").await;
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    sqlx::query("UPDATE users SET current_sign_in_at = NULL WHERE account_id = $1")
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let mut bob = ws_connect(&ctx, "user", &ctx.bob_token).await;
    ctx.api
        .post_status(&ctx.alice_token, "into the void", "public")
        .await;
    assert!(quiet(&mut bob).await);
}

/// `user:notification` carries notifications and nothing else.
#[tokio::test]
async fn test_user_notification_stream() {
    let ctx = TestContext::new("stream-notif").await;
    signed_in(&ctx, &ctx.alice_id).await;
    let mut ws = ws_connect(&ctx, "user:notification", &ctx.alice_token).await;
    ctx.api
        .post_status(&ctx.alice_token, "my own post", "public")
        .await;
    assert!(quiet(&mut ws).await);

    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    let event = next_event(&mut ws).await.expect("a notification");
    assert_eq!(event["event"], "notification");
    assert_eq!(event["stream"], json!(["user:notification"]));
    assert_eq!(payload(&event)["type"], "follow");
}

/// A direct message reaches the `direct` stream as a `conversation`.
#[tokio::test]
async fn test_direct_stream() {
    let ctx = TestContext::new("stream-direct").await;
    let mut ws = ws_connect(&ctx, "direct", &ctx.alice_token).await;
    let alice = sqlx::query_scalar::<_, String>("SELECT username FROM accounts WHERE id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    ctx.api
        .post_status(&ctx.bob_token, &format!("@{alice} psst"), "direct")
        .await;
    let event = next_event(&mut ws).await.expect("a conversation");
    assert_eq!(event["event"], "conversation");
    assert_eq!(event["stream"], json!(["direct"]));
    let conversation = payload(&event);
    assert!(conversation["last_status"]["content"]
        .as_str()
        .unwrap()
        .contains("psst"));
}

/// A list streams only to its owner.
#[tokio::test]
async fn test_list_stream_is_the_owners() {
    let ctx = TestContext::new("stream-list").await;
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "l"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let mut ws = ws_connect(&ctx, "", &ctx.bob_token).await;
    send(
        &mut ws,
        json!({"type": "subscribe", "stream": "list", "list": list["id"]}),
    )
    .await;
    assert_eq!(
        next_event(&mut ws).await.unwrap(),
        json!({"error": "Not authorized to stream this list", "status": 401})
    );
}

/// Several streams on one socket, each named in its events, and an
/// unsubscribe that stops one.
#[tokio::test]
async fn test_multiplexing() {
    let ctx = TestContext::new("stream-mux").await;
    let mut ws = ws_connect(&ctx, "", &ctx.bob_token).await;
    send(
        &mut ws,
        json!({"type": "subscribe", "stream": "public:local"}),
    )
    .await;
    send(&mut ws, json!({"type": "subscribe", "stream": ["public"]})).await;
    // Subscribing twice changes nothing.
    send(&mut ws, json!({"type": "subscribe", "stream": "public"})).await;

    ctx.api
        .post_status(&ctx.alice_token, "Mux test", "public")
        .await;
    let mut streams = vec![
        next_event(&mut ws).await.unwrap()["stream"].clone(),
        next_event(&mut ws).await.unwrap()["stream"].clone(),
    ];
    streams.sort_by_key(|s| s.to_string());
    assert_eq!(streams, vec![json!(["public"]), json!(["public:local"])]);
    assert!(quiet(&mut ws).await);

    send(&mut ws, json!({"type": "unsubscribe", "stream": "public"})).await;
    ctx.api
        .post_status(&ctx.alice_token, "After", "public")
        .await;
    assert_eq!(
        next_event(&mut ws).await.unwrap()["stream"],
        json!(["public:local"])
    );
    assert!(quiet(&mut ws).await);

    send(&mut ws, json!({"type": "subscribe", "stream": "nonsense"})).await;
    assert_eq!(
        next_event(&mut ws).await.unwrap(),
        json!({"error": "Unknown stream type", "status": 400})
    );
}

/// A binary message closes the socket with 1003.
#[tokio::test]
async fn test_binary_messages_close_the_socket() {
    let ctx = TestContext::new("stream-binary").await;
    let mut ws = ws_connect(&ctx, "", &ctx.bob_token).await;
    ws.send(Message::Binary(vec![1, 2, 3].into()))
        .await
        .unwrap();
    let frame = timeout(Duration::from_secs(3), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(frame))) => break frame,
                Some(Ok(_)) => continue,
                _ => break None,
            }
        }
    })
    .await
    .unwrap()
    .expect("a close frame");
    assert_eq!(u16::from(frame.code), 1003);
}

// ── server-sent events and the plain endpoints ────────────────────────────

/// The event stream: `:)` first, then `event:` and `data:` lines.
#[tokio::test]
async fn test_event_stream() {
    let ctx = TestContext::new("stream-sse").await;
    let mut response = ctx
        .api
        .get("/api/v1/streaming/public/local", Some(&ctx.bob_token))
        .await;
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    let first = response.chunk().await.unwrap().unwrap();
    assert_eq!(&first[..], b":)\n");
    settle().await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "over SSE", "public")
        .await;
    let chunk = timeout(Duration::from_secs(3), response.chunk())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = String::from_utf8(chunk.to_vec()).unwrap();
    let data = text
        .strip_prefix("event: update\ndata: ")
        .and_then(|rest| rest.strip_suffix("\n\n"))
        .unwrap_or_else(|| panic!("not an update event: {text:?}"));
    let streamed: Value = serde_json::from_str(data).unwrap();
    assert_eq!(streamed["id"], status["id"]);
}

/// What the event stream endpoints refuse, as the streaming server words it.
#[tokio::test]
async fn test_event_stream_errors() {
    let ctx = TestContext::new("stream-sse-errors").await;
    let read_only =
        seed_token_with_scopes(&ctx.db, ctx.alice_id.parse().unwrap(), "read:notifications").await;
    for (path, token, status, error) in [
        (
            "/api/v1/streaming",
            Some(ctx.bob_token.as_str()),
            400,
            "Unknown channel requested",
        ),
        (
            "/api/v1/streaming/nope",
            Some(ctx.bob_token.as_str()),
            400,
            "Unknown channel requested",
        ),
        (
            "/api/v1/streaming/public",
            None,
            401,
            "Missing access token",
        ),
        (
            "/api/v1/streaming/public?access_token=bad",
            None,
            401,
            "Invalid access token",
        ),
        (
            "/api/v1/streaming/public",
            Some(read_only.as_str()),
            401,
            "Access token does not have the required scopes",
        ),
        (
            "/api/v1/streaming/hashtag",
            Some(ctx.bob_token.as_str()),
            400,
            "Missing tag name parameter",
        ),
        (
            "/api/v1/streaming/list",
            Some(ctx.bob_token.as_str()),
            400,
            "Missing list name parameter",
        ),
        (
            "/api/v1/streaming/list?list=1",
            Some(ctx.bob_token.as_str()),
            401,
            "Not authorized to stream this list",
        ),
    ] {
        let response = ctx.api.get(path, token).await;
        assert_eq!(response.status().as_u16(), status, "{path}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body, json!({"error": error}), "{path}");
    }
}

#[tokio::test]
async fn test_health() {
    let ctx = TestContext::new("stream-health").await;
    let response = ctx.api.get("/api/v1/streaming/health", None).await;
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.headers()["content-type"], "text/plain");
    assert_eq!(response.text().await.unwrap(), "OK");
}

/// Feed an activity from a remote actor through the inbox queue.
async fn receive(ctx: &TestContext, activity: Value) {
    sqlx::query(
        r#"INSERT INTO eunha.inbox_jobs (activity, activity_type, actor_uri, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())"#,
    )
    .bind(&activity)
    .bind(activity["type"].as_str().unwrap())
    .bind(activity["actor"].as_str().unwrap())
    .execute(&ctx.db)
    .await
    .unwrap();
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();
}

/// A remote post reaches `public` and `public:remote`, not `public:local`,
/// and its deletion follows it.
#[tokio::test]
async fn test_remote_posts_on_public_streams() {
    let ctx = TestContext::new("stream-remote").await;
    let domain = "stream-remote.invalid";
    let actor = format!("https://{domain}/users/eve");
    sqlx::query(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'eve', $2, 'eve', '', $3, $3, 'remote-key',
                   $3 || '/inbox', $3 || '/outbox', now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(domain)
    .bind(&actor)
    .execute(&ctx.db)
    .await
    .unwrap();
    // A post is only taken in from someone a local account follows.
    sqlx::query(
        r#"INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
           SELECT $1, $2, id, now(), now() FROM accounts WHERE uri = $3"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(ctx.bob_id.parse::<i64>().unwrap())
    .bind(&actor)
    .execute(&ctx.db)
    .await
    .unwrap();
    let mut remote = ws_connect(&ctx, "public:remote", &ctx.bob_token).await;
    let mut local = ws_connect(&ctx, "public:local", &ctx.bob_token).await;

    let note = format!("https://{domain}/notes/1");
    let public = "https://www.w3.org/ns/activitystreams#Public";
    receive(
        &ctx,
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("https://{domain}/activities/1"),
            "type": "Create",
            "actor": actor,
            "to": [public],
            "object": {
                "id": note,
                "type": "Note",
                "attributedTo": actor,
                "content": "<p>from afar</p>",
                "to": [public],
                "published": chrono::Utc::now().to_rfc3339(),
            },
        }),
    )
    .await;
    let event = next_event(&mut remote).await.expect("a remote update");
    assert_eq!(event["stream"], json!(["public:remote"]));
    let status = payload(&event);
    assert_eq!(status["account"]["acct"], format!("eve@{domain}"));
    assert!(quiet(&mut local).await);

    receive(
        &ctx,
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("https://{domain}/activities/2"),
            "type": "Delete",
            "actor": actor,
            "to": [public],
            "object": {"id": note, "type": "Tombstone"},
        }),
    )
    .await;
    let event = next_event(&mut remote).await.expect("a delete");
    assert_eq!(event["event"], "delete");
    assert_eq!(event["payload"], status["id"]);
}

/// Streams go through Redis, as Mastodon's do: another process serving the
/// same instance — `eunha accounts`, a second server — reaches them, and an
/// instance with another key prefix on the same Redis does not.
#[tokio::test]
async fn test_streams_are_reached_through_redis_under_the_instance_prefix() {
    let ctx = TestContext::new("stream-redis").await;
    signed_in(&ctx, &ctx.alice_id).await;
    let mut ws = ws_connect(&ctx, "user:notification", &ctx.alice_token).await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    assert!(ctx.state.streaming.is_online(alice).await);

    let config = (*ctx.state.config).clone();
    let other_tenant = {
        let mut config = config.clone();
        config.redis_key_prefix = format!("{}-other", config.redis_key_prefix);
        eunha::state::AppState::new(ctx.state.db.clone(), config)
            .await
            .unwrap()
    };
    assert!(!other_tenant.streaming.is_online(alice).await);
    other_tenant
        .streaming
        .notification(alice, json!({"id": "1", "type": "follow"}))
        .await;
    assert!(quiet(&mut ws).await, "another instance's channel");

    let other_process = eunha::state::AppState::new(ctx.state.db.clone(), config)
        .await
        .unwrap();
    other_process
        .streaming
        .notification(alice, json!({"id": "1", "type": "follow"}))
        .await;
    let event = next_event(&mut ws).await.expect("the notification");
    assert_eq!(event["event"], "notification");
    assert_eq!(payload(&event)["type"], "follow");
}

/// `push_to_home` streams only what `add_to_feed` took: a boost of a post
/// already among the newest in the home feed is aggregated away, and streams
/// nothing, while the post itself did.
#[tokio::test]
async fn test_home_updates_follow_what_the_feed_took() {
    let ctx = TestContext::new("stream-home-aggregate").await;
    let (carol_id, carol_token) =
        crate::helpers::seed_account_and_token(&ctx.db, &ctx.domain, "carol", "carol@example.test")
            .await;
    signed_in(&ctx, &ctx.alice_id).await;
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api
        .follow(&ctx.alice_token, &carol_id.to_string())
        .await;
    // Build alice's home feed, so that it is the feed that answers.
    ctx.api.home_timeline(&ctx.alice_token).await;
    let mut alice = ws_connect(&ctx, "user", &ctx.alice_token).await;

    let post = ctx
        .api
        .post_status(&ctx.bob_token, "boost me", "public")
        .await;
    let event = next_event(&mut alice).await.expect("the post");
    assert_eq!(event["event"], "update");
    assert_eq!(payload(&event)["id"], post["id"]);

    let boosted = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{}/reblog", post["id"].as_str().unwrap()),
            Some(&carol_token),
            &json!({}),
        )
        .await;
    assert_eq!(boosted.status().as_u16(), 200);
    assert!(
        quiet(&mut alice).await,
        "the aggregated boost streams nothing"
    );
}
