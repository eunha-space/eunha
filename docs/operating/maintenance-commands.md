Accounts, domains and emoji
===========================

The rest of `tootctl` that runs an instance day to day is in `eunha` under the
same names, with the same options, saying the same things: `eunha accounts`
beyond [creating and changing accounts](./first-account), `eunha domains`,
`eunha emoji` and `eunha maintenance`. The clean-ups of feeds, the cache,
statuses, media and preview cards are [Maintenance commands](./maintenance).
Each but `eunha maintenance`, which needs only a database, acts for the
instance as its server would, so it needs the instance's Redis as well as its
database. With `--tenants`, a command names the instance with `--instance`:

~~~~ sh
eunha --tenants /path/to/tenants accounts approve --all \
  --instance garden.eunha.space
~~~~

What a command sets off in the background, a delivery or an archive, goes into
the [job queue](./jobs) for the running server to do. `--dry-run`, where a
command has it, says what would be done, with ` (DRY RUN)` after it, and does
none of it. The commands that work through many accounts take
`--concurrency`, how many at once (five unless said otherwise), and report an
account that fails as `Error processing ID: …` and go on.


Accounts
--------

 -  `eunha accounts rotate USERNAME`, or `--all` for every local account that
    is not suspended, gives each a new RSA key. Its profile then goes out as
    an `Update` to every server that knows it, signed with the old key, which
    is the one those servers hold; with `--all`, a thousand accounts every
    five minutes. The account's other keys stay, and the instance actor is
    left alone, since a running server keeps its key for the life of the
    process (`key-rotation-replaces-the-rsa-keys`,
    `command-line-leaves-the-instance-actor-out`).
 -  `eunha accounts delete USERNAME`, or `--email ADDRESS`, deletes the user,
    its address and everything the account posted, as deleting one's own
    account does but at once; the username stays taken.
 -  `eunha accounts approve` approves accounts awaiting approval: `--all` of
    them, the oldest `--number N`, or USERNAME's. Each is welcomed as an
    approval through the API welcomes it.
 -  `eunha accounts follow USERNAME` has every local account that is not
    suspended follow the local account USERNAME, past the follow limit.
    `eunha accounts unfollow ACCT` has every local follower of ACCT stop.
 -  `eunha accounts reset-relationships USERNAME` with `--follows` unfollows
    everyone the account follows and then does what a new account's arrival
    does (following its inviter when the invite says to, and telling the
    staff who manage users); with `--followers` it removes every follower.
 -  `eunha accounts backup USERNAME` asks for an archive of the account, which
    the running server builds and mails the link to, as asking for one in the
    settings does, but without the week between archives.
 -  `eunha accounts cull [DOMAIN…]` asks every remote account's server for
    it, and removes those it answers `404` or `410` for. An account seen
    within the last week is left alone, and so is every account on a server
    that could not be reached; those servers are listed at the end. An
    account that was asked about is marked as seen, even in a dry run, so
    that the next run passes over it.
 -  `eunha accounts prune` destroys the remote accounts nothing here refers
    to: no posts, follows or follow requests either way, mentions,
    favourites, blocks, mutes or reports. Bots, groups, and suspended or
    silenced accounts stay.
 -  `eunha accounts refresh USERNAME@DOMAIN…`, `--domain DOMAIN` or `--all`
    fetches remote accounts' actors again. Mastodon downloads their avatars
    and headers again instead; eunha keeps no copies of them
    (`accounts-refresh-fetches-the-actor`).
 -  `eunha accounts merge FROM TO`, both `username@domain`, gives TO
    everything FROM had and removes FROM: for the duplicates a server that
    changed its domain leaves behind. It refuses two accounts whose keys
    differ, unless `--force`. The keys compared are the ones each account is
    verified with (`remote-duplicates-told-apart-by-their-keypairs`).
 -  `eunha accounts fix-duplicates` only says it is deprecated, as it does in
    Mastodon 4.7, whose schema keeps an actor's id unique.


Domains
-------

