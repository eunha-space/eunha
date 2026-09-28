Several instances in one process
================================

Instances may share an S3-compatible media bucket when every one has a stable,
unique object namespace. Set `media_storage.key_prefix` to prepend that
namespace to every object read, write, delete and public URL. Leave it empty
for the historical dedicated-bucket layout:

~~~~ toml
[media_storage]
bucket = "eunha-media"
key_prefix = "tenants/9bd0de00b92141828d4bd2d36222f70c"
base_url = "https://r2.eunha.space"
~~~~

The prefix is an ownership boundary for object layout, not authorization;
bucket credentials can still access other prefixes in the same bucket.

Every eunha process serves a registry of instances and hands each request to
one of them by its `Host` header. Run without arguments, it serves the single
instance in `config.toml` and the environment, as it always has, and answers
whatever host it is asked by. Given a directory, it serves one instance per
`*.toml` in it, each answering to its `instance.domain`:

~~~~
eunha --tenants /srv/eunha/tenants migrate
eunha --tenants /srv/eunha/tenants
~~~~

Each instance keeps its own database, Redis prefix, background tasks and
signing keys; what they share is the process and its routes, built once rather
than per instance. Three things follow from that:

 -  **Tenant files are read on their own.** Environment variables belong to the
    process, so none of them overrides a tenant's file.
 -  **Every tenant must agree on what the process owns:** `bind_address`,
    because there is one listener, and `allowed_private_networks`, because the
    SSRF-guarded resolver is shared. Eunha refuses to start otherwise, and when
    two files claim one domain.
 -  **A tenant that cannot start does not stop the rest.** One whose database is
    behind this binary, or that fails to start, is left out and its host
    answers 503; a host no tenant serves answers 421. A lone instance still
    refuses to start, as it always has.

`eunha --tenants <dir> migrate` migrates every tenant's database, and
`--check` exits non-zero if any of them is behind.

Sharing a process also means sharing its capacity, and two limits keep one
instance from taking more than its share:

 -  **Requests in flight, per instance.** Past `max_concurrent_requests` in
    `[limits]`, an instance's requests are answered at once with 503 and
    `Retry-After` rather than queued, and its neighbours carry on. Unset, a
    lone instance has no limit and one among several has 64. A streaming
    connection counts only while it is being opened.
 -  **Deliveries in flight, per process.** `process_delivery_concurrency` in
    `[workers]`, 256 by default, caps outbound ActivityPub deliveries across
    every instance, first come first served, so one with a large fan-out
    waits its turn instead of opening thousands of connections. Every
    instance in a directory must name the same value.
 -  **Open files, per process.** Every socket counts against the process's
    limit on open files: deliveries in flight, connections kept for reuse,
    database and Redis connections. A service launchd starts has a soft
    limit of 256 unless its plist sets another, which a fan-out passes, so
    eunha raises its own soft limit to the hard one at startup — 92,160 on
    macOS — and warns if that is still under 4,096.

And a process refuses to start with more than it can hold, before any instance
is started:

 -  **At most 50 instances**, or `process_max_tenants` in `[limits]`. Every
    instance in a process goes down with it, so this is how many one crash
    may take — a decision about the failure domain, not merely about density.
 -  **Pools that fit their database server.** Eunha asks each PostgreSQL
    server its instances use how many connections it accepts —
    `max_connections` less the reserved ones — and refuses when their
    `database_pool.max_connections` add up to more. Overrun, that budget
    would fail whichever instance happened to ask last. It sees only its own
    process, so processes sharing a server divide it between them with
    `process_database_connections` in `[limits]`.

Every instance in a directory must name the same value for both, and a lone
instance is held to them too.

Everything an instance does is logged inside a `tenant{domain=…}` span — its
requests, its streaming connections, its background queues and every task any
of them starts — so one process's log can be read one instance at a time. A
lone instance's lines carry it too. A task started with Tokio directly would
begin outside every span, so `clippy.toml` refuses `tokio::spawn` and
`spawn_blocking` in favour of `tenants::spawn` and `tenants::spawn_blocking`,
which carry the span along.

Tenants come and go without a restart. Sent `SIGHUP`, a process serving a
directory rereads it: a new file starts its tenant, a removed one stops its
tenant — whose host then answers 421 — and a changed one restarts it, answering
503 until it is back. The rest serve on untouched. A stopped tenant's
background queues finish the batch they are in, for up to 20 seconds, and its
streaming connections are closed so that clients reconnect.

~~~~
kill -HUP <pid>
~~~~

A reload is all or nothing. It is refused, and the running tenants left as they
were, when the directory could not have been started as it stands — a domain
served twice, too many tenants, pools past their budget, no tenants at all — or
when it would change what the process set up when it started: `bind_address`,
`allowed_private_networks` and `process_delivery_concurrency` take a restart. A
tenant that fails to start does not fail the reload; its host answers 503, and
the next `SIGHUP` tries it again.

This is the start of the shared-process work planned in
the [multitenancy plan](../design/multitenancy.md); what sharing a process
saves is measured in [benchmarking](../design/benchmarking.md).


Answering more than one hostname
--------------------------------

An instance may answer additional HTTP hostnames without changing its canonical
ActivityPub identity by listing `aliases` under `[instance]`. Handlers continue
to emit URLs and account identities using `instance.domain`; aliases only affect
the shared runtime's initial Host dispatch.

~~~~ toml
[instance]
domain = "garden.eunha.space"
aliases = ["garden.eunha.site"]
~~~~
