Where to pick this up
=====================

State at handoff: `main` is deployed to seoul.earth and healthy. Schema matches
Mastodon 4.7.0 across 974 constraints. 856 integration tests, 70 unit tests, CI
green on eight gates — both live-Mastodon harnesses are among them now, and
run clean against Mastodon 4.7.0. 857 integration tests. Verified by looking at
the run, not by assuming: CI had in fact been red for at least two commits. The
federation harness passes 24/24 from a clean checkout, both servers inside the
container network.

Three harnesses verify parity, described in
[Mastodon compatibility](../mastodon/tracking.md) and in each script's header.
Read those headers before believing a failure — most of what they report first
is the harness or the reference, not eunha.


What is unfinished
------------------

**Nothing the differential harness reports.** It runs clean: 56 endpoints, no
differences. The nine `favourited` findings were the harness, not eunha and not
the fixture — its Mastodon had no Sidekiq, so the rows `unfavourite` and
`unreblog` hand to a worker were never deleted and every later request read them
back. See [differential testing](../mastodon/differential-testing.md); the
harness now refuses to compare without a live worker.

**`/api/v1/timelines/home` answering 206 on one side and 200 on the other** is
fixed. The first was the same dead worker — a feed with nothing to regenerate
it stays `regenerating?` forever. The second, eunha 206 and Mastodon 200, was
eunha's own: it regenerated any feed without an eunha-only `:populated` marker
the first time it was read, so a new user's empty feed read as rebuilding. The
feeds are now Mastodon's model throughout — no markers, nothing built on read,
regenerated only for a returning user or held partial by a first follow — so
both sides answer 200 for a new user.

**The 503 on `/ap/users/:id/collections/featured` is fixed.** It was a missing
route, not an error: under the numeric AP-ID scheme only `followers` and
`following` had been mirrored, so the two collection URLs an actor advertises
had no handler and fell through to the SPA — an HTML page where the frontend is
built, a 503 where it is not. A regression test fetches exactly what the actor
says it has.

**Notification grouping agrees with Mastodon.** Compared end to end against a
live 4.7.0 for the same fixture — three accounts favouriting one status and
following one account — and both a group's `notifications_count` and the
identity *and order* of its `sample_account_ids` match. See
[differential testing](../mastodon/differential-testing.md).

**`sharedInbox` works in both directions.** eunha delivers to
`https://mastodon.test/inbox`, and Mastodon delivers to eunha's `/inbox` — four
times in a run. The earlier note here, that Mastodon uses the per-actor inbox
when a single recipient is on the far side, was wrong: it prefers the shared
inbox for distributing a status whenever the peer advertises one, and keeps the
personal inbox for directed activities like Follow and Accept. Measuring it is
what showed that.

What a single follower could not test is the point of a shared inbox — one
delivery, addressed to nobody in particular, fanned out by the *receiving* side
to every local account it concerns. A second eunha account now follows the same
remote actor, and both see the status.


Where bugs have actually been
-----------------------------

Thirty were found in the work leading here, and they clustered:

 -  **Guard clauses.** `return if direct_visibility?`,
    `if in_reply_to_id.present? && distributable?`, `return unless accepted?`.
    Unconditional logic was nearly always already right; the conditions are
    what a reimplementation drops. When reading upstream for a rule, read the
    first three lines of the method.
 -  **The ActivityPub ingest paths.** eunha's own API maintained counters that
    its inbox handlers did not — six bugs of one shape, because Mastodon gets
    this from model callbacks that fire however a record came to exist.
    `src/counters.rs` now owns those rules; keep new paths calling it rather
    than writing their own.
 -  **Sub-resources of the second URI scheme.** A local actor is served under
    `/users/{username}` or `/ap/users/{id}`, and every sub-resource is the actor
    URI with a suffix. The numeric block mirrored `followers` and `following`
    and stopped, while a comment above it said it mirrored the lot — so
    `featured` and `collections` were advertised at URLs with no handler. When a
    scheme is added, the question is which suffixes it owes, and the answer is
    all of them.
 -  **Values that are the right type.** `voted` false where Mastodon says true,
    `noindex` derived from an unrelated column, a limit advertised as 1500
    instead of 10000. A shape comparison passes all of these.


