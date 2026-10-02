Quotes
======

Eunha keeps quotes in Mastodon's `quotes` table and moves them through the
states Mastodon does: `pending`, `accepted`, `rejected`, `revoked` and
`deleted`. The model's own rules live in *src/quotes.rs* (`Quote#accept!`,
`Quote#reject!`, the counter they move, `TagManager#approval_uri_for`,
`RevokeQuoteService`, and the `DistributionWorker` update a changed status
gets); what arrives over ActivityPub is handled in
*src/api/ap/inbox/quote.rs*.


Quoting
-------

A local quote of a local post is accepted at once when `StatusPolicy#quote?`
lets the quoter quote it; a quote that may not be made, of a post that is not
there, or of a direct one, is the same 404 Mastodon gives
(`quoted_status_not_found`). A quote of a boost quotes the boosted post. The
quoted author is mentioned silently in the quote (`ensure_quoted_access`), a
mention an edit leaves alone, and is notified with a `quote` notification
about the `Quote`, as `LocalNotificationWorker` writes it.

A quote of a remote post stays pending, and a `QuoteRequest`, named under the
quoter (`/quote_requests/{uuid}`), goes to the quoted author's own inbox.
Their `Accept` is taken when its `result` is on their host and the quote is
still pending; the quote is accepted with that stamp, and the quoting post is
sent again as an `Update`. A `Reject` revokes an accepted quote and rejects
any other.

A remote post's quote is recorded pending, or `deleted` when the post quotes
a `Tombstone`, and verified as `ActivityPub::VerifyQuoteService` verifies it:
the quoted post is fetched if need be, a self-quote is accepted, and a
`quoteAuthorization` stamp is fetched and accepted only if it is a
`QuoteAuthorization` on its author's host naming both posts. A stamp that is
gone rejects the quote. A quote of a local post waits for its `QuoteRequest`,
which is accepted when the requester may quote the post and the quoting post's
quote names it, and answered at the requester's own inbox. An edit that
changes the quoted post replaces the quote, and a changed stamp sends it back
to pending.

The `quotes_count` of the quoted post counts accepted quotes; see
`quotes-count-counts-accepted` in [divergences](./divergences.md) for where
that departs from Mastodon's arithmetic.


Revoking
--------

The quoted author may revoke a quote
(`POST /api/v1/statuses/:status_id/quotes/:id/revoke`, `QuotePolicy#revoke?`),
and deleting a quote of a local post revokes it first, as
`RemoveStatusService` does. Revoking rejects the quote (an accepted one
becomes `revoked`), refreshes the quoting post in local timelines, and sends
the `Delete` of its `QuoteAuthorization` stamp, with a Linked Data signature
whatever the mode, to every server that saw either post (`StatusReachFinder`
with `unsafe`). A local author's stamp is named
`{actor}/quote_authorizations/{id}`, under the actor's id scheme, and never
stored, as Mastodon validates. A remote author's `Delete` of a stamp revokes
the quote here.

Changing a post's quote policy (`PUT /api/v1/statuses/:id/interaction_policy`)
refreshes it in local timelines, without notifications, and sends it as an
`Update`.


What still differs
------------------

 -  Mastodon retries a stamp it could not fetch for now
    (`RefetchAndVerifyQuoteWorker`) and refreshes stale stamps weekly
    (`QuoteRefreshWorker`); eunha leaves such a quote pending.
 -  A remote `Delete` of a stamp is not forwarded to the followers of local
    accounts that boosted or quoted the quoting post (`ActivityPub::Forwarder`).
 -  Statuses are soft-deleted, and their quotes kept: a deleted quoted post is
    shown as `deleted` from its row, where Mastodon nullifies
    `quoted_status_id`.
