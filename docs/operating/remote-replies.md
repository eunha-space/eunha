Remote replies
==============

A server only pushes a post to the servers that follow its author or are
mentioned in it, so a remote thread seen here is usually missing most of its
replies. Eunha fills the gap as Mastodon does, by reading the `replies`
collection a post links to and fetching what it lists, at two moments.


When a post arrives
-------------------

A remote post delivered in a `Create` has the first page of its `replies`
read in the background: the page embedded in the post, or the one its
`first` links to on the author's server. Up to five of the replies listed
there, on the author's server, are fetched. Mastodon's own posts embed a
first page holding the author's self-replies, so for a new post this usually
costs no request for the page.

Each reply is fetched by a job of its own and stored as any fetched post is
(see [below](#how-a-fetched-post-is-stored)), so a reply new to us has the
first page of its own `replies` read in turn.


When a thread is opened
-----------------------

When a signed-in user opens a remote post's thread
(`GET /api/v1/statuses/:id/context`), eunha walks its whole reply tree in
the background, if the post is

 -  public or unlisted,
 -  at least five minutes old, and
 -  not walked in the last fifteen minutes (`statuses.fetched_replies_at`).

The walk starts at the post, fetched again from its server, and reads its
`replies` collection page by page until it has at least five items. The post
itself is processed again from that document, as an `Update`. Each reply
listed is queued to be fetched and processed as above, and the walk fetches
it too, to read its own collection the same way. Collection pages are only
read from the server of the post they belong to. A reply already held is
walked again only if it is not local, not new (five minutes) and not walked
in the last fifteen minutes, and that walk counts as its own; replies held
for a post that its collection no longer lists, from authors nobody here
follows, are walked too. One walk stops after discovering 1,000 replies or
reading 500 collection pages; the replies it has discovered by then are
still fetched.

The walk is an `ActivityPub::FetchAllRepliesWorker` in the
[job queue](./jobs.md), retried three times when a request for the post
itself is not answered at all, or a collection page fails for the time being
(a `5xx`, `401`, `408` or `429`); a post its server answers for with an
error is not walked, and not retried; the first page read when a post arrives
is an `ActivityPub::FetchRepliesWorker`, and each reply is a
`FetchReplyWorker`, retried three times when it cannot be processed. All three
wait in the `pull` queue. A reply is therefore fetched twice when the walk
reaches it, once to read its collection and once to store it, as in Mastodon.


How a fetched post is stored
----------------------------

Every post eunha fetches — a reply, a pinned post, a boosted or quoted post,
one looked up by its URL — is stored as Mastodon's `FetchRemoteStatusService`
stores it: as the `Create` it would have come in, through the same handler
delivered posts go through, except that it is taken whether or not anyone
here follows its author. A post already held from the same author is
processed again as an `Update`: it changes only if it says it was edited
since (`updated`), and otherwise only its quote policy, its poll's tallies,
its quote's approval and its counts are refreshed. A fetched `Announce` is
processed as the boost it is.

A remote poll is fetched again the same way when a signed-in user asks for
it (`GET /api/v1/polls/:id`, Mastodon's `FetchRemotePollService`), signed on
their behalf, if it may be stale: it has never been fetched since it was
stored, or not since it closed, and not in the last minute. A server that
does not answer makes the request fail with `503`, as in Mastodon.

A `Note` or `Question` is a post as it is. An `Article`, `Page`, `Image`,
`Video`, `Audio` or `Event` is converted, as Mastodon converts it: its text
is its title as a heading, its summary, and a link to it, and it has no
content warning. A post's language is the first one its `contentMap`,
`nameMap` or `summaryMap` names, in the spelling of the language Mastodon
supports when it is one.

A post's server may report how often it was favourited and boosted
(`likes` and `shares` with a `totalItems`). Those counts are kept beside
eunha's own (`status_stats.untrusted_favourites_count` and
`untrusted_reblogs_count`) and served in their place, moving with each
favourite and boost made here.

A post older than six hours when it is stored does not notify the accounts
it mentions and is not added to home and list feeds, as Mastodon distributes
only posts within its real-time window. A post its server answers `404` for
when fetched, and that is public or unlisted, is deleted here.

One chain of fetches — a post, the replies its `Create` reads, and theirs —
stops after a thousand posts (`status_discovery_per_request:*` in Redis); a
fetch the chain did not start from another begins a chain of its own.


Async refreshes
---------------

The response that starts a walk carries Mastodon's header

~~~~
Mastodon-Async-Refresh: id="…", retry=3, result_count=0
~~~~

and so does every response for the same thread while the walk runs, signed
in or not. The client polls `GET /api/v1_alpha/async_refreshes/:id` (a user
token with the `read` scope) until its `status` is `finished`;
`result_count` counts the replies that were new. Generating an annual report
(`POST /api/v1/annual_reports/:year/generate`) works the same way, with
`retry=2` and no count, and `GET /api/v1/annual_reports/:year/state` answers
`generating` with the header while it runs.

A refresh is a Redis hash, `context:{status_id}:refresh` or
`wrapstodon:{account_id}:{year}`, kept in the coordination pool under the
instance's prefix. It lives a day while running and an hour once finished.
A walk and the replies it queues are one batch (`worker_batch:{id}`, in
the same pool, living an hour), and the refresh is finished when the last of
its jobs has run once, whether or not it succeeded; a retry that runs after
the refresh has finished counts in none. A job the instance stops in the middle
of leaves the batch as it stops, so the refresh is not left running for the day.

Mastodon's id is the key signed by Rails' message verifier, keyed from
`SECRET_KEY_BASE`, and an instance given its Mastodon's
[`secret_key_base`](./instances.md#mastodon-s-secret-key-base) signs exactly
that. Without it, eunha's id has the same shape — the key in URL-safe base64,
`--`, and a hex HMAC-SHA256 — keyed from the instance's VAPID private key, and
that kind is read either way. Clients treat the id as opaque and it lives a
day at most, and no Mastodon process ever reads an id eunha handed out, so
the two need not agree. Rotating either key only makes refreshes already
handed out under it unreadable.

The home timeline carries a refresh, with `retry=5` and a `206` status, while
the member's home feed is rebuilt; see
[accounts](./accounts.md#the-home-feed-while-away).
