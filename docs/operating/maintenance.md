Maintenance commands
====================

The `tootctl` commands that look after what an instance keeps — its feeds, its
cache and counters, other servers' posts and media, and link preview images —
are eunha subcommands of the same names, with the same options and output:

~~~~ sh
eunha feeds build --all
eunha cache recount accounts
eunha statuses remove --days 90
eunha media remove --days 7
eunha preview_cards remove --days 180
~~~~

Each acts on one instance. With `--tenants`, pick it with `--instance`, which
may come anywhere after the group's name. Like the other one-off commands, they
refuse a database that `eunha migrate` has not brought up to date. Where
`tootctl` draws a progress bar, eunha prints only the final line; `--verbose`
still prints each record as it is processed, and errors are printed with the
id they came from while the rest carry on.

The daily vacuum already does the routine part of this (see
[content retention](./administration#content-retention)); these are for
clearing a backlog, or for an instance that keeps more than its retention
settings say.


Feeds
-----

`eunha feeds build USERNAME` rebuilds one account's home feed and lists from
the database, as `PrecomputeFeedService` does when a user comes back after a
week away. With `--all`, or no username, it rebuilds them for every active user
— confirmed, signed in within the last seven days, neither suspended nor
waiting for deletion — `--concurrency` (`-c`, five by default) at a time.
`--skip-filled-timelines` leaves alone any feed more than half full, and
`--dry-run` counts without writing.

`eunha feeds clear` deletes every home and list feed, and what tracks their
boosts (`feed:*`), under the instance's [Redis key prefix](./redis) only, found
with `SCAN` rather than `KEYS`. A feed is filled again when its user next comes
back after a week away, or by `feeds build`.

`eunha feeds vacuum` goes over every feed Redis holds and deletes each that
is not a recently signed-in user's, or one of their lists'. The daily vacuum
visits only the users who have stopped signing in, so this also catches feeds
whose user or list is gone.


Cache and counters
------------------

`eunha cache recount accounts` counts afresh each local account's follows,
followers and posts (direct messages aside); `eunha cache recount statuses`
counts each post's replies (direct ones aside), boosts, favourites and
accepted quotes. A count is written only where it changed, so `updated_at`
moves only on the rows that had drifted. Recounting statuses visits every post
the instance knows and takes a long time.

`eunha cache clear` is `Rails.cache.clear`: it deletes what eunha keeps where
Mastodon keeps its cache, under the instance's prefix. That is Mastodon's own
`cache:*` keys (the rate limits' counts, donation campaigns, the instance
activity), and the entries Mastodon reads from its cache by name but eunha
keeps outside the `cache:` namespace: the followers digests
(`followers_hash:*`), the oEmbed endpoints (`oembed_endpoint:*`), whether a
server's accounts have feature approval policies, translation languages and
translations, the active user counts, and fetched JSON-LD contexts
(`jsonld:context:*`). Each is worked out or fetched again when it is next
needed. Feeds, locks, the job queue and delivery state are not cache and are
left alone.


Other servers' posts
--------------------

`eunha statuses remove` deletes remote posts older than `--days` (90) that
nothing local refers to: no local account replied to, boosted, quoted, pinned,
favourited, bookmarked or was mentioned in it, and, unless `--clean-followed`,
nobody local follows its author. The candidates are listed first, into
`eunha.statuses_to_be_deleted`, then deleted `--batch-size` (1,000) at a time
straight from the table, as `tootctl` does, so what hangs off a post goes by
its foreign keys and no counter is adjusted. An interrupted run carries on
from that list with `--continue`.

It then removes uploads never attached to a post that are older than `--days`
less one (unless `--skip-media-remove`; a scheduled post's media counts as
attached), and the conversations no post is in any more, and analyses both
tables. `--compress-database` runs `VACUUM FULL` and `REINDEX` instead, which
locks the tables, so run it with the instance stopped. `--skip-status-remove`
does only the clean-up.

`tootctl` keeps its lists in `public`; eunha keeps them in its own schema, and
builds no temporary index, because the tracked schema already indexes
`statuses.conversation_id`.


Media
-----

Media is kept in the instance's bucket under its `media_storage` key prefix, at
the keys Mastodon gives it (see [Media](./media)), and nothing these commands
do reaches outside that prefix, so instances sharing a bucket are safe from
each other.

`eunha media remove` forgets copies of other servers' media attachments older
than `--days` (`-d`, seven): the files are deleted and the attachment's file
columns cleared, leaving its remote URL. `--keep-interacted` keeps media on
posts a local account favourited, bookmarked, quoted, replied to or boosted.
With `--prune-profiles` it removes remote avatars and headers instead, of
accounts neither webfingered nor updated within `--days`; with
`--remove-headers`, only headers. Those skip accounts followed by or following
anyone local, unless `--include-follows`. Eunha itself never downloads other
servers' media (it links to it), so this finds only the copies a Mastodon
sharing the database made.

`eunha media remove-orphans` lists every object in the instance's storage and
deletes those whose record is gone, or which are not a file the record holds:
a superseded avatar, say. Objects not laid out as Mastodon lays out media,
eunha's instance icon among them, are reported as unrecognized and kept. A
file counts as the record's when it has the record's file name, or its stem
with any extension, since a style may be kept in another format; Mastodon
insists on one of the attachment's style formats. What earlier versions of
eunha kept beside an attachment's file, a video's upload waiting under
`source/` and a small style named after the thumbnail, is kept while the
attachment is there. `--prefix` limits the listing, `--start-after` resumes it
from a key (both within the instance's prefix), and `--dry-run` deletes
nothing. Listing costs a request per thousand objects with most providers.

`eunha media usage` prints the space each kind of stored file takes, and the
local accounts' share, from the sizes the database records.
`eunha media lookup URL` prints the public page a media URL is shown on: the
post an attachment belongs to, or the profile an avatar or header belongs to.

`tootctl media refresh` downloads remote media again; eunha keeps no copies of
remote media, so there is nothing to download, and the command says so.
`tootctl media remove-orphans --fix-permissions` resets each object's ACL;
eunha never sets per-object ACLs, leaving it to the bucket's own policy to
serve what it stores, so it is not offered.


Preview cards
-------------

`eunha preview_cards remove` removes the images of link preview cards not
updated within `--days` (180), and with `--link` only link cards', leaving
photo and video cards alone. A card is fetched again only when its link is
posted two weeks after it was last used, so removing images newer than that is
not recommended. See [Preview cards](./preview-cards).
