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


Context
-------

`GET /api/v1/statuses/:id/context` answers with the post's ancestors and
descendants, as `Status::ThreadingConcern` finds them. The ancestors are the
chain from the post replied to up to the thread's root, the nearest kept when
there are more than the limit, root first; the descendants are the reply tree
under the post, depth first. Both walks go through posts since deleted, which
are not shown. A signed-in viewer gets up to 4,096 of each; anyone else 40
ancestors and 60 descendants at most twenty replies deep.

Each post then goes through `StatusFilter`, as
`permitted_statuses_from_ids` has it: a post of the viewer's own is always
shown; any other is left out when the viewer may not see it, blocks or mutes
its author or blocks its author's domain, or when its author is silenced and
not followed by the viewer, which, signed out, no one is. The author's own
replies to themself come first among the descendants, in their order.


Votes
-----

`POST /api/v1/polls/:id/votes` makes a vote for each choice in one
transaction, as `VoteService` does, each checked as `VoteValidator` checks
it: the poll has not ended, the choice exists, the voter is not the poll's
author, and the voter has not voted already — on a poll with one choice, at
all; on one with several, for that choice, a choice given twice included.
One vote that fails keeps none of them, and the answer is a 422 listing what
failed (*Validation failed: You have already voted on this poll*, *The chosen
vote option does not exist*, *You cannot vote in your own polls*, *The poll has
already ended*). Votes are counted once they are all made.


Boosts and favourites
---------------------

A boost is a post of the booster's whose `uri` is the id of its `Announce`,
as `Status#store_uri` stores it: `/statuses/{id}/activity` under the
booster's actor, `/ap/users/{id}` or `/users/{username}` as the account's id
scheme has it (`TagManager#activity_uri_for`). The `Announce` is the one the
outbox serves (`AnnounceNoteSerializer`): addressed to the public and cc'ing
the booster's followers when public, to the followers and cc'ing the public
when unlisted, to the followers alone when followers-only, and cc'ing the
boosted post's author first in every case. It names the boosted post by its
URI, except that an account boosting its own followers-only post sends the
post along, which its followers could not fetch otherwise. Undoing the boost
sends an `Undo` with the id `{actor}#announces/{id}/undo`, addressed to the
public, carrying that `Announce` with the boosted post named by its URI
(`UndoAnnounceSerializer`). Eunha left local boosts' `uri` empty until
migration 036; see
[migrations](../operating/migrations#local-boosts-before-migration-036).

A new favourite of a remote post is a `Like` with the id
`{actor}#likes/{favourite id}` (`LikeSerializer`), sent to its author's own
inbox rather than their server's shared one, as `FavouriteService` sends it;
favouriting the post again sends nothing. Undoing it sends the `Undo` with
`/undo` added to that id (`UndoLikeSerializer`), to the same inbox.


What a post is federated as
---------------------------

The `Note` a post is federated as is `NoteSerializer`'s. Its custom emoji are
those its content warning, text and poll options name, each once, in the
order first named; a shortcode stuck to a letter or a colon is not one, as
`CustomEmoji::SCAN_RE` reads them. Its `inReplyTo` is nothing once the post
replied to is deleted, that post's `url` when its URI is not HTTP, and
otherwise its URI, a local post's named by its account's scheme. Its
mentions are tagged in the order they were made. An edit's `Update` is
`published` at the edit, in whole seconds; a poll's `Update` is addressed
`to` alone, as `UpdatePollSerializer` writes it; and pinning or unpinning
sends an `Add` or `Remove` without an `id`, as `AddNoteSerializer` and
`RemoveNoteSerializer` do.


Who may feature an account
--------------------------

Every account the API renders carries `feature_approval`, and its
`current_user` says where the signed-in viewer stands, as
`Account#feature_policy_for_account` decides: `automatic`, `manual`,
`missing` for a remote account that federated no policy, `unknown` for a
policy with a flag eunha does not know, or `denied`, which is also the answer
to nobody signed in. A local account allows everyone unless it is locked, when
only its followers and itself, or nobody when it is not discoverable.

Eunha renders accounts in many places that do not know who is asking, so, as
Mastodon's `StatusCacheHydrator#hydrate_account` sets the field on a payload
rendered once, eunha sets it on each response for its viewer: every account
object in it, embedded in a post, a notification or a list, is answered with
one query for the follows in both directions.
