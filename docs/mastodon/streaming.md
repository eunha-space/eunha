Streaming
=========

Mastodon's streaming API is a separate Node server, *streaming/index.js*, that
the Rails app talks to through Redis: Rails publishes `{event, payload}`
messages on `timeline:*` channels, and the streaming server passes them to the
connections that subscribed. Eunha serves both halves, through Redis as
Mastodon does, with the same channels and the same messages:

 -  *src/streaming.rs* is the bus: `PUBLISH` on the `timeline:*` channels,
    and one subscribing connection per instance in each process that serves
    streams, subscribed to a channel while a stream listens on it. Channels
    and keys go under the instance's Redis key prefix, as Mastodon's go under
    `REDIS_NAMESPACE`, so instances sharing a Redis never hear each other,
    and a separate process, such as `eunha accounts`, reaches the same
    streams. A stream sets `subscribed:<channel>` for eighteen minutes when
    it subscribes and every six minutes while it lasts, as the streaming
    server's heartbeat does, and that key is what the publishing side asks
    (`StreamBus::is_subscribed`).
 -  *src/streaming/fan\_out.rs* publishes what `FanOutOnWriteService`,
    `FeedManager`, `PushUpdateWorker`, `RemoveStatusService` and the
    notification, conversation and announcement workers publish.
 -  *src/api/mastodon/streaming.rs* is the streaming server.


Endpoints
---------

 -  `/api/v1/streaming/health` answers `OK`, as `text/plain`.
 -  A WebSocket upgrade anywhere under `/api/v1/streaming` is the WebSocket;
    the path is not read, only `stream` and the messages.
 -  Any other request under `/api/v1/streaming` is an event stream, for the
    stream its path names, or 400, `Unknown channel requested`.

The event stream paths are `user`, `user/notification`, `public`,
`public/local`, `public/remote` (each of the three with `only_media` for its
`:media` stream), `hashtag` and `hashtag/local` with `tag`, `list` with
`list`, and `direct`. An event stream starts with `:)`, sends `:thump` every 15
seconds, and each event as `event:` and `data:` lines.

A WebSocket takes `{"type": "subscribe", "stream": …}` and
`{"type": "unsubscribe", "stream": …}`, with `tag` or `list` beside `stream`
where the stream needs one; `stream` in the URL subscribes to one stream from
the start. Each event names the stream it came from as the client asked for
it — `["public:local"]`, `["hashtag", "Rust"]`, `["list", "12"]` — and comes as
`{"stream", "event", "payload"}`, the payload a string of JSON. Subscribing to
a stream twice changes nothing. A subscription that fails is answered with
`{"error", "status"}`, an unsubscription with
`{"error": "Error unsubscribing from channel"}`. The server pings every 30
seconds and drops a socket that did not answer the last ping; a binary message
closes it with 1003.


Authentication
--------------

Every connection needs an access token: the `Authorization` header, else the
`access_token` parameter, else the `Sec-WebSocket-Protocol` header, whose first
protocol the WebSocket then echoes back. The token must be unrevoked, and its
user neither disabled nor suspended; the streaming server does not look at a
token's expiry, and neither does eunha (eunha issues none that expire). A
WebSocket that fails this is answered before the upgrade, with 401 and the
reason in `X-Error-Message`; an event stream with 401 and `{"error": …}`.

Each stream needs `read` or `read:statuses`, and `user:notification`
`read:notifications` instead of the latter. `user` carries notifications only
for a token with `read` or `read:notifications`. A list streams only to its
owner. Every connection also listens on `timeline:access_token:<id>` and
`timeline:system:<account>`, where `kill` ends it: revoking the token,
disabling or suspending the account, deleting it, or resetting its password.


Channels and what reaches them
------------------------------

