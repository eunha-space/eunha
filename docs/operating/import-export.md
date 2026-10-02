Data export and import
======================

A member can take their follows, blocks, lists and the rest away to another
server, and bring them in from one, the way Mastodon's *Import and export*
settings do it. Eunha runs upstream's models and services for this; what
differs is that it serves them over REST under `/api/eunha/v1/` rather than
as settings pages (the `data-portability-rest-api` divergence), and runs the
background work on its own queue. The code is in *src/portability/* and the
routes in *src/api/eunha/portability.rs*.

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
`domain_blocks`, `bookmarks`, `custom_filters`).

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
it; a list member is followed, then added to the list, which fails if they
are on it already, as upstream's validation does. A row that imported is
deleted; one that did not stays, and is what the failures file lists.

### The queue

Upstream runs rows as Sidekiq jobs. Each eunha instance runs an import
queue instead (*src/portability/import.rs*), which takes a confirmed import,
runs fifty of its rows in row order, and moves on to the import that has
waited longest. Where it got to — whether the first pass is done, and the
last row handled — is kept in `eunha.bulk_import_progress`, under a lease, so
an import interrupted by a restart carries on from the next row, and an
import Mastodon's own workers had scheduled is taken up as well. A row that
errors fails at once rather than being retried, and an import whose rows
have all been run is finished (the `bulk-imports-run-in-process`
divergence).
