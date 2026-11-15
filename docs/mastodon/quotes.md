Quotes
======

eunha keeps quotes in Mastodon's `quotes` table and moves them through the
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
(`quoted_status_not_found`). Someone else's post that is not public or
unlisted, which `StatusPolicy#quote?` may allow, still cannot be quoted
(`Quote#validate_visibility`): nothing is posted, and the answer is a 422,
where Mastodon's unsaved status fails with a 500. A quote of a boost quotes
the boosted post. The
quoted author is mentioned silently in the quote (`ensure_quoted_access`), a
mention an edit leaves alone, and is notified with a `quote` notification
about the `Quote`, as `LocalNotificationWorker` writes it.

The web client's Quote item, in a post's boost menu (“Boost or quote”), can be
chosen only when the post's `quote_approval.current_user` is `automatic` or
`manual` and the post is public or unlisted, or is your own followers-only
post, which it quotes to your followers alone; otherwise it is disabled and
says why (“Private posts cannot be quoted”, “Only followers can quote this
post”, or “You are not allowed to quote this post”), as Mastodon's boost menu
does. When neither boosting nor quoting is allowed the menu is disabled
(“Boosts and quotes are disabled”). Where the author reviews quotes by hand
(`manual`) it reads “Request to quote”, with “Author will manually review”
under it, and a pending quote says “Post pending” with a “Learn more” that says
why. Posts from the public and hashtag streams carry a policy written for no
one, which the web client drops, as Mastodon's does, and fetches afresh when
the boost menu opens.

A quote of a remote post stays pending, and a `QuoteRequest`, named under the
quoter (`/quote_requests/{uuid}`), goes to the quoted author's own inbox.
Their `Accept` is taken when its `result` is on their host and the quote is
still pending; the quote is accepted with that stamp, and the quoting post is
sent again as an `Update`. A `Reject` revokes an accepted quote and rejects
any other. The answer is found by the request it names, whatever its own id:
hackers.pub names its `Accept` after the request (`{request}#accept`), on our
host, and it is taken like any other, as Mastodon takes it.

A remote post's quote is recorded pending, or `deleted` when the post quotes
a `Tombstone`, and verified as `ActivityPub::VerifyQuoteService` verifies it:
the quoted post is fetched if need be, a self-quote is accepted, and a
`quoteAuthorization` stamp is fetched and accepted only if it is a
`QuoteAuthorization` on its author's host naming both posts. A stamp is fetched
as `fetch_resource` fetches it, with no redirect followed: one that is gone, or
that is anything but a `200` served as ActivityPub naming itself, rejects the
quote, and one whose author is on another host leaves it pending. The posts a
quote or a `QuoteRequest` names are found as `TagManager#uri_to_resource` finds
them: a local one by any URL that shows it (`/@alice/{id}` too), and a remote
one by its URI less any fragment. A stamp is only looked at if it starts with
`http://` or `https://`, in those letters, and is not on the web domain, its
port included, as `unsupported_uri_scheme?` and `TagManager#local_url?` have
it; an `Accept`'s `result` is held to the same scheme test. A stamp inlined as
an object (or anything but a string) is one Mastodon fails on, testing its
scheme: the `Create` fails before the post is created, and an edit before any
of it is kept, each to be tried again. A quote of a local post waits for its
`QuoteRequest`, which is accepted when the requester may quote the post and the
quoting post's quote names it, and answered at the requester's own inbox; a
requester with no inbox, or an author with no key, is not answered, but the
quote is accepted all the same. One whose quoting post cannot be found, or
quotes nothing, fails and is tried again, as Mastodon's raises calling `quote`
on it. An edit that changes the quoted post replaces the quote, and a changed
stamp sends it back to pending. A quote an edit adds is pending, even of a
`Tombstone`, as `update_quote!` creates it with no state; only a replaced one
is `deleted`. A changed quote is not itself a change to the post, as
`update_quote!` runs after Mastodon has decided whether the edit changed
anything: an edit that changes only its quote records no version, and is
neither shown again in timelines nor forwarded.

The `quotes_count` of the quoted post moves as `Quote`'s callbacks move it: up
by one when a quote is created accepted or becomes accepted, down by one when an
accepted quote is destroyed, and down by one whenever a quote that is not
legacy changes to any state but accepted, whatever it was before, never below
nought. So a pending quote that is rejected, or a rejected one sent back to
pending, takes off one it never added, as upstream's code (and its TODO) has
it.


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

