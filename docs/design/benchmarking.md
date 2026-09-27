Small-instance benchmark
========================

`scripts/benchmark_small_instance.sh` measures a ten-user Eunha instance with
fresh PostgreSQL data and a namespaced Redis keyspace. It runs a read-heavy mix
of home/public timelines, notifications and account reads, with 5% status
writes. Every pool-size case uses a separate temporary database, which is
removed on exit.

Run a representative burst:

~~~~ sh
mise exec -- env EUNHA_BENCH_SECONDS=30 \
  EUNHA_BENCH_CONCURRENCY=8 \
  EUNHA_BENCH_POOLS="1 2 3 5" \
  scripts/benchmark_small_instance.sh
~~~~

The script requires a local PostgreSQL server, Redis on database 14, Node.js,
and a current `target/release/eunha`. Results are written below
`benchmark-results/`, which is ignored by Git.


2026-09-09 baseline
-------------------

Measured on an Apple Silicon Mac17,2 with 10 CPU cores and 24 GiB RAM, using
PostgreSQL 18 (`shared_buffers=160 MiB`, `work_mem=4 MiB`) and eight concurrent
clients for 20 seconds:

| Pool |  Idle RSS |  Peak RSS | Avg CPU | Peak active DB | Requests/s |      p95 | Errors |
| ---: | --------: | --------: | ------: | -------------: | ---------: | -------: | -----: |
|    1 | 32.64 MiB | 61.81 MiB |  59.52% |              1 |   1,017.95 | 13.87 ms |      0 |
|    2 | 32.16 MiB | 65.23 MiB |  98.03% |              1 |   1,776.00 |  7.19 ms |      0 |
|    3 | 32.70 MiB | 66.09 MiB | 127.40% |              2 |   2,131.05 |  6.42 ms |      0 |
|    5 | 32.78 MiB | 67.45 MiB | 152.64% |              2 |   2,144.55 |  6.74 ms |      0 |

At two concurrent clients, one, two and three connections delivered 758.75,
783.35 and 774.30 requests/s respectively, all without errors. Peak Eunha RSS
was 43.88–45.16 MiB.

This is an intentionally hot local API benchmark, not a federation or media
benchmark. It establishes the per-process baseline and connection-pool knee; a
production sizing exercise must additionally replay real federation ingress,
media uploads, network latency and retained data.


Hosting recommendation
----------------------

For a small, roughly ten-person hosted instance, start with:

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

Two connections were effectively identical to larger pools at ordinary
concurrency and retained most of the throughput at the deliberately excessive
eight-client load. Three is a reasonable promotion tier for a busy tenant;
five showed no material throughput benefit in this run. The idle timeout,
queue poll and worker threads come from the density measurements below, where
they cut an idle tenant's PostgreSQL memory by about five times and its threads
by three.

Budget PostgreSQL slots at the host level instead of multiplying its setting
until every tenant's theoretical pool maximum fits. Reserve slots for
operations and migrations, and enforce an admission budget such as:

~~~~ text
tenant slots = max_connections - superuser_reserved_connections
               - reserved_connections - operational headroom
~~~~

For example, `max_connections=200`, three superuser-reserved slots and 17
operational slots leave 180 tenant slots: 90 two-connection small instances,
before accounting for any dedicated large tenants. Do not plan to run at that
absolute edge; a 70–80% steady-state slot target leaves room for migrations,
failover and load movement.

Raising `max_connections` consumes PostgreSQL shared resources even while
connections are idle. `work_mem` is more dangerous: it applies per query
operation, and parallel workers can multiply it. On a 32 GiB host, 200 is a
conservative first ceiling with modest `work_mem`, measured under the actual
query mix; raising it further should follow load testing and memory telemetry,
not tenant count alone.

For substantially more small tenants, put PgBouncer in transaction pooling
mode between Eunha and PostgreSQL, cap pools per database/user, and verify its
prepared-statement configuration with SQLx. Keep mega instances on dedicated
PostgreSQL pools or hosts so they cannot consume the small-tenant connection
and memory budget.

Tenants per host
================

