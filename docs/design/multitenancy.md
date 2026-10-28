Hosted Eunha tenancy plan
=========================

Decision
--------

Eunha hosting should initially keep one Eunha process and one
Mastodon-compatible PostgreSQL database per tenant. The hosting platform may
share physical PostgreSQL and Redis servers, but it must preserve explicit
tenant boundaries.

The first density improvement should be unloading dormant Eunha processes and
starting them on demand. A multi-instance Eunha runtime may be considered
later, but it should still route every tenant to a separate database and Redis
namespace. Adding a `tenant_id` column to Mastodon tables is out of scope.

This ordering preserves Eunha's database-compatibility promise, keeps tenant
exports straightforward, and avoids making every application query part of a
security boundary before measurements show that such complexity is necessary.


Current measurements
--------------------

The ten-user benchmark in [benchmarking](./benchmarking.md) measured an
Eunha release process at approximately:

 -  33 MiB RSS when idle;
 -  44–67 MiB RSS under the tested API workloads;
 -  two useful PostgreSQL connections for a small tenant;
 -  no meaningful throughput improvement between three and five connections.

The density benchmark in the same file, which runs tenants side by side on one
host, corrected what those figures implied:

 -  RSS overstates a tenant: it counts the shared binary once per process. The
    real cost is 14–18 MiB for a fresh process and about 40 MiB once it has
    served load, because the allocator keeps what a burst grew.
 -  The zero-minimum pool never reached zero. Queue loops polled every
    half-second, so no connection stayed idle long enough to close; they now
    sleep until work is enqueued, which cut idle transactions from 3.1/s to
    0.2/s per tenant.
 -  A PostgreSQL connection that has served the query mix costs 19–24 MiB
    privately. Connections are a memory budget, not only a slot budget: two warm
    connections cost a tenant as much as its Eunha process.
 -  Idle CPU is negligible once the queues are quiet — under 6% of one core for
    300 tenants, Eunha and PostgreSQL together.
 -  With the timed tasks sleeping until something is due, an idle tenant holds
    a third of a connection on average rather than one and a third.
 -  A prototype running many tenants in one process cut Eunha's memory to about
    3 MiB per tenant, from 12–16 MiB as separate processes, with twelve threads
    for the whole process and no slowdown from a saturated neighbour.

Resident memory, not CPU, therefore decides how many small tenants a host
holds. One hundred tenants that have each served load and hold two warm
connections cost roughly 4 GiB of Eunha and 4 GiB of PostgreSQL backends,
before shared buffers, Redis and the operating system. With the hosting profile
the PostgreSQL share falls to about 0.2 GiB, and in one shared process Eunha's
would fall to about 0.3 GiB.


Isolation model
---------------

Every hosted tenant retains:

 -  a separate Mastodon-compatible PostgreSQL database;
 -  a distinct database role with access only to that database;
 -  a unique Redis key prefix and tenant-specific Redis credentials;
 -  tenant-scoped signing keys, secrets and instance configuration;
 -  an independent migration, backup, restore, export and deletion lifecycle;
 -  a path to dedicated compute, PostgreSQL or Redis when it becomes large.

Shared infrastructure must never turn a namespace into an authorization
boundary. Redis ACLs enforce the configured key prefix. PostgreSQL roles enforce
database access. The routing layer selects a tenant before any application
authentication or data access occurs.


Target hosting architecture
---------------------------

~~~~ text
Cloudflare custom hostname
          |
          v
Always-on tenant gateway
          |
          +-- running tenant --> Eunha process or runtime shard
          |
          +-- dormant tenant --> single-flight start --> health check --> proxy
                                      |
                                      +--> PostgreSQL database
                                      +--> Redis tenant namespace
                                      +--> shared media storage

Shared scheduler and durable workers
          |
          +--> enqueue or process tenant work
          +--> wake a tenant when application execution is required
~~~~

Cloudflare for SaaS terminates custom hostnames and sends all tenant traffic to
the always-on gateway. The gateway resolves the hostname using control-plane
state and never accepts a tenant identifier supplied only by an untrusted
header or request body.


Phase 1: isolated active processes
----------------------------------

Run one Eunha process per active tenant. For a small instance, begin with:

~~~~ toml
[database_pool]
max_connections = 2
min_connections = 0
acquire_timeout_seconds = 5
idle_timeout_seconds = 20

[workers]
queue_idle_poll_seconds = 300
~~~~

and `TOKIO_WORKER_THREADS=2` in the process environment.

The zero minimum allows a quiet process to release its PostgreSQL connections
without stopping. Its queue loops and timed tasks wake about once per
`queue_idle_poll_seconds`, so an idle tenant holds a third of a connection on
average, and closing them that often is also what keeps each one from growing
to 19–24 MiB of cached state. Tenants started together wake together, so a host
that restarts every tenant at once sees their whole pools open at the same
moment. Promote a demonstrably busy tenant to three connections.
Larger pools should follow measurements rather than plan names or member count.

