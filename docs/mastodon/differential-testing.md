Differential testing against a live Mastodon
============================================

The entity check compares eunha to what upstream's serializers *say*. This one
asks upstream directly: the same request goes to both servers and the responses
are compared.

~~~~
scripts/differential_test.sh                              # its own eunha
scripts/differential_test.sh http://localhost:3001 TOKEN  # one you run
~~~~

Given no arguments it brings up an eunha of its own — scratch database,
migrations, two accounts and a token — and tears it down afterwards. That form
exists so CI and a developer run the same path: the eunha-side setup used to
live in whoever had last run it, which is most of why this went weeks without
being run at all. It needs `target/release/eunha` built, as
`federation_test.sh` does.

That brings up Mastodon in Docker — the official image, because building it from
source on macOS means libidn, OpenSSL headers for `hiredis-client`, libvips, and
a `pg` gem that segfaults against Postgres 18, all of which the image has
already solved — seeds both servers, and compares what a client actually does:
31 reads, nine writes, the interaction verbs (favourite, boost, bookmark, pin,
follow, block, mute, and their undos), and about two hundred steps of
[flows](#flows) — domain blocks, reports and moderation, moves, and the
everyday client endpoints — each a sequence in which one request sets up what
the next reads. It is Mastodon 4.7.2, the release eunha tracks, and the whole
comparison takes about a minute once the image is pulled.

The stack runs Sidekiq as well as the web process. Without a worker nothing
Mastodon defers ever happens, and some of that shows in the API — a home feed
stays `regenerating?` and answers 206 forever — which reads as a difference from
eunha when it is a missing worker. An unfaithful reference invents findings.

It invented ten. Sidekiq boots as soon as Postgres answers, but the schema is
created by the web process's `db:prepare`, so on a cold database the worker
started first, died on `relation "users" does not exist`, and compose did not
bring it back. `unfavourite` and `unreblog` hand the removal to a worker and
force the flag false in *their own* response, so each undo looked right while
the row survived — and every later request read that row and reported
`favourited: true` on a status that had just been unfavourited. Nine
`favourited` differences and one `reblogged`, all recorded against eunha, all of
them a dead worker. The worker now restarts until the schema exists, and the
harness asks Redis whether one has registered before it compares anything,
rather than trusting that a container was started.

Each do/undo pair also acts on a status of its own now. Sharing one across all
ten verbs meant one pair's deferred work was visible to the next, and `unreblog`
opens a window Mastodon disagrees with itself in: `Status.reblogs_map` is
`unscoped` and counts the discarded reblog, while `Account#reblogged?` goes
through `default_scope { recent.kept }` and does not, so two endpoints answer
differently about the same status depending on whether the controller passes a
relationships presenter. A pair per status leaves nothing to leak.

Nothing in it encodes what the answer should be, which is the point: a rule
misread while writing a test would be misread in the test too. It found seven
differences the source-reading had missed, all of them fields nested inside
objects the entity extraction never descended into.

It compares values on writes and interactions, where both servers act on the
same input so a count or a flag that differs is a real difference — that is
where `poll.voted` was `false` for a poll's own author, `noindex` told every
account to hide from search engines, and a status came back with no language at
all. Identifiers, hostnames, timestamps and totals over an instance's whole
history are excluded, because two servers cannot agree on those however
identical the request, and comparing them buries everything else.

For reads it compares values only under `configuration.*` — the limits an
instance states about itself, where two servers genuinely should agree. That
came second, after a shape-only comparison passed `max_display_name_length: 30`
against Mastodon's 40, both being integers. Comparing values found the media
description limit still advertised at Mastodon's older 1500 rather than 10,000.

Everything else stays shape-only on purpose. `followers_count`, ids and
timestamps depend on each instance's data, and comparing them would bury real
findings under differences that mean nothing.

It runs in CI, as its own job, for the reason everything else here does: the two
ways it broke — a reference with no worker, and a compose file another harness
had edited out from under it — were both invisible to anyone not running it.

One comparison is built rather than observed. **Notification grouping** is the
part of that API which is not a straight translation of a row: Mastodon
collapses notifications into groups, and a client renders “X and 2 others
favourited your post” out of a group's `notifications_count` and
`sample_account_ids`. A server that groups differently shows a different
sentence with every field present and of the right type, so shape cannot see it
— and neither can one account, because a group of one is a group on any server.
Three further accounts favourite the same status and follow the same account,
and the groups are compared. Account ids cannot match between two servers, so
the samples are compared by *who* they name: each fan is known by the position
it acts in, and a group naming `[fan3, fan2, fan1]` here has to name
`[fan3, fan2, fan1]` there. It agrees, on both the count and the order.

Getting there needed the fixture reset on both sides, and the second reset is
the one worth remembering: eunha gets a scratch database every run while the
Mastodon container is left up between them, so clearing the notifications is not
enough. A repeat follow produces no notification at all, so on a second run only
the server with a fresh database reports a follow group — which reads exactly
like eunha inventing one. The fans unfollow before they follow.

The groups are also read only once every act has become a notification.
Mastodon writes them from `LocalNotificationWorker`, after the favourite or
follow has already answered, while eunha writes them in the request — so reading
at once caught Mastodon with the third fan's follow still queued, a group of two
against eunha's three, recorded as eunha's difference.


Flows
-----

A single request says little about a domain block. What matters is that the
follower on that domain is gone afterwards, the notification it caused with it,
and that a `severed_relationships` notification says so — three requests later,
after a worker. *scripts/differential\_flows.py* drives sequences like that
through both servers and compares every step, so a difference is reported at
the step that produced it:

 -  **Domain blocks a member makes**: blocking a domain, the list, the
    relationship to an account there, the notification from it, the severed
    relationships notification, following an account there (403), unblocking,
    and a blank domain (422).
 -  **Domain blocks the moderators make**: silence, suspend and noop, with
    `reject_media`, `reject_reports` and `obfuscate`; a duplicate and a
    subdomain that is no stricter (422, with the existing block); what each
    severity does to an account on the domain, as a moderator and as a member
    see it; the follow a suspension severs; and removing it.
 -  **Reports, moderation and warnings**: a report with posts, a rule and a
    category; the admin report API — show, assign, unassign, recategorise,
    resolve, reopen; each account action (`none` with a report, `sensitive`,
    `silence`, `disable`, `suspend`) and its undo; the `moderation_warning`
    notifications, only for the actions that asked to notify; and what a
    disabled or suspended login is answered. Appeals are not here: Mastodon
    takes them through its web interface and has no API for them.
 -  **Moves**: a moved account's `moved`, and that it cannot be followed.
    Moving is a settings form in Mastodon, so the seed moves the account; the
    federation harness moves accounts for real.
 -  **Everyday client endpoints**: filters v1 and v2 with keywords and
    statuses, lists and their members, followed and featured tags, edits with
    their history and source, a thread's context, scheduled statuses, polls,
    conversations, markers, account lookups, notes, endorsements, the profile,
    the notification policy and requests, push subscriptions, app
    registration, media upload and description, the instance's pages, and the
    admin email, address and domain-allow blocks.

Ids differ between the servers, so each is given a fixture naming its seeded
accounts and rules, and a value a step makes is saved under a name the next
step uses; where an id *is* the answer — the posts a report names, the rule it
cites — it is compared by that name. Run one flow, and see each step's status
codes, with `DIFFERENTIAL_ARGS="--flow=reports --verbose"`.

Getting a flow to compare what it claims to took more than the flow:

 -  **The Mastodon container outlives the run.** eunha's database is new each
    time; Mastodon's is not. *scripts/differential\_seed.rb* puts back what the
    flows change — blocks, reports, strikes, the troll's suspension, filters,
    lists, the profile — or the second run compares eunha against the first.
    The profile was the one that showed it: `update_credentials` sets a note,
    and on the next run every status's `account.note` differed.
 -  **Rate limits answer 429 to both, which is agreement.** A token gets 300
    requests in five minutes, and the flows make more than that between them;
    once over, both servers answered 429 to everything, and a run with half its
    steps throttled reported no differences. Each flow acts through a token of
    its own, and the counts an earlier run left — Mastodon's in its cache,
    eunha's in the harness's Redis database — are cleared before comparing.
    `--verbose` shows what every step was answered, which is how this was seen.
 -  **Explicit ids leave a sequence behind.** The seed gives accounts and
    tokens fixed ids, and the first follow or app made through the API then
    collided with one: a 500 that was the harness's own.
 -  **A worker's first effect is not its last.** A moderator's domain
    suspension suspends the accounts and only afterwards clears the silence it
    replaced, so a read between the two saw Mastodon silenced and suspended at
    once. A step that waits for a worker waits for its effect and then until
    the answer stops changing.
 -  **Mastodon answers in no order.** `GET /api/v1/statuses?id[]=` and its
    account twin are not sorted, so they are compared by which ids they hold.
 -  **eunha keeps media in S3.** *scripts/differential\_fake\_s3.py* is a bucket
    in memory for it, so the media endpoints answer as they would.

Two differences are decisions rather than bugs, and the flows leave them out:
a report in a category that does not exist, and a push key that is the point
at infinity, are each a 500 from Mastodon and a 422 from eunha
(`report-unknown-category-rejected`,
`push-subscription-unusable-key-rejected`).

What they found, fixed in eunha: a remote account's entity carried `roles`; a
report's posts were serialized for nobody rather than for the moderator reading
them; a member's domain block took a blank domain; adding an account to a list
twice succeeded; a keyword made without `whole_word` defaulted to false, and an
update without it reset it; an edit's history carried `poll: null`; a deleted
status came back without its viewer's fields and with `content` beside `text`;
a scheduled status's `params` lacked `scheduled_at`, `idempotency` and
`with_rate_limit`; the profile had `username` and lacked its pictures'
descriptions and tab settings, and `PUT` ignored what it was sent; a push
subscription's `id` was a string; a single status left out the viewer's
matching filters; and an account read by id showed `feature_approval` as if
nobody were asking.