The small-instance benchmark measures one process. A hosting service needs the
other number: how many tenants one machine carries. That is mostly decided by
what tenants cost while nobody is using them, which one process under load
cannot show.

`scripts/benchmark_density.sh` answers it. It brings up a PostgreSQL cluster
and Redis server of its own, starts tenants in steps — one eunha process and
one database each, configured as recommended above — and at each step records
the population's idle cost and then its latency under an open-loop request
load spread across every tenant. At the end it restarts a burst of tenants at
once, which is what waking dormant tenants looks like.

~~~~ sh
mise exec -- env EUNHA_DENSITY_COUNTS="10 100 300" \
  EUNHA_DENSITY_LOAD_RPS="200 800" \
  scripts/benchmark_density.sh
~~~~

Every knob is an `EUNHA_DENSITY_*` variable at the top of the script, which
also records the traps that make naive measurements wrong. Two matter when
reading any number here:

 -  **Memory is phys\_footprint, not RSS.** RSS counts the binary's text once
    per process although all of them share it. A tenant's real cost is less
    than half its RSS.
 -  **PostgreSQL connections are costed privately.** Every backend's footprint
    repeats the shared buffer pages it has touched; the script subtracts that
    mapping and adds `shared_buffers` once.


2026-09-11 baseline
-------------------

Measured on the same Mac17,2 (10 cores, 24 GiB), PostgreSQL 18 with
`shared_buffers=1GB`, tenants with a two-connection pool and a 60-second idle
timeout. The machine was also a developer's workstation with most of its
memory already in use, which matters at the top of the range.

What one idle tenant costs, constant from 10 to 300 tenants:

| Resource                             |                  Per idle tenant |
| ------------------------------------ | -------------------------------: |
| eunha footprint, never served load   |                        14–18 MiB |
| eunha footprint after serving load   |                        37–40 MiB |
| Threads                              |                               12 |
| PostgreSQL connections held          |                                2 |
| PostgreSQL private memory            | 9.6 MiB (4.8 MiB per connection) |
| Transactions while idle              |                            3.1/s |
| CPU while idle, eunha and PostgreSQL |                0.09% of one core |
| Empty database on disk               |                         15.3 MiB |

Two of those are not what the tenancy plan assumed. A “zero minimum” pool
never reached zero: the queue loops polled every half-second, so no
connection ever sat idle long enough to close, and an unused tenant held two
PostgreSQL backends indefinitely. And the allocator keeps what a burst grew:
a tenant that has once served load stays at roughly 40 MiB, so capacity has to
be planned on that figure, not on a freshly started process.

Latency under load, spread evenly over every tenant:

| Tenants | 200 req/s p95 | 800 req/s p95 | 1,600 req/s p95 |
| ------: | ------------: | ------------: | --------------: |
|      10 |        6.8 ms |        7.3 ms |          150 ms |
|      50 |        8.0 ms |        7.4 ms |           91 ms |
|     100 |        9.7 ms |        6.5 ms |           35 ms |
|     200 |       10.1 ms |        9.0 ms |           75 ms |
|     300 |       10.0 ms |      4,608 ms |        2,540 ms |

A request costs about 1.95 ms of CPU across eunha and PostgreSQL together, so
800 req/s occupies roughly one and a half of ten cores. The collapse at 300
tenants is therefore not a shortage of CPU: eunha's CPU per request doubled
while most cores stayed idle, which points at scheduling 3,600 threads, or at
memory compression on a machine already short of memory, rather than at work.


Quiet queues
------------

The durable queues — deliveries, inbound activities, media — were polled every
half-second to two seconds whether or not anything could be waiting. Every job
is enqueued by a request the same process serves, so the loops now sleep until
that request wakes them, and otherwise back off to
`[workers] queue_idle_poll_seconds` (30 by default), which only bounds how late
a retry that has come due, or a job another process enqueued, is picked up.

With every other setting unchanged:

| Idle                              |          Before | After |
| --------------------------------- | --------------: | ----: |
| Transactions per tenant           |           3.1/s | 0.2/s |
| eunha CPU, 300 tenants            | 12.1% of a core |  0.8% |
| PostgreSQL CPU, 300 tenants       | 13.6% of a core |  4.8% |
| PostgreSQL connections per tenant |               2 |     2 |