The web client offers it, as Mastodon's does, in the menu of someone else's
post that quotes yours (“Remove my post from @…’s post”), after a
confirmation, and says once, on a quote notification, that the menu is
where to find it.

Changing a post's quote policy (`PUT /api/v1/statuses/:id/interaction_policy`)
refreshes it in local timelines, without notifications, and sends it as an
`Update`.


Retries and forwarding
----------------------

A stamp that cannot be fetched for now (no answer, or a status that may change)
leaves the quote pending and is tried again as `RefetchAndVerifyQuoteWorker`
tries it: between 30 seconds and ten minutes later, then up to five more times
on Sidekiq's exponential backoff, the quoting post refreshed in local timelines
if the quote's last change moved its state (`state_previously_changed?`), as an
update that is not an edit is too: a stamp that changed sends an accepted quote
back to pending, and if it is then accepted again, that is a move all the same.
The retries wait in the [job queue](../operating/jobs.md), so a restart does
not lose them.

A quoted post whose server does not answer is not tried again for itself:
`VerifyQuoteService` keeps the error and goes on to the stamp, and raises it
again only if the quoting post (`@quote.status`, not the quoted one) is
missing, which it never is. Without a stamp the quote stays pending; with one,
the stamp is fetched, and retried if it cannot be, as above.

Mastodon 4.7.1 also has a `QuoteRefreshWorker`, to verify a stamp again a
week after it was last checked, but nothing schedules it
(`Quote#schedule_refresh_if_stale!` has no caller), so it has no counterpart
here.

A remote author's `Delete` of a stamp is passed on, as
`ActivityPub::Forwarder` passes it, to the followers of the local accounts
that boosted or quoted the quoting post, and of the local author it replies
to, signed by that author or else by the first of those accounts. The same
forwarder passes on a remote status's `Delete`, and an edit's `Update`, when
the activity carries a Linked Data signature and the status is public or
unlisted. What it passes on goes as `ActivityPub::LowPriorityDeliveryWorker`
sends it: on a delivery queue of its own, Mastodon's `pull`, taken only when
the others have nothing due, and tried nine times rather than seventeen.


How a quote is shown
--------------------

A status's `quote` is rendered as `REST::StatusSerializer#quote` renders it,
for the viewer asking, wherever the status appears: on its own, as the post a
boost carries, in a notification, or in any list of statuses. A legacy quote
that was never accepted is left out. A quote not accepted shows its state
alone; an accepted one is `deleted` when the quoted post is gone, and
otherwise is filtered as `StatusFilter#filter_state_for_quote` has it for the
viewer (`unauthorized`, `blocked_domain`, `blocked_account`, `muted_account`)
and carries the quoted post unless it is `unauthorized`. The post it carries
is rendered whole for the viewer, as any status is: its counts, its author,
its application, and the viewer's flags and matching filters. The quoted post's
own quote is rendered shallow, as `REST::ShallowQuoteSerializer` renders it,
under the same rules for the same viewer: its `quoted_status_id` names the
post it quotes only where the quote would carry that post, and is `null`
otherwise.

Deleting a post answers it with its source, as Mastodon renders it with
`source_requested`, for a redraft: each post in the answer, the quoted one
included, has its `text` in place of `content`, and the quote and its post's
own quote show their posts whether or not they were accepted.

`GET /api/v1/statuses/:id/quotes` lists the posts quoting one with an
accepted quote, newest quote first, less those of accounts the reader blocks,
mutes or is blocked by. The quoted author sees all of them; anyone else, as
`StatusFilter` lets them see each. It pages by the quotes' ids, as Mastodon
does: `next` (by `max_id`) only after a full page, `prev` by `since_id`, and
`min_id` is not read.


What still differs
------------------

 -  A quoted post is sent without its `filtered`, which Mastodon's
    `REST::StatusSerializer` gives it, so the web client's filtering of
    quoted posts has nothing to act on yet.

 -  Statuses are soft-deleted, and their quotes kept: a deleted quoted post is
    shown as `deleted` from its row, where Mastodon nullifies
    `quoted_status_id`.
