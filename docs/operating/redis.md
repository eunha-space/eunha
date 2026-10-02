Shared Redis
============

Eunha uses unprefixed Redis keys by default, which is appropriate when an
instance has a dedicated Redis process. A pooled deployment must give every
instance a unique prefix and a Redis user restricted to that prefix:

~~~~ toml
redis_url = "redis://tenant-example:password@redis-pool:6379/0"
redis_key_prefix = "tenant-example"
~~~~

The prefix may contain ASCII letters, digits, hyphens and underscores. Eunha
adds the separating colon, so the ACL key pattern for the example is
`~tenant-example:*`. Every Redis key Eunha owns — feeds, feed population
markers, ActivityPub and preview card locks, tombstones, the oEmbed endpoints
remembered for each domain, posting idempotency, notification
group state, async refreshes, the days each server failed deliveries on, the
activity counts behind trends and email domain blocks' `history`, the sets of
what was used today that trends are rescored from, the posts waiting to be
emailed to an account's subscribers, and sign-ins waiting on a second factor
with their attempt counts — uses that namespace.

Do not treat a prefix as authorization. Give each instance a distinct Redis
user, the matching key pattern, and only the commands Eunha uses:

~~~~
+get +set +setex +exists +fcall +zadd +zremrangebyrank +zrem
+zrangebyscore +zrevrangebyscore +mget +del +sadd +scard
+incrby +pfadd +pfcount +expire +smembers +srem +hset +hget +hincrby
~~~~

The hosting provisioner installs the fixed `eunha_compare_delete` function used
for lock release. Tenant users receive `FCALL`, but not `EVAL`, `EVALSHA`,
`SCRIPT` or `FUNCTION`, so a compromised credential cannot submit arbitrary Lua
to the shared event loop. Dedicated Redis remains zero-configuration: Eunha
falls back to its existing inline script when the named function is absent.
`INFO` is optional; without it the admin API reports the Redis version as
unknown. Process-wide memory from `INFO memory` is never exposed when a key
prefix is configured. Set `redis_process_metrics = false` to suppress it for an
otherwise dedicated deployment as well.

ACLs do not isolate CPU, memory, eviction or persistence. A shared pool remains
one performance and failure boundary and needs monitoring, bounded feed
retention, admission controls, and a path for moving heavy tenants to dedicated
Redis.

Feeds, their population markers and the remembered oEmbed endpoints use
`redis_url`; they are bounded cache state. Set `redis_coordination_url` to
route locks, ActivityPub deletion tombstones, posting idempotency,
notification grouping, async refreshes (see
[Remote replies](./remote-replies.md)) and the batches of posts waiting for
[email subscribers](./email-subscriptions.md) to a separate non-evicting Redis
pool, along with the
days each server failed deliveries on, `exhausted_deliveries:<host>` as
Mastodon names them, which mark a server unavailable once there are seven. If
it is absent, both classes use `redis_url` as they did before this option
existed. Both endpoints use the same `redis_key_prefix` and tenant credentials
may differ by embedding them in their respective URLs. Process-wide memory is
omitted from tenant-facing admin responses whenever a prefix or separate
coordination endpoint is configured.