The connections stay because a 30-second poll never lets a 60-second idle
timeout expire. That matters more than the baseline suggested: a connection
costs 4.8 MiB privately when fresh, but **19–24 MiB once it has served the
query mix**, as PostgreSQL keeps catalog caches and prepared statements for
the life of the backend. Two warm connections cost a tenant as much memory as
its eunha process does.

Load behaved as before, and 300 tenants at 800 req/s held a p95 of 8.2 ms. The
baseline's collapse at that point did not recur — nor did it when the baseline
binary was run again (p95 14.5 ms). It was the workstation, not eunha, and the
400-tenant run below shows what the workstation was running out of.


Hosting profile
---------------

Quiet queues stop the polling, but a 60-second idle timeout still outlives the
periodic tasks that run every minute, and every process still starts one Tokio
worker per core. The profile changes configuration only:

~~~~ toml
[database_pool]
idle_timeout_seconds = 20

[workers]
queue_idle_poll_seconds = 300
~~~~

with `TOKIO_WORKER_THREADS=2` in the environment.

| Idle, per tenant                        | Quiet queues | Hosting profile |
| --------------------------------------- | -----------: | --------------: |
| Threads                                 |           12 |               4 |
| PostgreSQL connections held, on average |            2 |            1.35 |
| PostgreSQL private memory               |    38–47 MiB |     about 7 MiB |
| New connections                         |         none |      2 a minute |
| Transactions                            |        0.2/s |           0.2/s |

The memory is the point. Connections are closed before they accumulate caches,
so a sampled connection stays at 4–5 MiB instead of 19–24 MiB. They did not
reach zero: the scheduled-status and poll-expiry tasks both ran every minute,
on two connections at once, and sqlx closes idle connections on a 20-second
cycle, so each was open for 20–40 seconds of every minute — until those tasks
were changed, below.

p95 latency at 200 and 800 req/s:

| Tenants |  Quiet queues | Hosting profile |
| ------: | ------------: | --------------: |
|      10 | 7.1 / 15.6 ms |    7.0 / 5.4 ms |
|     100 | 12.2 / 8.6 ms |   10.0 / 5.7 ms |
|     300 | 14.8 / 8.2 ms |  21.8 / 10.6 ms |
|     400 |       not run | 36.9 / 3,934 ms |

Up to 300 tenants the two are indistinguishable within this machine's noise.
At 400 the host collapsed at 800 req/s while decompressing 207,000 pages a
second: the workstation, already using most of its 24 GiB for other work, had
no memory left to keep 400 tenants resident, and every request to one whose
pages had been compressed waited for them. Memory, not CPU or connections, was
the ceiling, and the baseline's earlier collapse at 300 tenants — before the
harness recorded decompression — fits the same explanation.


Timed tasks
-----------

What kept those connections open was three timed tasks — scheduled statuses,
poll expiry and suspended account cleanup — which ran every minute or two
whether or not anything was due. Each now sleeps until its next item falls due
and, when nothing is, for `queue_idle_poll_seconds`, and is woken early when a
schedule or a poll is created or moved.

With the hosting profile, 100 tenants were left idle for eleven minutes and then
sampled every two seconds for ten:

| Idle, per tenant                        |     Before |        After |
| --------------------------------------- | ---------: | -----------: |
| PostgreSQL connections held, on average |       1.35 |         0.33 |
| New connections                         | 2 a minute | 0.8 a minute |
| Transactions                            |      0.2/s |       0.13/s |

Two things nearly made this measurement wrong, and both apply to any idle figure
taken with a long poll. An idle tenant's cost is periodic: with a 300-second
poll, a one-minute window can fall between two rounds of wake-ups and report
nothing at all, which is what the first attempt did. And tenants started
together wake together: the peak was 200 connections, every tenant's whole
pool at once, which is also what a host restart does to real tenants.


Waking tenants
--------------

A tenant started alone answers its first request in 145 ms. Started together:

| Burst | All answering |      Median |
| ----: | ------------: | ----------: |
|    10 |   0.45–0.49 s | 0.44–0.49 s |
|    50 |     1.9–2.0 s |   1.8–1.9 s |
|   100 |     3.7–3.8 s |   3.6–3.7 s |

