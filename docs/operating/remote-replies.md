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
there, on the author's server, are fetched and stored. Mastodon's own posts
embed a first page holding the author's self-replies, so for a new post this
usually costs no request at all.

Replies fetched this way do not in turn have their own `replies` read; in
Mastodon each one goes through the same `Create` processing and does.


When a thread is opened
-----------------------

When a signed-in user opens a remote post's thread
(`GET /api/v1/statuses/:id/context`), eunha walks its whole reply tree in
the background, if the post is

 -  public or unlisted,
 -  at least five minutes old, and
 -  not walked in the last fifteen minutes (`statuses.fetched_replies_at`).

The walk starts at the post, fetched again from its server, and reads its
`replies` collection page by page until it has at least five items. Each
reply listed is fetched and stored, and its own collection read the same
way. Collection pages are only read from the server of the post they belong
to. A reply already held is walked again only if it is not local, not new
(five minutes) and not walked in the last fifteen minutes, and that walk
counts as its own; replies held for a post that its collection no longer
lists, from authors nobody here follows, are walked too. One walk stops after
discovering 1,000 replies or reading 500 collection pages; the replies it
has discovered by then are still fetched. Each reply is fetched once, with a
single request that serves both for storing it and for reading its
collection.

The walk is a task in the instance's process, not a queued job. A failed
fetch is not retried, as Mastodon's `FetchReplyWorker` retries it three
times; the post's next walk, fifteen minutes on, picks up what was missed.
A post that is already held is not refreshed by being fetched again.


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
If the instance stops while the work runs, the refresh is marked finished
rather than left running for the day.

Mastodon's id is the key signed by Rails' message verifier, keyed from
`SECRET_KEY_BASE`, and an instance given its Mastodon's
[`secret_key_base`](./instances.md#mastodon-s-secret-key-base) signs exactly
that. Without it, eunha's id has the same shape — the key in URL-safe base64,
`--`, and a hex HMAC-SHA256 — keyed from the instance's VAPID private key, and
that kind is read either way. Clients treat the id as opaque and it lives a
day at most, and no Mastodon process ever reads an id eunha handed out, so
the two need not agree. Rotating either key only makes refreshes already
handed out under it unreadable.

A home timeline is never answered with a refresh: eunha reads a feed Redis
does not hold from the database there and then, where Mastodon answers `206`
with a partial feed while it regenerates one.
