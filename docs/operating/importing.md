Importing a Mastodon instance
=============================

A Mastodon instance moves to eunha by having its data restored into a database
eunha has migrated. There is nothing to convert: eunha builds the schema of the
release it tracks, so the dump's tables are the tables that are already there.

~~~~ sh
createdb eunha
DATABASE_URL=postgres:///eunha eunha migrate
DATABASE_URL=postgres:///eunha eunha import-mastodon dump.custom --domain seoul.earth
~~~~

`--check` reports what the dump holds and whether it fits, writing nothing.

This moves a whole instance. A member bringing their own follows, blocks and
lists over from another server uses the data import instead; see
[Data export and import](./import-export.md).

The restore is data-only and loads through the schema's foreign keys, so it
needs a superuser connection and a database with no Mastodon data in it. The
dump's own `schema_migrations` and `ar_internal_metadata` are left out: eunha's
[migrations](./migrations) seed those, and they are what the database is
described by.

Three things are checked, and each one refuses rather than guesses:

 -  **The release.** The newest migration in the dump's `schema_migrations` is
    the schema its data was written for, and it has to be the one this binary
    builds. Anything else puts data shaped for one schema into another, where
    columns added since keep their defaults instead of being backfilled and
    columns dropped since have nowhere to go. Upgrade the source Mastodon to
    the release eunha [tracks](../mastodon/tracking) and dump again, or use an
    eunha that tracks the source's release. `--allow-schema-mismatch` proceeds
    anyway.

 -  **The database.** Migrations must all be applied, and `accounts` must be
    empty.

 -  **The domain.** Mastodon keeps its instance actor as the `accounts` row with
    id `-99`, whose `username` *is* the instance's domain, so the restored data
    says which instance it came from. A dump that does not hold the domain that
    was asked for is refused after it is restored — nothing in a custom-format
    dump answers that question without reading its `accounts` data, which costs
    what restoring it costs. Discard that database rather than serving it.

Import an instance as itself. `--rename-from` moves its local account and status
URLs, and its instance actor, to a different domain, which is only useful for an
instance nobody federates with yet: remote servers remember accounts at the
domain they were seen under and will not follow them to a new one.

An imported instance keeps its accounts'
[signing keys](../mastodon/signing-keys), so it needs the ActiveRecord
encryption secrets the source Mastodon used. The import reads a dump, not the
source's *.env.production*, so these come across by hand. So should its
[`SECRET_KEY_BASE`](./instances#mastodon-s-secret-key-base), which keeps the
unsubscribe and password reset links that Mastodon mailed working.


Media
-----

The database holds a file's name, never its address, so the media has to end up
under the keys the instance already minted. Mastodon's `public/system` tree is
laid out by those keys, so moving it is a copy:

~~~~ sh
eunha import-media ~/seoul-earth/system
~~~~

Each file goes under the prefix `media_storage.key_prefix` namespaces the
instance's objects with, which is what keeps instances sharing a bucket apart;
a dedicated bucket leaves it empty and the keys are the tree's own paths.

What moves is the instance's own media. Most of a Mastodon media directory is
usually not that: every remote avatar, attachment, emoji and link preview it has
ever shown is cached there, and on an instance with any reach the cache dwarfs
what its own people posted. A cached file is a copy of somebody else's and is
fetched again when it is missing, so it is left behind. Which files those are is
read from the database — a remote account's avatar and a local one's sit in the
same directory under the same shape of path, and only the row says which is
which — so this needs the instance's database as well as its storage.
`--include-cached-remote` carries the cache too, to spare the re-fetching or to
keep media whose origin has since gone.

`--skip-existing` asks for each object before sending it, which is how an upload
that was interrupted resumes at the cost of a request per file already moved
rather than the file again. `eunha-upload-media` does the same work for a media
directory that has to go somewhere no config file describes.

Rehearse against real data before doing it for real: an instance gets one
attempt.