None failed. Scale-to-zero is practical, but a host waking tenants after an
outage or a deploy should admit a bounded number of starts at a time rather
than all of them.


What one Mac mini holds
-----------------------

Memory decides it. Per tenant, from the runs above, for a process per tenant:

| Tenant                                        |     eunha | PostgreSQL |   Total |
| --------------------------------------------- | --------: | ---------: | ------: |
| Idle, never busy (hosting profile)            | 12–14 MiB |     ~2 MiB | ~16 MiB |
| Idle after serving load (hosting profile)[^1] |   ~40 MiB |     ~2 MiB | ~42 MiB |
| Warm, default settings                        |   ~40 MiB |  38–47 MiB | ~85 MiB |

PostgreSQL's share is a third of a connection on average at 4–5 MiB each, since
the timed tasks stopped waking every minute; it was 7 MiB before.

Planning on ~42 MiB — every tenant has at some point been busy — and setting
aside macOS and services (4 GiB), `shared_buffers`, and page cache and Redis:

| Host memory |     Set aside | For tenants | Tenants at 42 MiB | At 85 MiB |
| ----------: | ------------: | ----------: | ----------------: | --------: |
|      16 GiB | 4 + 2 + 2 GiB |       8 GiB |              ~190 |       ~95 |
|      32 GiB | 4 + 4 + 4 GiB |      20 GiB |              ~490 |      ~240 |
|      64 GiB | 4 + 8 + 8 GiB |      44 GiB |            ~1,070 |      ~530 |

Those are ceilings; plan steady state at 70–80% of them. One process for many
tenants would raise them several times over (see “One process for many
tenants”). At these counts the other resources do not bind first:

 -  **CPU.** A request costs 1.5–2 ms of CPU across eunha and PostgreSQL. Ten
    cores at half utilisation serve roughly 2,500–3,000 req/s, which for 490
    tenants is 5 req/s each at peak — far more than a small instance's clients
    make.
 -  **Connections.** 490 idle tenants hold about 160 connections on average, but
    tenants that wake together — after a host restart — briefly open their whole
    pools, and 490 two-connection pools is 980. `max_connections = 1000` only
    just covers that; spread the wake-ups, or put PgBouncer in front, well
    before then.
 -  **Threads.** Four per tenant is about 2,000 at 490, against a macOS
    host-wide limit of 30,720.
 -  **Disk.** An empty tenant database is 15 MiB. Real ones grow with federated
    content, which this benchmark does not model.