| Stream                      | Channel                                        |
| --------------------------- | ---------------------------------------------- |
| `user`                      | `timeline:<id>`, `timeline:<id>:notifications` |
| `user:notification`         | `timeline:<id>:notifications`                  |
| `public`, `public:local`, … | `timeline:public`, `timeline:public:local`, …  |
| `hashtag`, `hashtag:local`  | `timeline:hashtag:<tag>`, `…:local`            |
| `list`                      | `timeline:list:<id>`                           |
| `direct`                    | `timeline:direct:<id>`                         |

The hashtag in a subscription is normalized as the streaming server's
`normalizeHashtag` does; a post goes to its tags' names lower-cased, as
`FanOutOnWriteService` sends it, so a tag whose name folds differently, such as
`Café`, never meets its subscribers, upstream as here.

 -  `update` and `status.update` on the public and hashtag streams: a public
    post that is not a boost, by an account not silenced, rendered for nobody;
    a reply to someone else reaches the hashtag streams but not the public
    ones. A remote post only within six hours of being written.
 -  The same on `timeline:<id>` for the author, the followers and the
    followers of its hashtags whose home it would enter by
    `FeedManager#filter_from_home` and `#filter_from_tags?`, and on
    `timeline:list:<id>` for the lists `#filter_from_list?` lets it into,
    rendered for the timeline's owner: `favourited`, `reblogged`, `muted`,
    `bookmarked`, `pinned` and `filtered` are theirs. Only users who signed in
    within a week get these. A follower who also follows one of its hashtags
    gets it twice, as upstream pushes it twice. An edit also reaches the
    mentioned accounts' `timeline:<id>:notifications`.
 -  `delete` wherever the post went, to the accounts it mentions, and for each
    boost removed with it.
 -  `notification`, rendered for its recipient, unless it was filtered;
    `notifications_merged` once a notification request is accepted.
 -  `conversation` on `direct` when a direct message arrives, or the
    conversation is marked read or unread.
 -  `announcement`, `announcement.reaction` and `announcement.delete` to the
    `user` stream of every user who signed in within a week.
 -  `filters_changed` is published on `timeline:<id>` with no payload, so it
    never reaches a client; on the system channel it drops the connection's
    cached filters.


Filtering for the viewer
------------------------

What reaches `user`, `list` and `direct` was filtered and rendered for its
owner before it was published. On the public and hashtag streams the streaming
server filters `update` and `status.update` for each connection, in this order:

1.  A feed whose access setting is `disabled` (`local_live_feed_access` and
    the rest; `local_topic_feed_access` for hashtags) is dropped, unless the
    user's role may `view_feeds`. A post is local when its account's
    `username` equals its `acct`.
2.  With `chosen_languages` set, a post in another language, or with none, is
    dropped. The languages are read when the connection opens.
3.  A post by or mentioning someone the viewer blocks or mutes — expired
    mutes included — by someone who blocks the viewer, or from a domain the
    viewer blocks, is dropped.
4.  `filtered` is set from the viewer's keyword filters, of any context, with
    the filter reduced to `id`, `title`, `context`, `expires_at` and
    `filter_action`, `keyword_matches` the text that matched and
    `status_matches` null. A filter that blurs has no `filter_action`, as
    upstream leaves it undefined. Filters on posts rather than keywords do not
    apply. The text matched is the spoiler, content, poll options and media
    descriptions as the browser would read them.


Differences
-----------

 -  Mastodon pushes a home or list update when `FeedManager#add_to_feed` put
    the post in the feed. Eunha builds feeds only for users who read them, so
    it reads the feed back instead: a post the feed holds, or any post when the
    feed has not been built, counts as added. A deletion likewise goes to
    every follower streaming whose feed is not built.
 -  `conversation` is not sent when a deleted post leaves a conversation, nor
    for the direct messages a newly accepted notification request brings in.
 -  A stream that falls 256 messages behind on one channel loses the oldest,
    where the streaming server would let Redis's output buffer grow.
 -  When an instance is stopped or reloaded, its WebSockets get a close frame,
    so clients reconnect to whatever serves the host now; its event streams
    end.
