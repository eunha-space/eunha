Posting and editing
===================

What `POST /api/v1/statuses`, `PUT /api/v1/statuses/:id` and the endpoints
around a post do, as Mastodon's `PostStatusService` and `UpdateStatusService`
do it. The handlers are in *src/api/mastodon/statuses/*.


Edit history
------------

A post's edit history is a `status_edits` row for each version, the current
one included, as `Status::SnapshotConcern` writes it
(*src/status\_snapshot.rs*). The first edit records the original, stamped with
the post's `created_at` and by its author; every edit then records the version
it made, stamped with the post's new `edited_at` and by whoever made it. An
edit that changes nothing records nothing and leaves `edited_at` alone.

`GET /api/v1/statuses/:id/history` serves exactly those rows, oldest first,
each with the account that made it. A post never edited has no rows, and its
history is the one version built on the spot, stamped with its `created_at`.

A moderator marking a local post sensitive, and an approved appeal marking it
not sensitive again, edit it as the instance actor (`Account.representative`).
A remote post's history is recorded from its `Update`s; see
[inbound statuses](./inbound-statuses#attachments-and-polls).

Eunha recorded histories another way until migration 033; see
[migrations](../operating/migrations#edit-histories-written-before-migration-033).


Mentions
--------

An edit mentions whoever its text now names, as `ProcessMentionsService`
does. Whoever it no longer names stays mentioned, silently: they keep seeing a
private or direct post, but the post no longer shows them as mentioned, is not
addressed to them, and does not tag them. Naming them again makes the mention
active again. An account the author blocks, or one on a domain the author
blocks, is never mentioned, and a mention of it made before is removed.