[^1]: Measured on tenants that had served the whole baseline sequence, with a
      worker per core. One tenant profiled after a single 30-second burst kept
      15–31 MiB whatever its worker count (see “Where a tenant's memory goes”),
      so ~40 MiB is a conservative figure to plan on.


Where a tenant's memory goes
----------------------------

Profiled with macOS's own tools — `MallocStackLogging` set on the eunha
process, then `heap`, `vmmap` and `malloc_history` — so without changing a
line of eunha. One tenant in the hosting profile, fresh and after 30 seconds at
160 req/s, across 16 runs:

|                                  |   Fresh | After load |
| -------------------------------- | ------: | ---------: |
| Footprint                        |  14 MiB |  15–31 MiB |
| Live allocations                 | 3.4 MiB |    3.4 MiB |
| Fragmentation in the malloc zone |  ~6 MiB | 7.5–22 MiB |

The live 3.4 MiB is mostly startup state that every process repeats: 1.4 MiB
for the router — 594 routes, each with its middleware layers cloned — about
0.3 MiB of root certificates, and 0.2 MiB for the S3 client. What belongs to
the tenant itself, its pool, Redis connection and configuration, is about
0.3 MiB. A burst of load leaves 35 KiB of new live data behind.

Of those, the router is now built once for the whole process rather than once
per instance, and what that saves was measured the same way as everything else
here: one process serving the same tenants, under the binary before the change
and the binary after it. Its harness and results are in
`benchmark-results/router-20260912/`.

| One process, idle           | 10 tenants | 50 tenants | Per tenant |
| --------------------------- | ---------: | ---------: | ---------: |
| A router for every instance |   37.0 MiB |  144.0 MiB |   2.68 MiB |
| One router for the process  |   14.0 MiB |   27.0 MiB |   0.33 MiB |

The per-tenant column is the slope between the two sizes, not footprint divided
by tenants, which would bury it under the ~10.5 MiB a process pays once however
many tenants it serves — both builds pay that, and both run 12 threads. So a
tenant's own router cost about 2.35 MiB of footprint, rather more than the
1.4 MiB of live allocations above: footprint counts the pages those allocations
sit on, not the bytes. At 50 tenants that is 117 MiB the process no longer
needs, and it is what stops the routes from being the thing that limits how
many tenants a process can hold.

Read the absolute numbers only against each other. These tenants have no data
and serve no requests, with pools of two connections, measured 30 seconds after
every tenant answered — so they cost less per tenant than the 3 MiB the
prototype measured under load with seeded databases. Both builds served the
same tenants alternately, twice over at each size, and repeated to the tenth of
a MiB.

The certificates and the S3 client are still each instance's own.

Everything else is fragmentation: freed space scattered across pages that each
still hold something live, which the allocator cannot hand back. It is
bimodal — a process either settles near 15 MiB or keeps 20–31 MiB — and does
not follow the number of Tokio workers: with one worker a tenant kept 29 MiB
twice, and with ten it never settled below 24 MiB. `malloc_zone_pressure_relief`
released nothing on any call. Latency was the same whichever way a run went.

Two consequences. A process per tenant pays about 10 MiB that one shared
process would pay once — router, certificates, the binary's dirty data pages,
stacks and allocator metadata — before any fragmentation. And the
fragmentation, the larger and less predictable part, is the allocator's rather
than eunha's.

### Allocators

For comparison, eunha was built with jemalloc (`tikv-jemallocator`) and with
mimalloc as its global allocator. Neither is kept; the system allocator is what
eunha uses. Same profile, at each allocator's default settings:

| Allocator               | Runs |  Fresh | After load | Two minutes later |        p95 |
| ----------------------- | ---: | -----: | ---------: | ----------------: | ---------: |
| System                  |    4 | 14 MiB |  25–35 MiB |         25–31 MiB |  7.7–13 ms |
| jemalloc 5.3.1          |    3 | 16 MiB |  29–35 MiB |         23–35 MiB | 7.5–7.9 ms |
| mimalloc (crate 0.1.52) |    3 | 16 MiB |  35–48 MiB |         20–36 MiB | 8.0–9.0 ms |

At their defaults neither does better than the system allocator once the load
has passed, and both start 2 MiB larger. Both return memory lazily — on later
allocations rather than on a clock of their own — so a process that goes quiet
keeps most of what it had; jemalloc gave memory back in one run of three,
mimalloc in all three but from a higher peak.


One process for many tenants
----------------------------

Whether sharing a process is worth building was measured rather than argued,
with a throwaway prototype: a binary that loads many tenants' configurations
into one process, each with its own database pool, Redis namespace,
application state, background tasks and listening port, on one Tokio runtime.
It is not a working multi-tenant server — it routes by port rather than by
`Host`, and the process-wide statics keep the first tenant's domain — but what
it costs to run is what a real one would cost. Its source, harness and results
are kept outside the repository, in `benchmark-results/modes-20260911/`.

The same tenants, configuration (the hosting profile) and load, each run from a
fresh start:

| eunha, per tenant           | Separate ×100 |   Shared ×100 |  Separate ×300 |   Shared ×300 |    Shared ×600 |
| --------------------------- | ------------: | ------------: | -------------: | ------------: | -------------: |
| Memory, idle after 11 min   |      12.0 MiB |       3.0 MiB |       12.1 MiB |       3.2 MiB |        2.9 MiB |
| Memory, a minute after load |      13.0 MiB |       7.0 MiB |       15.7 MiB |       3.3 MiB |        3.4 MiB |
| Threads, whole population   |           400 |            12 |          1,200 |            12 |             12 |
| Until every tenant answers  |        13.8 s |         1.7 s |         43.7 s |         6.2 s |         15.1 s |
| p95 at 200 / 800 req/s      | 11.4 / 8.9 ms | 10.6 / 786 ms | 19.2 / 10.9 ms | 17.3 / 8.4 ms | 31.0 / 21.8 ms |
| eunha CPU at 800 req/s      |          104% |           75% |            75% |           57% |            70% |

At 300 tenants eunha needed 4.7 GiB as separate processes and under 1 GiB as
one, and 600 tenants in one process needed 2 GiB. The shared process also spent
about a quarter less CPU on the same load. Its one bad number, shared ×100 at
800 req/s, came with CPU far from saturated and did not recur at 300 or 600
tenants; it reads as the workstation, but it happened once and is not
explained.

A saturated tenant did not slow its neighbours in either mode. With one tenant
sent 1,200 req/s while the rest shared 100, its own requests queued for seconds
behind its two connections, and the others' p95 stayed at 9–22 ms — faster, in
fact, than when measured alone just before, because that run found their
connections closed and had to open them again.

What limits the shared process is PostgreSQL. From 300 to 600 tenants its CPU at
800 req/s went from 119% to 251% of a core, more than three times eunha's, and
latency rose with it: 600 databases on one server, not 600 tenants in one
process.

With the timed-task change, then, an idle tenant in a shared process costs about
3 MiB of eunha and a third of a connection, against about 42 MiB as its own
process after serving load. On memory alone a 32 GiB Mac mini would hold
several thousand; that is not a number to plan on until PostgreSQL has been
measured holding that many databases.


Ways to scale further
---------------------

In order of what the measurements say they are worth:

1.  **One process for many tenants.** Measured with a prototype: about 3 MiB of
    eunha per tenant instead of 12–16, twelve threads for the whole process, a
    seventh of the startup time, and no slowdown from a saturated neighbour. It
    pays for fragmentation once, where a different allocator did not recover it
    (see “Allocators”). It needs the refactor and controls in phase 4 of
    [the multitenancy plan](./multitenancy.md), and PostgreSQL becomes the
    limit.
2.  **Spread idle wake-ups.** Tenants started together wake together every
    `queue_idle_poll_seconds` and open their whole pools at once. A little
    random jitter on each sleep would turn that spike into a steady trickle.
3.  **Dormancy.** A dormant tenant costs its database on disk and nothing else,
    and wakes in 145 ms. It needs the gateway and shared workers described in
    [the multitenancy plan](./multitenancy.md), and an admission limit on
    concurrent wakes.
4.  **PgBouncer** once connections approach `max_connections`, in transaction
    mode with prepared statements verified against SQLx.
5.  **PostgreSQL on its own machine.** On one Mac mini it competes with tenants
    for the same memory, and with tenants sharing a process it is where the CPU
    goes: at 600 tenants it used more than three times eunha's. Separating them
    gives both room, at the cost of a network hop on every query, which should
    be measured.
6.  **More machines,** placing tenants by measured memory rather than by count.

Spikes
======

The benchmarks above hold a steady load. A spike is the other thing an instance
meets: a sudden, bounded burst that it has to absorb and then recover from.
`scripts/spike_viral_post.sh` runs the classic one — a local post goes viral,
and thousands of remote actors the instance has never seen like, boost and
reply to it within a few minutes.

~~~~ sh
cargo build --release --bin eunha --bin eunha-fedisim
cargo install --locked inferno rustfilt
scripts/spike_viral_post.sh
~~~~

The remote side is `eunha-fedisim`, one process that plays every remote
server: `s<n>.fedisim.test`, each with its own actors. It serves their actor
documents and notes when eunha fetches them, signs the activities they send
with real RSA keys through feder, and sends them along a curve that rises to
`EUNHA_SPIKE_PEAK_RPS` at `EUNHA_SPIKE_RISE_SECONDS` and then decays. Each
actor likes, boosts and replies at most once. The load is open-loop: when eunha
falls behind, activities beyond the in-flight limit are counted as shed rather
than delayed, so the offered load stays what the curve says.

Around the spike the script records, once a second, the inbox queue, database
connections and lock waits, eunha's CPU and phys\_footprint, Redis's memory,
and the latency a local user sees on their home timeline, the post's thread and
their notifications.

Memory is also taken whole at four moments: before the spike, when it ends,
when the queue has drained, and after `EUNHA_SPIKE_COOLDOWN_SECONDS` idle. At
each one it records eunha's footprint and, of its malloc zones, what is live and
what is dirty but free. It also records the private memory of this database's
PostgreSQL backends, Redis, and the database's size on disk. That last moment
answers whether a spike's memory is ever given back. `vmmap` suspends the
process it reads, so a latency probe that overlaps a snapshot is marked
`(paused)` in *timeseries.csv* and left out of the summary.
It takes a flamegraph at baseline, at the peak and during recovery, with
macOS's `sample` — only stacks that were on CPU, demangled — and keeps the
unfiltered stacks beside them. It then waits for the queue to drain and writes
`summary.md` below `benchmark-results/`. Every knob is an `EUNHA_SPIKE_*`
variable at the top of the script.

### Nothing leaves the machine

A spike is only safe to run against a copy of a real instance if nothing it
provokes reaches the real fediverse, a push service or a mail provider. Three
things make sure of that, and each run reports on all three:

 -  **eunha's only proxy is the simulator.** Every HTTP client eunha has
    honours `HTTP_PROXY` and `HTTPS_PROXY`, and the simulator answers only for
    its own hosts. Any other request — every `https://` `CONNECT` included — is
    refused and counted by host. Media storage points at the simulator too,
    which accepts uploads and discards them.
 -  **eunha and the simulator are sandboxed.** Both run under `sandbox-exec`
    with outbound networking denied except loopback and PostgreSQL's socket.
    Name resolution is denied too, as nothing needs it. Before starting, the
    script checks that the sandbox really does refuse a request to the internet.
 -  **Every socket is audited.** Once a second the script lists eunha's
    sockets. A peer that is not loopback fails the run, and so does an audit
    that never ran.

The status that goes viral links to `https://example.com/`, as a canary: eunha
fetches a preview for it, so the proxy's refusal count must show exactly that
host. A canary that is not seen means the count cannot be trusted. Set
`EUNHA_SPIKE_OFFLINE=0` to run without the sandbox.

### Against a copy of a real instance

Left alone, the script creates a database holding the other benchmarks' ten
users. A copy of a real instance gives realistic table sizes and indexes. Make
the copy with `eunha rehearse-migration` on the database host, and name it in
`EUNHA_SPIKE_DATABASE`. The script refuses a name without `spike`, `clone` or
`rehearsal` in it, because it deletes the copy's push subscriptions and adds an
access token:

~~~~ sh
EUNHA_SPIKE_DATABASE=seoul_earth_spike EUNHA_SPIKE_DOMAIN=seoul.earth \
  EUNHA_SPIKE_ACCOUNT=someone \
  EUNHA_SPIKE_CONFIG_APPEND=encryption.toml \
  scripts/spike_viral_post.sh
~~~~

`encryption.toml` holds only the instance's `[active_record_encryption]`
table, which the copy needs to read its own signing keys.

Spiking a copy on the production host still shares that host's CPU and its
PostgreSQL server with the real instance. Run it somewhere else, or accept that
the real instance will feel the spike too.

### Driving it by hand

The simulator can also be run on its own and spiked on demand, for watching an
instance under load rather than measuring it. Start eunha with `HTTP_PROXY`
and `HTTPS_PROXY` pointing at the simulator, as the script does, or it cannot
reach the simulated servers:

~~~~ sh
eunha-fedisim --eunha http://127.0.0.1:3000 --domain example.test
curl -X POST 127.0.0.1:18990/__spikes -d '{
  "status_uri": "https://example.test/users/alice/statuses/1",
  "author_uri": "https://example.test/users/alice",
  "peak_rps": 200, "rise_seconds": 20, "duration_seconds": 120 }'
