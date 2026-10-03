Data export and import
======================

A member can take their follows, blocks, lists and the rest away to another
server, bring them in from one, and download an archive of their posts, the
way Mastodon's *Import and export* settings do it. Eunha runs upstream's
models and services for this; what differs is that it serves them over REST
under `/api/eunha/v1/` rather than as settings pages (the
`data-portability-rest-api` divergence), and runs the background work on its
own queues. The code is in *src/portability/*, the routes in
*src/api/eunha/portability.rs*, and the web app's page is at
`/settings/export`.

This is one account's data. Moving a whole Mastodon instance's database onto
eunha is something else: see [Importing a Mastodon instance](./importing.md).


Exports
-------

`GET /api/eunha/v1/exports/{file}` serves upstream's `Export`, byte for byte,
with the same `Content-Disposition` file name:

| `{file}`              | Saved as                 | Contents                                                                  |
| --------------------- | ------------------------ | ------------------------------------------------------------------------- |
| `follows.csv`         | `following_accounts.csv` | `Account address,Show boosts,Notify on new posts,Languages`, newest first |
| `lists.csv`           | `lists.csv`              | a `list title,account` row per member, no header                          |
| `mutes.csv`           | `muted_accounts.csv`     | `Account address,Hide notifications`, newest first                        |
| `blocks.csv`          | `blocked_accounts.csv`   | one account a row, newest first, no header                                |
| `domain_blocks.csv`   | `blocked_domains.csv`    | one domain a row, no header                                               |
| `bookmarks.csv`       | `bookmarks.csv`          | one post URI a row, newest first, no header                               |
| `custom_filters.json` | `custom_filters.json`    | `{"custom_filters": [...]}`, by title                                     |

Local accounts are written `username@domain`, so that the file still names
them once it is read elsewhere. The CSV is Ruby's: a field is quoted only
when it holds a comma, a quote or a line break, an empty string is `""` and a
missing value nothing. A filter's `expires_at` is written as `JSON.generate`
writes a time, `2030-01-02 03:04:05 UTC`.

`GET /api/eunha/v1/exports` answers the export page's counts (`storage`,
`statuses`, `follows`, `followers`, `lists`, `mutes`, `blocks`,
`domain_blocks`, `bookmarks`, `custom_filters`), the account's archives, and
`can_request_backup`.

Exports need only the token's read scope for what they list, and work for an
account that cannot otherwise use the API, as upstream skips
`require_functional!` for them.


Imports
-------

An import is upstream's three steps:

1.  `POST /api/eunha/v1/imports`, multipart, with `type` (`following`,
    `blocking`, `muting`, `domain_blocking`, `bookmarks`, `lists` or
    `custom_filters`), `mode` (`merge` or `overwrite`) and the file as
    `data`. This is `Form::Import`: the file is read, checked, and stored as
    an unconfirmed `bulk_imports` row with a `bulk_import_rows` row per line.
    The answer carries `total_items`, and `likely_mismatched` when the file's
    headers or name suggest another type.
2.  `POST /api/eunha/v1/imports/{id}/confirm` schedules it;
    `DELETE /api/eunha/v1/imports/{id}` drops it instead. Either works only
    while it is unconfirmed, and one left unconfirmed for ten minutes is
    deleted.
3.  The queue carries it out. `GET /api/eunha/v1/imports` lists the ten
    newest with their `state`, `processed_items`, `imported_items` and
    `failure_count`, and once one is `finished`,
    `GET /api/eunha/v1/imports/{id}/failures` serves the rows that did not
    import, as a file of the same kind (`following_accounts_failures.csv`
    and so on, or JSON for filters).

### Reading the file

A CSV file is read as upstream reads it. If the first field of its first row
is one of `Account address`, `#domain`, `#uri` or `List name`, that row is
the header; otherwise every row is data, read with the type's default
columns: `Account address` for accounts, `#domain`, `#uri`, or `List name`
and `Account address` for lists. So Mastodon's header-less exports and the
ones with headers both import. A file without a column the type needs is
refused (`Incompatible with the selected import type`).

Fields are trimmed; a handle loses a leading `@`, a domain is lowercased,
`Show boosts`, `Notify on new posts` and `Hide notifications` are Rails
booleans (`false`, `f`, `0` and `off` are false, blank is unset), and
`Languages` is a comma-separated list. A filters import must be a JSON file
(`application/json`) in the export's shape.

Refused, with a 422 and upstream's message:

 -  a file over 20 MB (`File is too large`);
 -  an empty one, or one Ruby's CSV parser would reject, with its error;
 -  more than 20,000 rows;
 -  for follows, more rows than the follow limit leaves room for: 7,500, or
    1.1 times the account's followers once it follows more than that, less
    what it already follows unless overwriting.

### Carrying it out

`BulkImportService` runs first. In overwrite mode it undoes what the file
does not list — unfollows, unblocks, unmutes, removes bookmarks, deletes the
lists not named and empties the rest, deletes every filter — and settles the
rows naming what is already there. A domain block import is done entirely
here: upstream's overwrite lifts every block before blocking the file's
domains again, and so does eunha.

Then each row is `BulkImportRowService`: the handle is resolved, over
WebFinger when the account is not known (not on a server deliveries have
given up on), and followed, blocked or muted through the same services the
API uses; a post is found or fetched, and bookmarked if the account may see
it; a list member is followed, then added to the list. A row that imported
is deleted; one that did not stays, and is what the failures file lists.

A custom filter row is carried out in upstream's order: the filter is
created with its title and context, then given its keywords, its action,
its expiry and its posts, and saved again. What a step saved stays when a
later step fails, so a filter whose action is not `warn`, `hide` or `blur`
is left behind, with its keywords, every time its row runs.

### The queue

Imports run on the [job queue](./jobs.md) as upstream runs them on Sidekiq
(*src/portability/import.rs*). Confirming an import queues
`BulkImportWorker` on the `pull` queue, never retried, which marks it in
progress and runs `BulkImportService`; that queues an `Import::RowWorker`
for each row it leaves, all at once, and the job loops run them
concurrently, `[workers] job_concurrency` at a time.

A row job counts its row as processed, and as imported if it was, and the
import is finished once as many rows are processed as it has. A row that
raises — a list member already on the list, or not followed; a filter
upstream's validations refuse; a follow the target forbids — is retried six
times, on Sidekiq's schedule, and then counted as processed and not
imported. A row that simply finds nothing to act on, such as a handle that
does not resolve, fails at once.

`BulkImportService` indexes account rows by handle and bookmark rows by
URI, so when a file lists the same account or post twice only the last of
those rows is queued. The earlier one is never run, and the import stays in
progress, as upstream's does, until it is deleted with the rest.

An import `BulkImportService` fails on — a domain that cannot be blocked, a
list that cannot be made — is finished as it stands, as upstream's `rescue`
finishes it.

Migration 024 handed the imports eunha's earlier import loop was carrying
out to the queue, and with them any that Mastodon's own workers had
scheduled or started.

Imports, finished or not, are deleted a week after they were made.


Archive takeout
---------------

`POST /api/eunha/v1/backups` asks for an archive, at most once every six
days (upstream's `BackupPolicy`; the page says seven), under the Redis lock
`lock:backup:{user id}`. It is upstream's `BackupService`. The zip holds, in
this order:

 -  `outbox.json`, an `OrderedCollection` of every post, oldest first, as
    the `Create` or `Announce` that made it;
 -  the original of each attached media file, named by its storage path
    from after the last `/system/` (with an `S3_KEY_PREFIX`-style key
    prefix, under it): `media_attachments/files/{id partition}/original/{file}`;
 -  `likes.json` and `bookmarks.json`, collections of post URIs in the order
    of the posts' ids;
 -  `avatar.*` and `header.*`;
 -  `actor.json`, whose `outbox`, `likes`, `bookmarks`, `icon` and `image`
    name those files.

The JSON is what upstream's serializers write, not what eunha federates.
The outbox carries Mastodon's full JSON-LD context (every named context and
every extension of `ContextHelper`), and its items are unsigned and carry
none. Each note has the members of `ActivityPub::NoteSerializer`, including
the ones eunha leaves out of the notes it federates because it does not
serve what they point at: `atomUri`, `inReplyToAtomUri`, `conversation`,
`context`, and the `replies`, `likes` and `shares` collections. A boost of
one's own followers-only post carries the post inline. An attachment's
`url` is rewritten to the path of its URL without a leading `/system/`, as
upstream does, so with media served from their own host (no `/system/`) it
keeps a leading slash and does not name the file in the zip. `actor.json`
has the members and the context of `ActivityPub::ActorSerializer`.

The zip is stored where Paperclip stores a backup's dump,
`backups/dumps/{id partition}/original/archive-{time}-{hex}.zip`, in the media
bucket, and `backups` records it. The account's older archives are then
deleted, and the member is mailed upstream's link to
`/backups/{id}/download`. That page needs a signed-in web session (the
`account_session` cookie of the account pages, see
[accounts](./accounts.md)): a signed-out browser is sent to `/account/login`
with a `302`, and back to the link once it has signed in. Like upstream's
`authenticate_user!`, it lets in a member whose account is suspended or
disabled, so that they can still take their archive away, but not a
memorial. Someone else's archive, or one not built yet, is a `404`; one's
own is a `302` to a link to the file signed for an hour.
`GET /api/eunha/v1/backups/{id}/download` answers `{"url": ...}` with the same
link, for the settings app.

Archives are built by each instance's archive queue, from
`eunha.backup_jobs`, so a restart loses no request. A build that fails is
retried five times, backing off, and then the request is dropped.

Once a day, archives older than the `backups_retention_period` setting
(seven days unless an administrator changed it; not a positive number keeps
them) are deleted, files and all.
