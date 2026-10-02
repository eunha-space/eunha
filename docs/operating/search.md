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
