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

The migrations are compiled into the binary from *migrations/*. The build script
watches that directory, so adding or editing a migration rebuilds the binary
that `eunha migrate` runs, without a `cargo clean` or a touched source file.

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


Sign-ups waiting before migration 025
-------------------------------------

Until migration 025, a sign-up waited in `eunha.pending_signups` for its link
to be followed, and only then became a `users` row. It is now a `users` row
with no `confirmed_at` from the start, as Mastodon writes it, and migration 025
drops the table. Before applying it, `eunha migrate` turns each sign-up still
within its day into such a user: the account with a new signing key, the user
with the address, password, locale, reason, app and time zone it was made with,
approved as registrations and its invite allow, and the invite's use counted;
the `account.created` webhook is queued for the server to deliver once it runs.
The user keeps the confirmation token its mail carries and the time it was
sent, so the link already in its inbox confirms it within two days of sign-up.

~~~~ console
$ eunha migrate
1 sign-up(s) awaiting confirmation kept as unconfirmed users.
1 expired or conflicting sign-up(s) dropped.
Migrations applied.
~~~~

A sign-up past its day, or whose username or address has been taken since, is
dropped with the table. So are all of them when `eunha migrate` cannot read the
instance configuration, which it says; `eunha rehearse-migration` and a
database migrated by the tests have none either. Whoever is dropped can sign up
again.


Custom emoji eunha uploaded
---------------------------

Eunha used to store an uploaded custom emoji at `emoji/<shortcode>.<ext>` in
the media bucket and its URL in `image_remote_url`, leaving Paperclip's file
columns empty. Mastodon finds no image for such a row, and eunha now reads
only the columns too, so `eunha migrate` moves each one to where Paperclip
keeps a local emoji, as [administration](./administration#custom-emoji)
describes: it reads the image back from its old key, stores it and its
`static` PNG under `custom_emojis/images/`, writes `image_file_name`,
`image_content_type`, `image_file_size`, `image_updated_at` and
`image_storage_schema_version`, clears `image_remote_url`, and deletes the old
object. The paths cannot be made to match without moving the files: Paperclip
names an emoji's by its id and a random file name, not its shortcode.

~~~~ console
$ eunha migrate
Migrations applied.
3 custom emoji(s) moved to where Mastodon keeps them.
~~~~

It runs on every `eunha migrate` and finds nothing once they are moved. An
emoji whose image is not at its old key, or whose URL is not eunha's, is left
as it was and named, and shows Mastodon's missing image until it is uploaded
again. With `--tenants`, each instance's own bucket is used; a single instance
needs its media storage in the configuration or the environment, or the
emoji wait for the next `eunha migrate` that has it.


Annual reports generated before migration 033
---------------------------------------------

Until migration 033, eunha labelled the annual reports it generated
`schema_version` 1, though their data had the keys of Mastodon's schema 2
(`archetype`, `top_statuses`, `time_series`, `top_hashtags`). Mastodon reads
schema 1 as its 2024 reports and looks for `most_reblogged_accounts` in them,
so serving such a database it would fail on every one of them; its web client
also shows a report's share link only for schema 2.

Migration 033 labels those reports schema 2 and empties their
`top_statuses.by_favourites` and `by_replies`, which Mastodon's schema 2 leaves
empty. The rest of their data is kept as eunha generated it, which can differ
from what Mastodon would have: posts counted by when they were made rather
than by their ids, a pollster told by whole numbers, a hashtag used only once
or named by its name rather than its display name, and a top post chosen
among originals only. A schema 1
report Mastodon made has `most_reblogged_accounts` and is left as it is.


Edit histories written before migration 034
-------------------------------------------

Mastodon keeps every version of an edited post in `status_edits`, the
current one included: the first edit records the original, stamped with the
post's `created_at`, and every edit records the version it made, stamped with
the post's new `edited_at`. `GET /api/v1/statuses/:id/history` serves exactly
those rows. Until migration 034, eunha recorded only the version each edit
replaced, stamped with that version's own time, and added the post as it is
when serving the history. Mastodon serving such a database would leave the
current version out of each history.

Migration 034 gives each local post whose last recorded version is older than
its `edited_at` its current version as the last row, by its author, stamped
with its `edited_at`. A history Mastodon wrote ends with that row already and
is left as it is; so is every remote post's, which eunha never recorded.
Remote posts edited before then have no history until their next edit, and
show only their current version until then.


Media types written before migration 035
----------------------------------------

`media_attachments.type` is Mastodon's enum: image 0, gifv 1, video 2,
unknown 3, audio 4. Until migration 035, eunha wrote audio as 3 and unknown as
4, so Mastodon serving such a database showed eunha's audio as attachments it
could not play, and eunha showed Mastodon's audio the same way.

Both numberings use the same integers, so migration 035 tells the rows apart
by what Mastodon never writes. Its unknown attachments are remote ones it has
not downloaded, which have no content type, and its audio is what it
transcoded to MP3, or a copy of one since removed, which keeps its
`meta.original`. So:

 -  a 3 whose content type is `audio/*` becomes 4, as does an upload of
    eunha's still waiting in its transcoding queue as audio;
 -  a 4 whose content type is anything but audio becomes 3, as does a remote
    one with no file, no content type and no `meta.original`, which is how
    eunha recorded an attachment from a domain blocked with `reject_media`.

What is left is a remote attachment that named no media type and whose
address had no extension, which eunha recorded without a content type: a 3 if
its ActivityPub type was `Audio`, a 4 if it was anything else but `Image` or
`Video`. Neither can be told from Mastodon's own. To list them:

~~~~ sql
SELECT id, "type", remote_url, file_meta FROM media_attachments
WHERE "type" IN (3, 4) AND file_content_type IS NULL AND remote_url <> '';
~~~~


Notifications written before migration 032
------------------------------------------

A notification points at what it is about: its `activity_type` and
`activity_id`. Until migration 032, eunha pointed a favourite's at the post
favourited, where Mastodon points it at the `Favourite`; a boost's at the post
boosted rather than the boost; a poll's at the poll's post rather than the
`Poll`; and a follow's sometimes at the recipient's follow of the sender
rather than the sender's of the recipient. Mastodon serving such a database
would find no post for these, and undoing a favourite or a boost would leave
its notification behind.

Migration 032 points each of them where Mastodon does when that activity
still exists: the sender's favourite of the post, the sender's boost of it
(kept even once deleted), the post's poll, the sender's follow of the
recipient. A notification whose activity is gone — a favourite since undone,
say, whose notification eunha did not remove — is left as it was, and
Mastodon shows it without its post. To list what is left:

~~~~ sql
SELECT id, "type", activity_id FROM notifications
WHERE activity_type = 'Status' AND "type" IN ('favourite', 'poll');
~~~~


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