Things worth doing
------------------

1.  **Run the harnesses against the deployed build**, not a local one. Nothing
    has ever checked that production matches what the tests claim. Read the
    warning below first: the differential harness *writes*.
2.  **Fan a shared-inbox delivery out to more than two accounts.** Two proves
    the mechanism; it does not prove the addressing scales, or that a follower
    who should *not* receive something is left out. A private status delivered
    to a shared inbox is the interesting case.
3.  **Group more than three accounts.** `SAMPLE_ACCOUNTS_SIZE` is 8, and the
    grouping fixture uses three — so the sample being *capped* at eight, and
    what a group of twenty reports, is still untested.
4.  **Decide whether the federation gate belongs on every push.** It is in CI
    now, and it is the slowest gate by some way: a release build of eunha
    inside Docker before Mastodon has even booted. If it starts hurting, a
    schedule is the alternative, or caching the image layers.


How not to be fooled
--------------------

Nine failures in this work were the tooling rather than eunha, and each cost
real time:

 -  A commit shipped tests without the definitions they needed, because a
    keyword heuristic quietly reverted work while separating it from `cargo fmt`
    churn. Run the suite *after* cleaning up, not before.
 -  The federation harness passed by hand for hours and failed from a clean
    checkout, because a scratch directory had accumulated fixes the repository
    never received. Tooling is not real until it runs from `git clone`.
 -  The same thing then happened in reverse, and is what hid the ten findings'
    real cause. The
    federation work grew TLS proxies onto the compose file the *differential*
    harness shared, mounting certificates generated into that scratch directory
    — so `differential_test.sh`, which runs its compose file in place, could no
    longer bring the stack up at all. The two now have a compose file each. A
    harness that another harness can break by editing is one harness.
 -  Ten differences were attributed to eunha for weeks because the reference was
    broken in a way that only ever made *eunha* look wrong. Before believing a
    finding, check that the thing you are comparing against is actually running.
 -  A deploy reported success having shipped nothing, because the push went to a
    branch that did not have the commits. Check the deployed *behaviour*, not
    the deploy's exit code — the 0.81s build was the tell.
 -  The federation harness had never worked from a clean checkout either, and
    for a reason no amount of re-reading would have found: eunha ran on the
    host, `mastodon.test` is a container-network alias, and a host process
    cannot resolve one. eunha could not fetch an actor's public key, so it
    rejected every inbound activity with a 401 — which reads as a signature bug.
    A commit message said it passed every check; what passed was a laptop with
    state nobody wrote down. Both servers live in the network now.
 -  **`grep -q` under `set -o pipefail` reports failure when it succeeds.** It
    exits on the first match and SIGPIPEs whatever feeds it, so the pipeline's
    status is the writer's death, not the match. A new check called itself a
    failure while four of the deliveries it was looking for sat in the log —
    and the obvious reading was that the feature was broken. `grep -c` and
    compare the number.
 -  **Mastodon rate-limits, and a throttled reference invents findings.** Three
    runs back to back against the same container answered 429, and the run
    before them reported a follow group of 3 against 2 — which looked like a
    real grouping difference and was a rate limit eating one follow. If a run
    disagrees in a way that looks almost right, `down -v` the stack and run it
    once before believing anything.
 -  This file claimed six green CI gates while CI had been red on `main` for at
    least two commits, because nobody had opened the Actions tab. The failures
    were *stacked*: CI ran `postgres:16`, which cannot execute
    `001_initial.sql` — its `pg_dump` preamble sets `transaction_timeout`, a
    parameter Postgres 17 introduced — so the first step died and hid a clippy
    error behind it. Development and production both run 18; CI was the only
    Postgres anywhere that was not. A red first gate means every gate after it
    has told you nothing.

The general form: a passing check is not evidence until you have seen it fail.
Disable the fix and confirm the test notices.