`eunha domains purge DOMAIN…` removes every account from the domains without
leaving a trace, unlike a suspension: if the server is still there, its
accounts come back when they are resolved again. Their custom emoji go too,
and the `instances` view is refreshed. `*.DOMAIN` or `--include-subdomains`
takes the subdomains with it, `--by-uri` matches the host of the accounts'
actor ids instead of their handles' domain, `--purge-domain-blocks` removes
the domains' blocks as well, and `--limited-federation-mode` purges every
domain that has not been explicitly allowed instead of the ones named.
`DELETE /api/v1/admin/instances/:domain` purges a single domain the same way
(see [Administration](./administration)); these options are only here.

`eunha domains crawl [START]` walks the fediverse by Mastodon's REST API:
each server's `/api/v1/instance`, `/api/v1/instance/peers` and
`/api/v1/instance/activity`, then each peer in turn. Without START it begins
at the servers this instance knows. `--concurrency` (fifty by default) is how
many servers are asked at once, `--exclude-suspended` leaves out the servers
suspended here and their subdomains, and `--format` is `summary` (servers,
registered accounts, and last week's logins and sign-ups), `domains` (every
server asked, a line each) or `json`. The requests go through the same guarded
client as every other request to a server someone else chose.


Custom emoji
------------

`eunha emoji import PACK.tar.gz` makes a local emoji of each `.png` and `.gif`
file in a gzipped tarball, its shortcode the file's name with `--prefix` and
`--suffix` around it. An emoji that exists already is skipped, unless
`--overwrite` gives it the new image. `--category NAME` files them under a
category, made if it is not there, and `--unlisted` keeps them out of the
picker. Each is checked and stored as an [upload](./administration#custom-emoji)
is; what fails is listed with why, and the rest are imported.

`eunha emoji export DIR` writes the local emoji, or `--category NAME`'s, to
`DIR/export.tar.gz`, each under its shortcode and its image's extension; an
archive already there is kept unless `--overwrite`. `eunha emoji purge`
deletes every custom emoji and their files, `--remote-only` other servers',
and `--suspended-only` those of servers suspended by a domain block and their
subdomains.


Duplicates in the database
--------------------------

A unique index sorts its entries by the database's collation, which comes from
the operating system's C library. When an upgrade changes the library's locale
data, an index built before no longer finds what it holds
([PostgreSQL's wiki]
explains), lets duplicates in, and a dump of the database can no longer be
restored, because building the index again fails.
`eunha maintenance fix-duplicates` is `tootctl maintenance fix-duplicates`: it
drops each affected unique index, removes or merges what it hid, and builds the
index again.

 -  Users sharing an address: the most recently updated keeps it, and the
    others' are prefixed with a number, to be given another with
    `eunha accounts modify` or deleted. A confirmation or password reset
    token held twice is taken from all but one.
 -  Local accounts sharing a username: it lists them and asks which to keep;
    the others are renamed `username_1`, `username_2` and so on.
 -  Remote accounts sharing a handle: the most recently updated stays, and
    another with the same key is merged into it, as `accounts merge` merges;
    one with another key is removed.
 -  Statuses sharing an id: the oldest stays, and is given a duplicate's
    favourites, mentions, poll, bookmarks, pin, replies and boosts when both
    are by one account.
 -  Hashtags, emoji and their categories, conversations, domain blocks and
    allows, unavailable domains, email domain blocks, account domain blocks,
    announcement reactions, media shortcodes, preview cards, security keys and
    webhooks, each as Mastodon's task handles them. The update check's records
    are deleted, to be fetched again.

It refuses a database at an older schema than the release eunha tracks and
asks before going on with a newer one, refuses while anything else is
connected to the database, and asks once more before it starts. It takes long
and may be destructive: stop every process using the database, and have a
backup. A dump [`eunha import-mastodon`](./importing) cannot restore for
duplicates is repaired at its source with this, pointed at the Mastodon
database (`fix-duplicates-for-the-tracked-schema`).

[PostgreSQL's wiki]: https://wiki.postgresql.org/wiki/Locale_data_changes
