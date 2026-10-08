Posting and editing
===================

What `POST /api/v1/statuses`, `PUT /api/v1/statuses/:id` and the endpoints
around a post do, as Mastodon's `PostStatusService` and `UpdateStatusService`
do it. The handlers are in *src/api/mastodon/statuses/*.


Posting
-------

Before anything else, the post replied to must be there and visible to the
poster (`set_thread`), or the answer is a 404, *The post you are trying to
reply to does not appear to exist.*; so must the post quoted, with its own
message.

A content warning with no text, on a post that quotes nothing, becomes the
text, and the post is still marked sensitive. Then the post must pass
`Status`'s validations, answered as a 422 *Validation failed: …* listing what
failed:

 -  it needs text unless it has media or quotes a post; text that is only
    whitespace is none, and a poll does not stand in for it (*Text can't be
    blank*);
 -  at most 500 characters, the content warning included;
 -  no hashtag a moderator made unusable (*Text contained a disallowed
    hashtag: …*);
 -  a poll whose options, once each is stripped and the blank ones dropped
    (`Poll#prepare_options`), are two to four, unique and at most fifty
    characters each, ending five minutes to a month from now (*Poll options
    must have more than one item*, *Poll expires at is too soon*, and so on).

`scheduled_at` is read as `String#to_datetime` reads it: ISO 8601 with or
without a zone (UTC when there is none), or RFC 2822; a blank one schedules
nothing, and one in the past posts now. One that does not parse, or a post to
schedule that would not pass the validations above, is a 422 with Mastodon's
bare *Record invalid*.


Editing
-------

`PUT /api/v1/statuses/:id` says what the post now is, whole: Mastodon's
controller gives `UpdateStatusService` the text, content warning,
sensitivity, attachments and poll every time, each none when the request
leaves it out. An edit that does not give the attachments takes them off the
post (they stay attached, for its history), one that does not give the poll
removes it with its votes, and one that does not say `sensitive` marks the
post not sensitive unless it has a content warning. Only the language, which
falls back to the post's, and the quote policy, kept unless one is given, are
kept when left out.

A blank text, on a post that quotes nothing, becomes the content warning
given; the post's content warning is then left as it was, and the warning
given does not mark it sensitive. `media_attributes` change the next
attachments' `description` and `focus` (`x,y`, each read as `to_f`). A poll
given is validated as on posting, its votes reset when its options or
multiplicity change, and it ends `expires_in` from now.

An edit that changes nothing is no edit: the post is answered as it was. One
that does is validated as a new post is (text required unless the post now
has media or quotes, its length, disallowed hashtags), and nothing of a
refused edit is kept.


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
