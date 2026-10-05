API entity parity
=================

The schema check answers whether eunha's database matches Mastodon's. The
entity check answers the API half: a client reads fields by name, so a missing
one breaks it and an unexpected one is a divergence nobody decided on.

`mastodon/entities.json` records what each of Mastodon's REST entities carries,
and tests fetch real responses from a running eunha and compare. It is built
from two sources because neither is sufficient alone — `app/serializers/rest`
decides what is actually emitted, and `app/javascript/mastodon/api_types` states
plainly which fields are optional, where the serializers hide that behind `if:`
conditions. 4.7.1's instance serializer emits `icon` and `wrapstodon` that the
TypeScript does not mention, so the serializers are the authority on what
exists.

~~~~
mise run entities:build        # re-record from a Mastodon checkout
~~~~

That reads a clone at `~/Git/mastodon` (`MASTODON_REPO` to point elsewhere) at
the tracked tag rather than its working tree, fetching tags if the tag is
missing. Mastodon is not a submodule: 424MB of history for 468KB of files that
only matter when adopting a release, and a submodule bump's diff is a SHA,
whereas the diff of what is recorded here *is* the change being adopted.


Boost counts
------------

As in Mastodon's `Status#increment_count!` and `#decrement_count!`, a remote
post's reported boost count is adjusted only when it is known. An absent
`status_stats.untrusted_reblogs_count` stays `NULL` through boosting, undoing a
boost, deleting the boost and purging its author. The REST serializer falls
back to the locally observed `reblogs_count` in that case. A known remote
count increments up to 100,000,000 and decrements down to zero; local posts
use their locally observed count and leave the remote count untouched.

Earlier Eunha code used PostgreSQL's `LEAST(NULL + 1, 100000000)`, which returns
100,000,000, when a member boosted a remote post with no reported count.
Clients consequently displayed `100M`. The corrected updates prevent this;
already stored counts are not reset automatically, because a legitimate
reported count can reach the same cap.