curl 127.0.0.1:18990/__stats
~~~~


2026-09-27 viral post
---------------------

Measured on the same Mac17,2, against a fresh database holding the ten
benchmark users, with a five-connection pool. 50,000 actors on 200 simulated
servers liked, boosted and replied to one post, peaking at 1,000 activities a
second 20 seconds in, for two minutes: about 46,000 activities, every one from
an actor the instance had never seen. Every run caught the canary, refused no
other host, and found no socket that was not loopback.

At a tenth of that — 10,000 actors, peaking at 100 a second — the queue never
held more than 16 jobs and eunha used a third of a core. The instance barely
noticed. At 1,000 a second it did. Ranges are over two runs before the changes
below and three after:

|                                   |        Before |         After |
| --------------------------------- | ------------: | ------------: |
| Activities accepted               |          100% |          100% |
| Actor documents fetched per actor |           2.0 |           1.0 |
| Inbox queue at its peak           | 18,631–20,888 | 11,002–13,356 |
| eunha CPU p95 during the spike    |      152–156% |      132–149% |
| Jobs failed                       |             0 |             0 |

The queue drained as the curve decayed, before the last activity arrived. A
local user's home timeline and notifications stayed under 75 ms at p95; their
view of the post's thread did not, at 90–230 ms, because by then it held
thousands of replies and a signed-in viewer is sent up to 4,096 of them, as
Mastodon does.

