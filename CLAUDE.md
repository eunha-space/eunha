Eunha
=====

Rust re-implementation of Mastodon. Eunha aims for 100% Mastodon database
schema compatibility, so that it can be a drop-in replacement on top of an
existing Mastodon database. It tracks the latest Mastodon release; behavioural
differences are allowed, but each one is either a bug or a recorded decision.

The documentation is in *docs/*, a VitePress site published at
<https://eunha.social/docs/>. Read the page for the area you are working in
before changing it, and update it in the same commit when behaviour changes.


Rules
-----

 -  All Mastodon tables go in the `public` schema; tables only eunha needs go
    in the `eunha` schema.
 -  Use mise for all tasks. See *mise.toml*.
 -  Use the [shadcn/ui] CLI when adding frontend components. Don't hand-roll
    components.
 -  For all federation work, use [ojak], and extend it when necessary.
 -  Start tasks with `tenants::spawn` and `tenants::spawn_blocking`, never
    `tokio::spawn` or `spawn_blocking` directly, so the task keeps its
    `tenant{domain=…}` span. *clippy.toml* enforces this.
 -  Every Redis key eunha owns goes through the configured key prefix. A new
    Redis command must be added to the ACL list in
    *docs/operating/redis.md*, because tenant users are granted only those.
 -  Migrations are applied by `eunha migrate`, never by starting the server.
 -  A deliberate difference from Mastodon gets an entry in *divergences.toml*,
    which `cargo test` checks.
 -  Run `mise run fmt:check` before committing. It covers Rust and every
    Markdown file, *docs/* included.
 -  Run `mise run docs:build` after editing *docs/*; it fails on dead links.

[shadcn/ui]: https://ui.shadcn.com
[ojak]: https://github.com/eunha-space/ojak


Where things are documented
---------------------------

Running an instance:

 -  *docs/operating/first-account.md*: `eunha accounts create`.
 -  *docs/operating/migrations.md*: `eunha migrate`, the startup check, and
    `eunha rehearse-migration`.
 -  *docs/operating/importing.md*: `eunha import-mastodon` and
    `eunha import-media`, and what each of them refuses.
 -  *docs/operating/redis.md*: key prefixes, ACLs, the coordination pool.
 -  *docs/operating/instances.md*: several instances in one process, the
    tenants directory, admission limits, `SIGHUP` reload, host aliases, and
    shared media buckets.
 -  *docs/operating/jobs.md*: the job queue, its workers, retries, unique
    locks and the dead set.
 -  *docs/operating/invites.md*: the everyone role and handing out invites.
 -  *docs/operating/moderation.md*: permissions, account actions and strikes,
    the audit log, and reports.
 -  *docs/operating/search.md*: search with and without Elasticsearch, the
    `[instance.elasticsearch]` settings, indexing, and `eunha search deploy`.
 -  *docs/operating/administration.md*: the server settings and what they
    drive, content retention, and the rest of the server administration.
 -  *docs/operating/email-subscriptions.md*: email subscriptions, their
    switches, distribution, and the admin API.
 -  *docs/operating/account-moves.md*: aliases, moving an account, redirects,
    and inbound `Move`s.
 -  *docs/operating/import-export.md*: a member's CSV exports, data imports
    and their queue, and the archive takeout (not `eunha import-mastodon`).
 -  *docs/operating/preview-cards.md*: link preview cards, how they are
    fetched, their images and authors.
 -  *docs/operating/remote-replies.md*: fetching remote threads' replies,
    and async refreshes (`Mastodon-Async-Refresh`).
 -  *docs/operating/terms-of-service.md*: terms of service versions, their
    admin API and notification, and the privacy policy.
 -  *docs/operating/accounts.md*: two-factor authentication and security
    keys, the sign-in steps after the password, and sessions.
 -  *docs/operating/translation.md*: DeepL and LibreTranslate, and what
    `POST /api/v1/statuses/:id/translate` translates.
 -  *docs/operating/update-notices.md*: the optional update check.
 -  *docs/operating/fasp.md*: Fediverse Auxiliary Service Providers, behind
    `experimental_features = ["fasp"]`.

Tracking Mastodon:

 -  *docs/mastodon/tracking.md*: `mastodon.toml`, `eunha-schema`, the schema
    check, and the steps for adopting a new Mastodon release.
 -  *docs/mastodon/signing-keys.md* and *docs/mastodon/http-signatures.md*.
 -  *docs/mastodon/streaming.md*: the streaming API, its channels, and what
    is filtered for each connection.
 -  *docs/mastodon/remote-actors.md*: how a remote actor's document becomes
    its account row (`ProcessAccountService`).
 -  *docs/mastodon/serving.md*: what other servers fetch: the actor, a
    post's replies, likes, shares and thread, an account's featured posts,
    hashtags and collections, featured hashtag federation, caching, and
    followers synchronization (FEP-8fcf).
 -  *docs/mastodon/quotes.md*: a quote's states, the consent handshake,
    verifying stamps, and revoking.
 -  *docs/mastodon/inbound-statuses.md*: an inbound status's audience,
    silent mentions and limited visibility, conversations, and what a
    `Delete` or `Undo` may take back.
 -  *docs/mastodon/entity-parity.md*, *docs/mastodon/differential-testing.md*
    and *docs/mastodon/federation-testing.md*: the harnesses that compare
    eunha with upstream, and the environment traps that fake failures.
 -  *docs/mastodon/divergences.md*: how *divergences.toml* works.
 -  *docs/mastodon/4.7.1.md*: what the tracked release changed.

Design records and handoff notes:

 -  *docs/design/protocol.md*: the ActivityPub extension eunha designs.
 -  *docs/design/multitenancy.md*: the hosted tenancy plan.
 -  *docs/design/benchmarking.md*: measurements.
 -  *docs/contributing/next.md*: where to pick the work up.
