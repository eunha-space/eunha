eunha
=====

Rust re-implementation of Mastodon, with drop-in database compatibility.

Eunha aims for 100% Mastodon database schema compatibility, so that Eunha can
be a drop-in replacement on top of your existing Mastodon database.

We track the latest Mastodon release, and provide migration path from old Eunha
database schema to updated Mastodon database schema.

It's not Eunha's goal to completely mimic Mastodon's feature set or its
implementation detail, and Eunha may contain behavioral differences.


Running eunha
-------------

 -  [Container images](./operating/containers.md): GitHub Actions builds,
    image tags and running the image.
 -  [The first account](./operating/first-account.md): creating an instance's
    owner from the command line, as `tootctl accounts create` does.
 -  [Migrations](./operating/migrations.md): `eunha migrate`, and why starting
    the server does not apply them.
 -  [Shared Redis](./operating/redis.md): key prefixes, ACLs and a separate
    coordination pool for deployments that share Redis.
 -  [Several instances in one process](./operating/instances.md): serving a
    directory of tenants, the limits that keep them apart, and reloading on
    `SIGHUP`.
 -  [Invites](./operating/invites.md): who may invite, and handing invites out
    to members.
 -  [Update notices](./operating/update-notices.md): the optional update check.


Mastodon compatibility
----------------------

 -  [Tracking Mastodon](./mastodon/tracking.md): the schema check, and adopting
    a new Mastodon release.
 -  [Signing keys](./mastodon/signing-keys.md) and
    [HTTP signatures](./mastodon/http-signatures.md).
 -  [API entity parity](./mastodon/entity-parity.md),
    [differential testing](./mastodon/differential-testing.md) and
    [federating with a live Mastodon](./mastodon/federation-testing.md): the
    harnesses that check eunha against upstream.
 -  [Deliberate divergences](./mastodon/divergences.md) from Mastodon, and what
    is [outstanding from 4.7.1](./mastodon/4.7.1.md).


Design
------

 -  [Protocol extension](./design/protocol.md): where ActivityPub scales badly
    and what eunha designs for it.
 -  [Hosted tenancy plan](./design/multitenancy.md): the plan for hosting many
    instances.
 -  [Benchmarking](./design/benchmarking.md): what a small instance costs, and
    what sharing a process saves.
 -  [Contributing](./contributing/index.md) and
    [where to pick this up](./contributing/next.md).