The peak flamegraph found where the CPU went, and it was not verifying the
activities' signatures (5%). It was eunha signing its own requests: every
actor seen for the first time is fetched with a signed GET, and that was a fifth
of all on-CPU time. Two things made it worse than it had to be, and both are
fixed:

 -  **Every new actor was fetched twice**: once by signature verification for
    its key, whose document was then thrown away, and again by the inbox worker
    to create the account. The first fetch now creates the account, as
    Mastodon's `FetchRemoteKeyService` does. A viral post is exactly when this
    matters, as it doubled the requests to the very servers sending the flood.
 -  **The instance actor's key was reloaded and reparsed for every GET**: read
    from the database, decrypted, and parsed from PEM with its CRT values
    precomputed, which together cost more than the signature. It is now parsed
    once per instance, through feder's `PrivateKey`.

Writing a test for the first also turned up an actor with no shared inbox
failing every activity with a 500, as its account could not be stored. It is
stored with an empty shared inbox now, as Mastodon does.

### Memory after a spike

|                         |  Before | Spike ended | Queue drained | Idle 120 s |
| ----------------------- | ------: | ----------: | ------------: | ---------: |
| eunha footprint         |  12 MiB |     325 MiB |       388 MiB |    151 MiB |
| of which live in malloc | 2.6 MiB |      15 MiB |        15 MiB |     10 MiB |
| Database on disk        |  15 MiB |     116 MiB |       116 MiB |    116 MiB |

