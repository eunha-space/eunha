Data export and import
======================

A member can take their follows, blocks, lists and the rest away to another
server the way Mastodon's *Import and export* settings do it. Eunha runs
upstream's models for this; what differs is that it serves them over REST
under `/api/eunha/v1/` rather than as settings pages (the
`data-portability-rest-api` divergence). The code is in *src/portability/*
and the routes in *src/api/eunha/portability.rs*.

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