Past a few hundred tenants a host runs out of PostgreSQL backends before it runs
out of anything else, because a backend is a process and costs 19–24 MiB. A
transaction pooler in front of PostgreSQL unties the two: a tenant holds a
backend only while a transaction is in flight, so the backend count follows
concurrent work rather than tenant count. Point the pool at it and leave
`database_url` alone:

~~~~ toml
database_url = "postgres://tenant:secret@127.0.0.1:5432/tenant"
pooled_database_url = "postgres://tenant:secret@127.0.0.1:6432/tenant"
pooled_client_slots = 10000
~~~~

`eunha migrate` keeps using `database_url`, because sqlx holds a session-scoped
advisory lock across a migration run and a transaction pooler would hand the
unlock to a different server connection than the lock. `pooled_client_slots` is
what the pooler accepts in total, and startup checks the tenants' pools against
it rather than asking PostgreSQL for a `max_connections` that no longer
describes the limit. Schema resolution has to move to the role as well —
`ALTER ROLE ... SET search_path TO eunha, public` — since a pooled connection is
handed to clients that never ran the `SET` the pool issues on connect.

The control plane records at least:

 -  desired and observed process state;
 -  runtime host and process identifier;
 -  last request, streaming connection and background-work activity;
 -  database and Redis placement;
 -  migration version and health status;
 -  resource tier and enforced limits;
 -  wake, crash and restart history.


Phase 2: dormancy and on-demand startup
---------------------------------------

Introduce three process states:

 -  `running`: normal requests and streaming connections are accepted;
 -  `draining`: new streaming connections are refused while current requests and
    background work finish;
 -  `dormant`: no tenant Eunha process is resident; the gateway wakes one when
    necessary.

A tenant is eligible for dormancy only when it has:

 -  no active HTTP requests;
 -  no streaming or WebSocket connections;
 -  no pending in-process work;
 -  no durable work requiring immediate application execution;
 -  no scheduled task due within the configured sleep window;
 -  no migration, backup, restore, provisioning or domain operation in progress.

Start with a conservative idle interval, such as 30–60 minutes. Keep recently
active and higher-tier tenants warm. Allow trial, low-cost or long-dormant
tenants to scale to zero. Dedicated and large instances remain always on.

### Wake behavior

The first request for a dormant tenant performs a single-flight wake:

1.  Resolve and authorize the hostname.
2.  Acquire a distributed tenant-start lock.
3.  Start the assigned Eunha process.
4.  Wait for its health endpoint and migration check.
5.  Release all requests waiting on that same start operation.
6.  Record startup duration and outcome.

Concurrent requests must never create duplicate processes. Wake attempts need a
deadline, bounded request buffering and a clear retryable error when startup
fails.

The gateway may serve carefully selected cached discovery responses while a
tenant sleeps, but cached data must not conceal suspension, deletion or key
rotation. Dynamic APIs and federation inboxes wake the tenant.

Federation request bodies must be bounded and either held until a fast startup
completes or written durably before acknowledgement. The gateway must not
return success for an activity that can be lost.

### Preventing wake abuse

Public actors, WebFinger and inboxes must remain reachable, so authentication
cannot be required before every wake. Mitigations include:

 -  caching safe public discovery documents at the gateway;
 -  rate limiting by hostname, route and source reputation;
 -  coalescing every concurrent wake into one operation;
 -  applying a minimum warm interval after startup;
 -  tracking repeated wake-without-use patterns;
 -  placing abusive or unusually busy tenants on dedicated capacity.


Phase 3: shared durable workers
-------------------------------

Today, work tied to an Eunha process cannot run while that process is dormant.
Before aggressive scale-to-zero, move scheduling and durable job ownership out
of tenant processes.

The shared worker system should:

 -  persist every job before acknowledging it;
 -  carry an immutable tenant identity with every job;
 -  resolve tenant credentials from trusted control-plane state;
 -  apply per-tenant concurrency and rate limits;
 -  provide fair scheduling so one tenant cannot starve others;
 -  retry with bounded backoff and expose dead-letter state;
 -  wake an Eunha process only for work that cannot execute in the worker;
 -  record queue latency and execution resource usage by tenant.

Migration jobs bypass transaction poolers and connect directly to PostgreSQL.
They run under an explicit tenant lock and never run as part of ordinary process
startup.


Phase 4: optional multi-instance runtime shards
-----------------------------------------------

A throwaway prototype has measured what sharing would save
([benchmarking](./benchmarking.md), “One process for many tenants”). With the
same tenants and load, Eunha needed about 3 MiB per tenant in one process
against 12–16 MiB as separate processes, twelve threads instead of four per
tenant, a seventh of the startup time and no more CPU, and a saturated tenant
did not slow the rest. The savings are real and large. What remains to justify
building it is the engineering below, and PostgreSQL, which became the limit at
600 tenants.

