Search
======

Search works as Mastodon's does, with the same two backends. Without
Elasticsearch, which is the default, accounts and hashtags are found in
PostgreSQL and posts are found only by their URL. With Elasticsearch or
OpenSearch configured, posts are searched by their text too, and accounts and
hashtags are ranked by the search server.

The code is in *src/search/*. `GET /api/v2/search` is `SearchService`,
`GET /api/v1/accounts/search` is `AccountSearchService` on its own, and
`GET /api/v1/peers/search` is the peers search controller.


Without Elasticsearch
---------------------

This is Mastodon with `ES_ENABLED` unset.

**Accounts.** A query with an `@` in the middle that is a complete handle
(`alice@example.com`, with or without the leading `@`) first looks for that
account exactly, on the first page only. A handle on this instance's domain
looks for the local account. With `resolve=true` a remote handle that is not
known yet is looked up over WebFinger, unless the domain is one this instance
does not federate with. The exact match is found whatever state the account is
in, so a suspended account still answers to its handle. With
`following=true`, it is dropped unless the searcher follows it.

The rest of the page is ranked with PostgreSQL's text search, the same SQL as
`Account.search_for` and `Account.advanced_search_for`:

 -  The display name weighs most, then the username, then the domain. Every
    term is a prefix, so `ali` finds `alice`.
 -  The rank is multiplied by a boost from the account's followers per
    following, its number of followers, and how recently it posted.
 -  A signed-in searcher sees accounts on either side of a follow with them
    first. With `following=true`, only the accounts they follow (and
    themselves) are searched.
 -  Suspended accounts, accounts being deleted, moved accounts and local
    accounts that are unconfirmed or unapproved are left out.

A query from someone not signed in that is shorter than three characters gets
the exact match or nothing.

**Hashtags.** Listable hashtags whose name starts with the query, shortest
first. A leading `#` is ignored, and the query is normalized the way hashtags
are (`HashtagNormalizer`), so full-width letters and accents match their plain
forms. With `exclude_unreviewed=true`, only reviewed hashtags are returned,
except the one whose name is the query itself. For a signed-in searcher each
hashtag says whether they follow and feature it.

**Posts.** Not searched. A post is found by its URL only, with `resolve=true`:
the URL is resolved instead of searched for, as Mastodon's `ResolveURLService`
does, and the result is the post or account it names, if the searcher may see
it.

**Peers.** `GET /api/v1/peers/search` returns up to ten known domains that
start with the query, after normalizing it as a domain. Blocked domains are
left out, and a blank query returns `null`.


The request
-----------

`GET /api/v2/search` takes the parameters Mastodon's does:

| Parameter            | Meaning                                                                                        |
| -------------------- | ---------------------------------------------------------------------------------------------- |
| `q`                  | Required. A missing or blank `q` is a 400.                                                     |
| `type`               | `accounts`, `statuses` or `hashtags`. Without it, all three are searched.                      |
| `limit`              | 20 by default, at most 40. A negative value is a 400, and `0` returns nothing.                 |
| `offset`             | Only used when `type` is given. Needs a signed-in user, as does `resolve`; anonymous is a 401. |
| `resolve`            | Resolve a URL, or look up an unknown remote handle over WebFinger.                             |
| `following`          | Only accounts the searcher follows. Ignored for anonymous searches.                            |
| `exclude_unreviewed` | Leave out hashtags not yet reviewed.                                                           |
| `account_id`         | With Elasticsearch, only that account's posts.                                                 |
| `min_id`, `max_id`   | With Elasticsearch, only posts after or before that id's time.                                 |

Typographic quotes (`“”„«»「」『』《》`) in `q` are read as `"`.

`GET /api/v1/accounts/search` needs a signed-in user with the `read:accounts`
scope. It takes `q`, `limit` (40 by default, at most 80), `offset`, `resolve`
and `following`.


With Elasticsearch
------------------

### Configuring it

Elasticsearch 7 and later and OpenSearch 1 and later work, as they do for
Mastodon. Each instance has its own settings under `[instance.elasticsearch]`
([several instances in one process](./instances#search) shows them all), and
a single instance configured from the environment reads Mastodon's variables:

| Variable           | Setting         | Meaning                                                            |
| ------------------ | --------------- | ------------------------------------------------------------------ |
| `ES_ENABLED`       | `enabled`       | `true` turns search on. Anything else leaves it off.               |
| `ES_HOST`          | `host`          | `localhost` by default. A URL with `https://` uses TLS.            |
| `ES_PORT`          | `port`          | 9200 by default.                                                   |
| `ES_USER`          | `user`          | Basic authentication, when the cluster asks for it.                |
| `ES_PASS`          | `pass`          | Its password.                                                      |
| `ES_PREFIX`        | `prefix`        | Put before each index name with `_`: `garden_statuses`.            |
| `ES_PRESET`        | `preset`        | `single_node_cluster` (default), `small_cluster`, `large_cluster`. |
| `ES_CA_FILE`       | `ca_file`       | A PEM certificate to trust for the cluster's TLS.                  |
| `ES_QUERY_TIMEOUT` | `query_timeout` | How long a search may take on the cluster, `10s` by default.       |

The preset decides replicas and shards as Mastodon's does: no replicas on a
single node, one replica on a small cluster, and on a large cluster one
replica and twice the shards. Then create the indexes and fill them:

~~~~ sh
eunha search deploy
~~~~

Instances sharing a cluster each need their own `prefix`. A cluster a Mastodon
on the same database used can be kept: the indexes have the same names,
settings and mappings, so `eunha search deploy` finds them up to date and only
imports again.

### What is indexed

The five indexes are Mastodon's, with the same analyzers and mappings:

 -  `accounts`: searchable accounts, that is not suspended, not moved, and
    local ones confirmed and approved. The bio is indexed only for a
    discoverable account.
 -  `tags`: listable hashtags, with whether they are reviewed and how many
    accounts used them in the last week.
 -  `public_statuses`: public posts, not boosts, by accounts that are
    `indexable`.
 -  `statuses`: every post someone here may search, which is their own, ones
    mentioning them, and ones they favourited, boosted, bookmarked or voted in.
 -  `instances`: the known domains, less the blocked ones, with their number
    of accounts.

A post's document holds its content warning, its text, its poll options and
its media descriptions, its hashtags, language and properties (`media`,
`image`, `video`, `audio`, `poll`, `link`, `embed`, `sensitive`, `reply`,
`quote`), and in `statuses` the local accounts that may search it.

### Keeping the indexes current

Indexing works as Mastodon's does. When a post is created, edited or deleted,
boosted, favourited or bookmarked, when an account is saved or its counts move,
and when a hashtag is used or reviewed, its id is added to a Redis set,
`chewy:queue:<Index>` under the instance's key prefix. Once a minute a
background task takes the ids out a thousand at a time and re-reads their rows:
those still in the index's scope are indexed, and the rest deleted. An id
leaves the set only after its batch was written, so the set is a durable
queue: while the cluster is down the ids wait, and a restart loses none of
them. Once an hour the instances index is brought up to date. When an account
becomes `indexable`, or stops being, its public posts are queued for the
public index.

This is why a new post takes up to a minute, and the index's 30 second
refresh, to be found. A queue Mastodon left behind on the same Redis is drained
too, since the sets have the same names.

### Searching

**Posts** are searched for a signed-in user only, in both post indexes: the
public one, and the `statuses` index limited to what that user may search.
The query syntax is Mastodon's:

| Syntax                                   | Meaning                                                       |
| ---------------------------------------- | ------------------------------------------------------------- |
| `word`                                   | Posts with the word in the text, stemmed as English.          |
| `"two words"`                            | The phrase.                                                   |
| `#tag`                                   | Posts with the hashtag.                                       |
| `-word`, `-"two words"`                  | Posts without it.                                             |
| `from:me`, `from:alice`, `from:a@b.org`  | Posts by that account. One that is not known finds nothing.   |
| `has:media`, `is:reply`                  | Posts with that property (any of the list above).             |
| `language:en`                            | Posts in that language.                                       |
| `before:2024-01-01`, `after:`, `during:` | Posts by date, in the searcher's time zone.                   |
| `in:library`, `in:public`                | Only posts the searcher interacted with, or only public ones. |

Any of the filtering ones can be negated with `-`. An unknown prefix
(`foo:bar`) is searched as the words `foo bar`. An invalid date is a 422, and a
query that does not parse finds nothing. A clause that is only an emoji
shortcode (`:blobcat:`), or an empty quoted phrase (`""`), is a 500, as
Mastodon's query transformer raises on both and nothing rescues it.
`account_id`, `min_id` and `max_id` become `from:`, `after:` and `before:`; an
`account_id` that names no account is a 404. Results are newest first, and are
filtered for the searcher afterwards as Mastodon's `StatusFilter` does: posts
they may not see, posts by accounts they block or mute or whose domain they
block, and posts by limited accounts they do not follow are left out.

**Accounts** are searched by username and display name, and from
`/api/v2/search` also by the bio of a discoverable account, boosted by the
number of followers; the accounts the searcher follows come first, or are the
only ones with `following=true`. **Hashtags** are matched by name prefix,
weighed by recent use and how recently they were used, with the hashtag that
is the query moved to the front. **Peers** are matched by domain prefix and
weighed by number of accounts.

If the cluster fails, accounts and hashtags are searched in the database
instead and posts find nothing, as in Mastodon. A peers search answers 500, as
upstream queries the index there without a rescue and without the stoplight.
After ten failures in a row the
instance stops asking the cluster for five minutes (Mastodon's
`SearchStoplight`); the count is kept per process.

### `eunha search deploy`

`tootctl search deploy`. It creates each index that does not exist, and
re-creates, emptied, each whose mapping, analysis or shard count differs from
what this binary would create. Then it imports every row from the database,
and deletes documents whose row no longer exists. Smaller indexes go first, so
that they are searchable sooner.

| Option                 | Meaning                                                                                   |
| ---------------------- | ----------------------------------------------------------------------------------------- |
| `-c`, `--concurrency`  | Batches written at once, 5 by default.                                                    |
| `-b`, `--batch-size`   | Rows in a batch, 100 by default.                                                          |
| `--only accounts,tags` | Only these of `instances`, `accounts`, `tags`, `statuses` and `public_statuses`.          |
| `--no-import`          | Create or upgrade the indexes, but import nothing.                                        |
| `--no-clean`           | Do not delete documents whose row is gone.                                                |
| `--only-mapping`       | Update a changed index's analysis and mapping in place, without re-creating or importing. |
| `--resume`             | Carry on an interrupted import from the last batch it wrote.                              |
| `--instance`           | With `--tenants`, the instance.                                                           |

Chewy notices a changed specification from a digest it keeps in its own
`chewy_specifications` index; eunha compares the cluster's mapping and
analysis settings with its own instead, so there is no `--reset-chewy`. While
an index is imported its refresh is turned off, and turned back on after.
`--resume` is eunha's own: the last batch written is kept in Redis
(`search:deploy:<index>`), and a run without it starts from the first row.

### Running the tests against a cluster

The integration tests in *tests/integration/c2s/search\_elasticsearch.rs* run
only when `EUNHA_TEST_ELASTICSEARCH_URL` names a cluster with security off,
and each creates indexes under a prefix of its own and deletes them after.
With Docker, or OrbStack:

~~~~ sh
docker run -d --name eunha-search-test -p 127.0.0.1:19200:9200 \
  -e discovery.type=single-node -e xpack.security.enabled=false \
  -e ES_JAVA_OPTS="-Xms512m -Xmx512m" elasticsearch:7.17.27
EUNHA_TEST_ELASTICSEARCH_URL=http://127.0.0.1:19200 \
  cargo test --test integration search_elasticsearch
docker rm -f eunha-search-test
~~~~

For OpenSearch, run `opensearchproject/opensearch:2` with
`-e DISABLE_SECURITY_PLUGIN=true` in place of the `xpack` setting.