eunha's live data never exceeded 15 MiB. Nearly all of the rest is *Malloc
Large (empty)* in vmmap: large blocks already freed, which macOS's allocator
keeps for reuse and returns on its own schedule — 331 MiB of it when the queue
drained, 107 MiB two minutes later. The largest allocations are responses, and
the thread is the largest response: while the harness still asked for it once a
second during cooldown, each request added about 7 MiB.

Two traps for anyone reading these numbers. RSS fell to 29 MiB while the
footprint stayed above 150 MiB, because macOS compresses an idle process's
dirty pages, which RSS does not count and footprint does; the process had not
given them back. And vmmap suspends the process it reads, which is why the
harness marks and discards the latency samples that overlap a snapshot, and
takes no snapshot during the spike itself.

Peak footprint and tail latency varied between runs as much as they did between
builds, so neither is claimed to have changed.


Not yet measured
----------------

 -  **Federation.** Inbound activities (signature verification, fetching remote
    actors) and outbound delivery fan-out are likely the largest CPU cost of a
    well-connected instance. The viral-post spike covers inbound activities
    from actors seen for the first time. Outbound fan-out is not covered yet.
 -  **Media.** Image decoding and blurhash are CPU-bound and bursty.
 -  **Streaming.** What each open WebSocket costs.
 -  **Real data.** Every database here was empty.
 -  **Dedicated hardware.** Every number was taken on a developer workstation
    already using most of its memory. Run the script on the machine that will
    serve tenants before sizing a fleet from it.
