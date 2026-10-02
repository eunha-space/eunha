Migrations
==========

Migrations are applied by `eunha migrate`, not by starting the server. A
migration takes as long as it takes and some are destructive — 4.7's account
merge deletes rows — so running them from a deploy script, before the new binary
starts, means a failure is found with the old version still serving rather than
with nothing serving at all.

A migration gets one attempt against an instance's own data, which no fixture
contains. `eunha rehearse-migration <database-url>` clones a live database,
applies what is pending over the copy, and reports how long it took, every table
whose row count moved, and whether the result still matches the Mastodon release
eunha tracks — see [tracking Mastodon](../mastodon/tracking). The source is only
read from, and the clone is left in place to be looked at.

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


Site settings
-------------

Eunha used to serve the site's title, descriptions, contact address,
registrations, privacy policy and terms of service from `[instance]` until an
administrator saved them, and now reads them only from Mastodon's tables. So
that an upgrade does not leave an instance titled `Mastodon` with registrations
closed, `eunha migrate` copies what the configuration says into the settings
nobody has saved, as `eunha settings import-config` does
([administration](./administration#settings-and-the-instance-configuration)
lists the keys):

~~~~ console
$ eunha migrate
Migrations applied.
wrote site_title
wrote registrations_mode
kept saved site_contact_email
Site settings imported from the configuration.
~~~~

It does so once per database, and only for one that was serving: migration 023
leaves a row in `eunha.site_settings_import` when the database already has
users, and the import removes it. A database migrated before it has any users —
a new instance, or one about to receive `eunha import-mastodon` — is owed
nothing and starts from Mastodon's defaults or the restored data. Whatever an
administrator saved is kept, and nothing is imported again afterwards, even
into a setting deleted later.

The single-instance `eunha migrate` reads `[instance]` from `config.toml` and
the environment for this; with `--tenants`, each tenant's file. When it cannot
read one, it says so, migrates anyway, and leaves the import for the next
`eunha migrate` that can. `eunha migrate --check` imports nothing, and
`eunha rehearse-migration` has no configuration, so its clone keeps the row and
gains no settings.


Domain blocks written before migration 015
------------------------------------------

Until migration 015, eunha stored `domain_blocks.severity` as noop 0, silence
1, suspend 2. Mastodon stores silence 0, suspend 1, noop 2, and eunha now
does too. Migration 015 converted the other enums eunha had numbered its own
way (report categories, IP block severities), but it cannot convert domain
blocks: both numberings use the same three integers, so a row does not say
which of them wrote it.

A block that eunha's admin API created before migration 015 now reads as
follows: a suspend reads as noop, a silence reads as suspend, and a noop
reads as silence. List every block and correct each one by hand:

~~~~ sql
SELECT id, domain, severity, created_at FROM domain_blocks ORDER BY id;
UPDATE domain_blocks SET severity = 1 WHERE id = …; -- suspend
~~~~

Blocks created by Mastodon, or imported from a Mastodon database, were never
affected.


Quote policies written before migration 017
-------------------------------------------

Until migration 017, eunha stored a post's `quote_approval_policy` as its own
enum: public 0, followers 1, nobody 2, manual 3. Mastodon stores, and eunha
now stores, a bitmap. The automatic policy sits in the high 16 bits and the
manual one in the low 16: public is `131072`, followers `262144`, and nobody
`0`. Migration 017 converts eunha's followers, nobody and manual values.

Eunha's public, 0, is also Mastodon's nobody, so migration 017 cannot tell
the two apart. Local posts eunha wrote with “anyone may quote” before the
migration now let no one quote them. Their authors can open them up again
through the interaction policy endpoint.


List reply policies written before migration 020
------------------------------------------------

Until the release that added migration 020, eunha stored a list's
`replies_policy` as followed 0, list 1, none 2. Mastodon stores list 0,
followed 1, none 2, and eunha now does too. No migration converts the
column: lists belong to local accounts whether eunha or Mastodon created
them, so a row does not say which numbering wrote it.

On an instance that eunha has run from the start, every list whose policy was
set to “followed” or “list” now reads as the other. Swap them back once:

~~~~ sql
UPDATE lists SET replies_policy = CASE replies_policy WHEN 0 THEN 1 WHEN 1 THEN 0 END
WHERE replies_policy IN (0, 1);
~~~~

On a database that Mastodon ran before eunha, run it only for lists created
after the switch to eunha (filter on `created_at`).
