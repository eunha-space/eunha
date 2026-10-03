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
markers and the boosts each feed tracks (`feed:home:<id>:reblogs` and
`feed:home:<id>:reblogs:<status>`, likewise for lists), ActivityPub and
preview card locks, tombstones, the oEmbed endpoints
remembered for each domain, posting idempotency, notification
group state, async refreshes, the days each server failed deliveries on, the
activity counts behind trends and email domain blocks' `history`, the sets of
what was used today that trends are rescored from, the posts waiting to be
emailed to an account's subscribers, the users who signed in each day
(`activity:logins:<day>`), sign-ins waiting on a second factor
with their attempt counts, translated statuses with the language list of
the translation service, the JSON-LD contexts fetched to check Linked Data
signatures (`jsonld:context:<url>`), what ojak remembers for the inbox (the
activities already processed, `ojak:inbox:<origin>:<id>:<digest>`, for a day;
those already forwarded, `ojak:forwarded:*`, for a week; and the remote keys
eunha does not store in `accounts`, `ojak:key:<key id>`, for an hour), the
search index queues
(`chewy:queue:<Index>`, see [search](./search)), the counts that limit how many
new remote
accounts one domain or one request may bring (`unique_subdomains_for:*`,
`discovery_per_request:*`), whether a domain's accounts have feature approval
policies (`feature_approval_policy_availability:*`), the circuit breakers on
deliveries
(`stoplight:<inbox>:*`), and the streaming channels (`timeline:*`) with the
`subscribed:<channel>` keys that say a stream listens on one — uses that
namespace.

Streaming is Redis pub/sub, as in Mastodon: whatever publishes a status,
notification or deletion `PUBLISH`es it on the instance's channels, and every
process serving streams for the instance subscribes to the channels its
connections listen on. Pub/sub channels have ACL patterns of their own, so the
tenant user needs `&tenant-example:*` as well as `~tenant-example:*`.

Do not treat a prefix as authorization. Give each instance a distinct Redis
user, the matching key pattern, and only the commands Eunha uses:

~~~~
+get +set +setex +exists +fcall +zadd +zremrangebyrank +zrem
+zrange +zrangebyscore +zrevrangebyscore +zrevrank +zscore +mget +del
+sadd +scard +incrby +pfadd +pfcount +expire +smembers +srem +hset +hget
+hincrby
+scan +sscan
+publish +subscribe +unsubscribe
~~~~

`SCAN` is only ever given a `MATCH` pattern under the instance's own prefix: the
federation admin page uses it to find the servers deliveries are failing to, as
Mastodon lists its `exhausted_deliveries` keys. `SSCAN` reads the search index
queues, and is only used when Elasticsearch is enabled.

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

Feeds, their population markers, the remembered oEmbed endpoints, and
ojak's JSON-LD contexts and inbox records use `redis_url`; they are bounded
cache state. Losing one of ojak's costs a refetch, or a redelivered activity
processed again, which processing an activity tolerates; Mastodon keeps no
such record at all. Kept in Redis rather than in each process's memory, a
redelivery is recognised whichever process it reaches. Set
`redis_coordination_url` to route locks, ActivityPub deletion tombstones,
posting idempotency, notification grouping, async refreshes (see
[Remote replies](./remote-replies.md)) and the batches of posts waiting for
[email subscribers](./email-subscriptions.md) to a separate non-evicting Redis
pool, along with the days each server failed deliveries on,
`exhausted_deliveries:<host>` as Mastodon names them, which mark a server
unavailable once there are seven. If it is absent, both classes use `redis_url`
as they did before this option existed. Both endpoints use the same
`redis_key_prefix` and tenant credentials may differ by embedding them in their
respective URLs. Process-wide memory is omitted from tenant-facing admin
responses whenever a prefix or separate coordination endpoint is configured.
