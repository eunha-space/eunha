Tracking Mastodon
=================

The Mastodon release eunha implements is recorded in `mastodon.toml` and
repeated as build metadata in `Cargo.toml`'s version, which `build.rs` checks
the two agree on. Releases are tagged the same way:

~~~~
v0.2.0+mastodon.4.7.3
~~~~

Eunha's own version moves independently; the part after `+` names the Mastodon
release whose schema and API that build implements, and is what
`/api/v1/instance`, `/api/v2/instance` and NodeInfo report.

`eunha-schema` (`mise run mastodon:status`, `mastodon:plan`, `schema:check`)
does the tracking:

~~~~
mise run mastodon:status                 # is there a newer Mastodon release?
mise run mastodon:plan --to v4.8.0       # what would adopting it involve?
mise run schema:check                    # does this database match the target?
~~~~

`schema:check` reads the live database back out of Postgres and compares it
against a **reference**: the structure of a database that Mastodon's own
ActiveRecord built from its `db/schema.rb`. Comparing Postgres to Postgres means
comparing everything Postgres knows — tables, column types, nullability and
defaults, the columns an index actually covers, every constraint *by name* as
well as by definition, sequences, and view definitions — rather than what a
parser of ours believes a Ruby file means. It is the test for the
100%-compatibility claim, and it runs on every `cargo test`.

Two things are deliberately not compared, because they record how a database
came to be rather than what it is, and `schema.rb` cannot express either:

 -  **Sequences left behind by dropped tables.** Mastodon creates one sequence
    per snowflake-id table by hand, so nothing owns it and dropping the table
    leaves it behind — `encrypted_messages_id_seq` has outlived its table since
    2022. Every Mastodon that migrated through that period has it; one
    installed fresh today does not. Eunha matches the former, because that is
    what it stands in for. A sequence whose table *does* exist, or one the
    reference has and the database lacks, is still reported.
 -  **Sequence ownership.** `quotes` was created with a serial id and later
    moved to `timestamp_id`, so a migrated Mastodon owns that sequence and a
    freshly loaded one does not.

Three files under `mastodon/` support it, all regenerated together by
`mise run schema:build-reference`:

 -  `schema.rb` — upstream's own file, vendored verbatim.
 -  `schema.sql` — a `pg_dump` of a database built from it, for reading and
    diffing.
 -  `schema.json` — the same database's structure as the checker sees it, which
    is what the test compares against so that it needs neither Ruby nor a
    database of its own.

Building the reference runs Mastodon's `schema.rb` through the real ActiveRecord
schema DSL. That needs Ruby, but not Mastodon: `activerecord` and `pg`, not its
thousand-gem bundle.


Counterparts
------------

Eunha is laid out by feature and Mastodon by Rails layer, so one eunha module
often stands for a controller, a service, a worker and a serializer upstream.
Rather than mirror Mastodon's directories, every Rust file under *src/* names
its counterparts at the end of its module documentation:

~~~~ rust
//! Mastodon: app/services/remove_status_service.rb
//! Mastodon: app/workers/removal_worker.rb
~~~~

Each line holds one or more comma-separated paths in the Mastodon repository;
a path ending in `/` names a directory. A module with no counterpart says
`//! Mastodon: none`, so that every module has declared one or the other.

`cargo test` holds the headers to the tracked release: every module must have
one, and every path must be in *mastodon/files.txt*, the server-side tree of
that release (`app/` without its web client, `config/` without translations,
`lib/`, `streaming/` and `db/views/`). The file starts with the tag it was
recorded from, and the suite fails when that is not the tracked release, so
adopting one means recording it again, and a header naming a file upstream
renamed or deleted fails until it is corrected.

Read the other way round, the headers answer what a release touches:

~~~~
mise run mastodon:changes --to v4.8.0
~~~~

lists every server file changed between the tracked release and the target,
under each eunha module that names it, followed by the changed files no module
names, which eunha does not implement. A rename is listed under its old path,
the one headers name. Both this and `mise run mastodon:files` read a local
Mastodon checkout (`$MASTODON_REPO`, or *~/Git/mastodon*; `--repo` for
another), fetching its tags if it lacks one, because a release's diff runs to
more files than GitHub's compare API returns.


Adopting a release
------------------

1.  `mise run mastodon:plan --to vX.Y.Z` lists the Rails migrations upstream
    added since the tracked release, and the deliberate divergences that now
    need re-examining against it. `mise run mastodon:changes --to vX.Y.Z`
    lists the eunha modules whose counterparts changed, which is the
    behavioural side of the same question.

2.  Write one eunha migration reproducing them, ending with an
    `INSERT INTO public.schema_migrations` of the versions it covers — a
    migration eunha deliberately does not implement is left out of that list, so
    that a Mastodon booted on the database still runs it. `--sql` prints the
    insert.

3.  Update `mastodon.toml` and `Cargo.toml`, replace `mastodon/schema.rb` with
    that release's, and run `mise run schema:build-reference`. The reference's
    diff is the schema delta you are adopting. Run `mise run mastodon:files`
    too, and correct any header that names a path the release no longer has.

4.  Re-examine each divergence and move its `reviewed_for` forward in
    `divergences.toml`; the suite fails until every entry has been looked at.

5.  `mise run schema:check` against a database that has run the new migration.

6.  Rehearse it against real data before deploying. An instance gets one attempt
    at a migration:

    ~~~~
    eunha rehearse-migration postgres://user@localhost/seoul_earth
    ~~~~

    That clones the database (reading only), runs the pending migrations over
    the clone as the server would — they are the ones compiled into the binary,
    so there is no `migrations/` path to get wrong — and reports how long they
    took, every table whose row count changed, and the schema check. Anything
    that moves rows it should not is visible there rather than in production.
    It leaves the clone to be looked at; `--drop-clone` does not.
