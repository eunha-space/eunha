Inbound statuses
================

What a remote server's `Create`, `Update`, `Delete`, `Like`, `Announce` and
`Undo` do here, as Mastodon's `ActivityPub::Activity` classes do them. They
run as `ActivityPub::ProcessingWorker` jobs (*docs/operating/jobs.md*); the
handlers are in *src/api/ap/inbox/*.


Audience and visibility
-----------------------

A status's audience is its object's `to` and `cc`, or the activity's when
the object has none (`StatusParser#audience_to` and `#audience_cc`). It is
public with the public collection in `to`, unlisted with it in `cc`, private
with its author's own followers collection in `to`, and direct otherwise.
Whether it is taken at all depends on that visibility
(`related_to_local_activity?`): a public or unlisted one from an account
someone here follows, or passed on by one (`relayed_through_actor`), through
an enabled relay, in reply to a local post or
to an account someone follows, or addressed to a local account; a private
one from a followed account, or passed on by one, or addressed to a local
account; a direct one
only addressed to a local account. Anything fetched on purpose is taken, and
so is anything delivered to a local account's own inbox, whose owner eunha
remembers on the job (Mastodon's `delivered_to_account_id`).

Every account already known in the audience, and the owner of the inbox it
was delivered to, can read the status (`process_audience`). Those it tags
are mentioned; the others are mentioned silently, and a direct message with
a silent mention becomes **limited** (`visibility = 4`), which the API shows
as `private`. A limited status is shown only to its author and the accounts
it mentions, and is never boosted. A local account the status tags but does
not address is mentioned, but its notification is decided as if the sender
were limited (`silenced_account_ids`), so a policy that filters or drops
limited accounts applies. Only tagged mentions notify, and a mention
notification is about the `Mention` (`activity_type` `Mention`), as
Mastodon's are.

A direct message goes into its author's conversations, whether the author
is local or remote (`deliver_to_conversation!`), and into those of each
local account it mentions whose mention notification was delivered rather
than dropped or filtered (`push_to_conversation!`): the others in each are
its author and everyone it mentions, not silently.

A reply named after an option of a local post's poll is a vote
(`poll_vote?`), counted unless the poll has ended, the voter wrote it, or
the voter already voted (once on a single-choice poll, once per option on
a multiple-choice one); the poll's tallies are sent out three minutes later
unless it hides them (`ActivityPub::DistributePollUpdateWorker`). Such a
reply to a remote poll, or naming no option, is a status. A local account's
vote through `POST /api/v1/polls/:id/votes` does the same for a local poll
(`VoteService#distribute_poll!`); on a remote poll, each choice goes to the
poll's author's own inbox as a `Create` of a `Note` named after it, with the
id `{voter}#votes/{id}` (`ActivityPub::VoteSerializer`).

A status already held, delivered again to a local inbox whose owner it does
not mention, gives that owner a silent mention, makes a direct message
limited, and goes into their home feed if they follow its author
(`postprocess_audience_and_deliver`). A status held under another author is
left as it is.

A tagged account that cannot be fetched because its server does not answer
is tried again with a `MentionResolveWorker` on the `pull` queue, seven
times on Mastodon's backoff; one whose server answers that there is no such
account is left out. An edit's mentions (`update_mentions!`) are the
accounts it now tags, mentioned not silently; an account it no longer tags
keeps a silent mention.


Conversations
-------------

Every status is in a conversation (`Status#set_conversation`). A remote
status names one by its `conversation`: a URI of ours names one of ours, and
any other is found or recorded under its `uri`. A reply that names none
joins its parent's; anything else, a boost included, starts one. A status
that is not a reply becomes the root of its conversation when it has none
yet (`parent_status_id`, `parent_account_id`), which names a local
conversation's context URL. A reply to an author's own reply is a reply to
whoever that one answered (`carried_over_reply_to_account_id`).

Muting a status (`POST /api/v1/statuses/:id/mute`) mutes its conversation,
so it works on any status the viewer can see; one without a conversation,
left from before, answers 422.

Mastodon resolves a context URL of ours, `/contexts/{account}-{status}`, by
looking the second half up as the conversation's id while the URL carries
the root status's id there; eunha does the same, so such a URL rarely finds
the conversation, and the reply joins its thread's instead.


Deletes and undos
-----------------

A `Delete` of the sender itself purges the account, once at a time. A
`Delete` of an authorization the sender gave to be featured revokes the item
of the local collection holding it. Otherwise, a URI on the sender's host is
remembered as deleted for six hours, so that its `Create` arriving late is
skipped, and tombstoned; then the sender's own status with that URI, or with
the object's `atomUri`, is forwarded and removed by `RemoveStatusService`,
which takes it off every feed and removes the boosts of it, or else the quote
stamp it names is revoked. A status of someone else's is never removed.

An `Undo` takes back only what the sender did: its boost, its follow of or
request to follow a local account, its favourite of a local post, its block
of a local account, and the acceptance it gave a local account's follow,
which leaves the follow a request again. An `Undo` naming its object by id
alone is tried as the sender's boost, follow or request, and block, in that
order. What is not found yet is remembered, so that it is skipped when it
arrives.


Attachments and polls
---------------------

A status's attachments are taken in the order its object lists them, at most
four, and that order is recorded (`ordered_media_attachment_ids`) and is the
order they are shown in. An edit keeps an attachment it still lists at its
URL, updated in place, records the new order, and leaves one it no longer
lists attached to the status, where the edit history can still show it.

A `Question` is a poll (`PollParser`). One with no option is not a valid
poll, and since the status is saved with its poll, the status is refused
whole; an edit to one is not kept, any of it. An update that does not say
the status was edited leaves a poll's options and multiplicity as they were,
and an edit that changes them resets its votes. An edit that is no longer a
`Question` destroys the poll, its votes and its notifications with it.

A poll ends at its `closed` time, now when `closed` is any other value but
`false`, or else at its `endTime`, each read as Ruby's `String#to_datetime`
reads it: RFC 3339 and RFC 2822, ISO 8601 in its extended or basic form,
with the time to the minute or the second or left out (midnight), a zone of
`Z`, `UTC`, `GMT` or an hour offset with or without minutes, and UTC when
there is none. What else `DateTime.parse` accepts, such as month names in
free text or a time with no date, is not read, and the poll then has no end (a
[recorded divergence](./divergences)).

The local accounts that voted in a remote poll hear that it ended from a
`PollExpirationNotifyWorker` job, queued for five minutes after its end when
one of them votes, and again when an update of a poll with votes moves its
end, unless it had already ended. Its author, being remote, is not told,
and a poll no one here voted in is noticed by no one. A local poll's job is
queued for when it ends, or five minutes after the end an edit gives it, and
tells its author and its local voters and sends its tallies out. A job run
before its poll has ended puts itself back until five minutes after.
