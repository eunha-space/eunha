Auxiliary service providers
===========================

A Fediverse Auxiliary Service Provider (FASP) is a service a server hands work
to that it cannot do well alone: finding accounts it has never heard of,
recommending whom to follow, noticing what is trending. Mastodon 4.7 talks to
them as an experimental feature, and eunha ports that implementation — its
tables, its endpoints, its wire formats and its signatures — so that a
provider written against Mastodon works against eunha unchanged. The protocol
is specified in the [FASP specifications]; where the two disagree, eunha
follows Mastodon's code.

[FASP specifications]: https://github.com/mastodon/fediverse_auxiliary_service_provider_specifications


Turning it on
-------------

Mastodon reads `EXPERIMENTAL_FEATURES=fasp` from its environment. Instances
sharing a process share an environment, so eunha reads the list from each
instance's `[instance]` table, and a `SIGHUP` reload picks up a change:

~~~~ toml
[instance]
domain = "garden.eunha.space"
experimental_features = ["fasp"]
~~~~

While it is off, `/api/fasp/` and `/api/v1/admin/fasp/` answer 404, nothing
is announced, and no provider is asked anything.


Registering a provider
----------------------

A provider registers itself: it is given the instance's address, and posts its
name, base URL, its own identifier for the server and its Ed25519 public key to
`POST /api/fasp/registration`. Eunha records it unconfirmed with a new Ed25519
key pair of its own, and answers with the provider's id here, its public key,
and the page that finishes the registration,
`https://<domain>/admin/fasp/providers/<id>/registration/new`.

That page, under *FASP* in the web client's admin, shows the provider's name and
the SHA-256 fingerprint of the key it sent. An administrator who started the
registration compares both with what the provider shows, then confirms or
rejects it. Confirming asks the provider's `/provider_info` for its privacy
policy, capabilities, sign-in URL, contact address and fediverse account, and
records them; nothing is saved if the provider cannot be asked.

The provider's edit page then lists its capabilities. Saving them activates
each one ticked and deactivates each one not, with a `POST` or `DELETE` to the
provider's `/capabilities/<id>/<major version>/activation`, for those that
changed. A provider whose sign-in URL is known gets a link to it; one with the
`callback` capability enabled gets a debug call button, which asks it to call
back, and the callbacks it makes are listed under *Debug callbacks*.

The pages read `/api/v1/admin/fasp/providers` (`GET`, and `GET`, `PUT` and
`DELETE` on `providers/:id`), `providers/:id/registration` and
`providers/:id/debug_calls` (`POST`), and `debug/callbacks` with
`DELETE debug/callbacks/:id`. Every one needs `manage_federation`, which is what
Mastodon's FASP pages ask for, and none writes an audit log entry, as theirs do
not. Deleting a provider deletes its subscriptions, backfill requests and debug
callbacks with it.


Signatures
----------

Each side signs what it sends the other with the Ed25519 keys exchanged at
registration, using HTTP Message Signatures ([RFC 9421]) as Mastodon makes
them with Linzer:

 -  A request covers `@method`, `@target-uri` and `content-digest`, and every
    request carries a `Content-Digest` (RFC 9530), an empty body's included.
    Eunha's requests are keyed by the identifier the provider gave; a
    provider's are keyed by its id here, must be no more than five minutes old,
    and must come from a confirmed provider.
 -  An answer covers `@status` and `content-digest`, without a `keyid`. Eunha
    signs each answer its provider API gives — refusals excepted, as Rails skips
    the signing for those — and refuses an answer from a provider whose
    `Content-Digest` is missing or not exactly its body's, or whose signature
    does not verify.

A request that fails to authenticate is answered with a bare 401. The answers
are served outside the compression layer, since a compressed body would no
longer match its digest.

Requests to providers go out through the SSRF-guarded client federation uses,
so a provider on a private address is reached only inside
`allowed_private_networks`. A request that cannot connect counts against the
provider's host at the resolution of minutes, as Mastodon's
`DeliveryFailureTracker` does for providers: failures in five different minutes
mark the host unavailable in `unavailable_domains`, which also stops federation
deliveries to it, and any answered request clears them. An unavailable provider
is skipped until an hour after its last failure.

[RFC 9421]: https://www.rfc-editor.org/rfc/rfc9421.html


Sharing data
------------

With the data sharing capability, a provider subscribes to events with
`POST /api/fasp/data_sharing/v0/event_subscriptions` — a `category` of
`account` or `content`, a `subscriptionType` of `lifecycle` or `trends`, a
`maxBatchSize`, and for trends a `threshold` whose `timeframe` (minutes),
`shares`, `likes` and `replies` default to 15, 3, 3 and 3 — and withdraws one
with `DELETE …/event_subscriptions/:id`. Eunha then posts announcements to the
provider's `/data_sharing/v0/announcements`:

 -  a status is announced as `new`, `update` and `delete` when it is public and
    its author is indexable, boosts included;
 -  an account is announced as `new`, `update` and `delete` while it is
    discoverable, and as `update` when a change turned discoverability off;
 -  a status is announced as `trending` when a favourite, boost or reply brings
    what it has had within the subscription's timeframe to its threshold.

The hooks sit where eunha creates, edits and deletes statuses — posting,
scheduled posts, boosts, and what arrives over federation — where it favourites
them, and where it creates accounts, updates a profile and deletes an account.
Eunha does not yet record the `discoverable` and `indexable` flags remote
actors publish, so for now only local accounts and their posts qualify.

A provider asks for what already exists with
`POST /api/fasp/data_sharing/v0/backfill_requests`, naming a category and a
`maxCount` (100 when not given). Eunha announces the newest batch at once —
discoverable accounts other than the instance actor, or public statuses that are
not boosts by indexable accounts — saying whether more are left, and announces
each next one when the provider posts to
`…/backfill_requests/:id/continuation`. The last batch marks the request
fulfilled.


Searches and recommendations
----------------------------

With `account_search` enabled, a signed-in account search through
`/api/v2/search` — one that is not resolving a URL — also asks each such
provider's `/account_search/v0/search` for up to ten matches. The accounts it
names that the instance does not know are fetched in the background, for the
next search to find; the answer carries a `Mastodon-Async-Refresh` header the
client can poll at `/api/v1_alpha/async_refreshes/:id`, which counts the
accounts fetched. A follow-up request carrying `Mastodon-Async-Refresh-Id`, or a
search already running for the same query, starts nothing.

With `follow_recommendation` enabled, `GET /api/v2/suggestions` asks each such
provider's `/follow_recommendation/v0/accounts` whom the account might follow,
in the same way. Each account it names that the instance did not know is
fetched and kept as a recommendation in `fasp_follow_recommendations`, and is
suggested with the source `fasp` from then on. Recommendations are forgotten
after a day.


Background work
---------------

Mastodon runs these on Sidekiq's `fasp` queue. Eunha runs each in the
background as soon as it is asked for, in the instance's tenant span, retried
on Sidekiq's schedule as often as Mastodon's worker allows: five times for
announcements and backfills, never for searches and recommendations. A
provider is only called while it is confirmed and available, and a request
that cannot reach it is retried only while it stays available. A retry waiting
when the process stops is not taken up again.