A runtime shard may host a bounded group of small tenants:

~~~~ text
Eunha runtime shard
  +-- tenant A --> database A --> Redis namespace A
  +-- tenant B --> database B --> Redis namespace B
  +-- tenant C --> database C --> Redis namespace C
~~~~

It must not place multiple tenants in the same Mastodon tables. Each request,
background task, cache entry, metric and outbound operation carries a resolved
tenant context that cannot be replaced by user input.

Begin with a small blast radius, such as 20–50 tenants per shard. Run several
shards per machine. Automatically move noisy or high-value tenants to dedicated
processes.

### Required engineering controls

Before production use, a shared runtime needs:

 -  hostname-to-tenant routing before authentication and database access;
 -  tenant-scoped configuration with no mutable global current tenant;
 -  separate, bounded database pools per tenant or a safe shared pooler;
 -  tenant-scoped Redis clients and key construction;
 -  tenant-scoped caches, rate limits, metrics and tracing fields;
 -  per-tenant CPU, request and background-work fairness;
 -  bounded HTTP connection pools and outbound federation concurrency;
 -  tests that deliberately attempt cross-tenant reads and writes;
 -  shard draining, tenant movement and rollback without DNS changes;
 -  failure injection proving that one tenant cannot corrupt another;
 -  memory-growth detection and shard-level admission limits.

A panic, leak or saturated runtime still affects the whole shard. The shard size
is therefore an explicit failure-domain decision, not merely a density setting.


Rejected design: shared Mastodon tables
---------------------------------------

Do not add `tenant_id` to Mastodon tables or combine tenants in one modified
Mastodon schema. That design would:

 -  break direct Mastodon schema compatibility;
 -  require tenant predicates in every query and unique constraint;
 -  turn one missed predicate into a cross-tenant disclosure;
 -  complicate every upstream schema migration;
 -  make tenant export and restoration more fragile;
 -  couple large tenants to the storage and migration lifecycle of small ones.

Physical PostgreSQL servers and connection poolers may be shared. Logical
Mastodon databases remain separate.


PostgreSQL capacity
-------------------

Host admission must use a global connection budget:

~~~~ text
tenant slots = max_connections
             - superuser_reserved_connections
             - reserved_connections
             - operational headroom
~~~~

As an initial example, `max_connections=200` with three superuser-reserved
connections and 17 operational connections leaves 180 tenant slots. That is 90
small, two-connection instances at the absolute boundary. Normal operation
should target 70–80% of that boundary to preserve space for migrations,
failover and tenant movement.

For higher density, use PgBouncer transaction pooling and enforce limits per
database and role. Verify prepared-statement behavior with SQLx before relying
on transaction pooling. Mega instances receive dedicated pools or database
hosts and do not share the small-tenant admission budget.


Promotion and placement
-----------------------

The control plane should move tenants between modes based on observed behavior:

| Tenant behavior                                          | Placement                                             |
| -------------------------------------------------------- | ----------------------------------------------------- |
| Long-dormant trial or small tenant                       | Scale-to-zero isolated process                        |
| Regular small tenant                                     | Warm isolated process                                 |
| Many small tenants after process memory becomes limiting | Bounded runtime shard                                 |
| CPU, federation, media or connection-heavy tenant        | Dedicated process                                     |
| Mega instance                                            | Dedicated compute and usually dedicated data services |

Promotion should use sustained CPU, RSS, request latency, database wait time,
connection saturation, Redis usage, federation queue latency and streaming
connections. Member count alone is not a sufficient placement signal.


Measurement gates
-----------------

Complete these measurements before moving between phases:

1.  Measure cold-start time through the real gateway, including health and
    database checks.
2.  Replay federation ingress, retries, media uploads and streaming clients in
    addition to local REST traffic.
3.  Measure how many dormant tenants can be woken concurrently without creating
    a PostgreSQL or CPU spike.
4.  Compare warm processes with scale-to-zero under representative daily usage.
5.  Attribute PostgreSQL, Redis and worker consumption per tenant.
6.  Run the same workload against Mastodon where performance claims are needed.
7.  Measure a shared-runtime prototype. Done on 2026-09-11 with a throwaway
    one: about 3 MiB of Eunha per tenant against 12–16 MiB as separate
    processes. Before production, repeat it with `Host` routing, per-tenant
    fairness, and PostgreSQL measured at the tenant counts a shard would reach.

The multi-instance runtime is justified when its measured savings exceed its
additional isolation, deployment and incident-response costs. Until then,
isolated processes with zero-minimum database pools and on-demand startup are
the preferred hosting model.
