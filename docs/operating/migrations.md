Migrations
==========

Migrations are applied by `eunha migrate`, not by starting the server. A
migration takes as long as it takes and some are destructive — 4.7's account
merge deletes rows — so running them from a deploy script, before the new binary
starts, means a failure is found with the old version still serving rather than
with nothing serving at all.

Starting the server checks instead: an instance whose database is behind its
binary refuses to serve and says so, rather than running queries against a shape
that has moved. `eunha migrate --check` answers the same question without
applying anything, and exits non-zero when something is pending, so a deploy
script can gate on it.

`public.schema_migrations` is what makes a database self-describing: it is
seeded for everything through 4.6.0 by `007_mastodon_schema_versions.sql`, and
[`eunha import-mastodon`](./importing) refuses a dump whose newest migration is
not the one eunha builds. A migration whose work depends on the instance rather
than the schema — so far only the move of local signing keys into `keypairs` —
is applied from code at startup and records itself then; `mastodon:plan` lists
those separately from ones still to write.
