Account moves
=============

A member who leaves for another account, here or on another server, takes
their followers with them the way Mastodon does it. Eunha runs upstream's models
and workers for this; what differs is that it serves them over REST rather than
as settings pages (the `account-moves-rest-api` divergence). The code is in
*src/moves.rs*.


Aliases
-------

Before anyone can move *to* an account, that account has to say it is the
same person: it lists the old account as an alias.
`POST /api/v1/profile/aliases` with `{"acct": "old@example.com"}` is upstream's
`AccountAlias`. The handle is kept as typed, less a leading `@`, in
`account_aliases.acct`; it is resolved, over WebFinger if the account is not
known yet, and the actor id it names goes in `account_aliases.uri` and onto the
end of `accounts.also_known_as`.

That column is the local actor's `alsoKnownAs`, which is what a follower's
server checks before it believes a `Move`. The actors the account had under the
instance's `previous_domains` follow it, which is what `eunha accounts move`
relies on after a domain change. Creating an alias sends the profile `Update`;
deleting one,
`DELETE /api/v1/profile/aliases/{id}`, takes the id out of the column and, as
upstream, sends nothing until the profile next changes.

An alias is refused, with a 422 and upstream's message, when the handle is
blank or its domain is not a domain name, when it cannot be found, when it is
the account itself, or when the account already has it.

Migration 018 repairs aliases eunha made before it did this: it stored the
handle in `uri` and never wrote `also_known_as`. Such handles become `acct`,
and aliases that held an actor id are copied into an empty `also_known_as`.


Moving
------

`POST /api/v1/accounts/move` with `acct` and `current_password` is upstream's
`AccountMigration` followed by `MoveService`. An account without a password
(signed up through SSO) sends `current_username` instead. It is refused when:

 -  the password or username is wrong (`Current password is invalid`);
 -  the account moved in the last thirty days (`You are on cooldown`);
 -  the target cannot be found — it is always fetched afresh, so that its
    `alsoKnownAs` is current (`Acct could not be found`);
 -  the target does not list this account as an alias
    (`Acct is not an alias of this account`);
 -  it is the account already moved to, or the account itself.

The migration is recorded in `account_migrations` with the follower count at
the time, under the Redis lock `lock:account_migration:{id}`. Then the account
redirects to the target, its local relationships move (below), its profile
`Update` goes out naming `movedTo`, and a `Move` goes to the inboxes of its
remote followers, of the remote accounts that block it, and of the enabled
relays. The Move's id is `{actor}#moves/{migration id}`; its `object` is the
old actor and its `target` the new one.

`POST /api/v1/accounts/redirect` puts up the redirect alone (`Form::Redirect`):
the same challenge, a target that can be found and is neither this account nor
the one already redirected to, no alias needed, nobody moved, no cooldown.
`DELETE /api/v1/accounts/redirect` takes it down; followers already moved stay
moved.

A moved account is meant to be restricted the way upstream's
`User#functional?` restricts it. These endpoints are exactly the ones upstream
leaves open to it (`skip_before_action :require_functional!`), so whatever gate
eunha puts in front of the API has to let them through.


Moves from other servers
------------------------

A `Move` arriving in an inbox is upstream's `ActivityPub::Activity::Move`. It
is ignored unless its `object` is the sender itself; the new account is its
`target`. Each account gets one Move processed a week: the first sets
`move_in_progress:{account id}` in Redis with a seven-day expiry, and a Move
that is refused or fails clears it again. The target is fetched afresh and
must be available and list the sender in `alsoKnownAs`. Then the sender
redirects to it and the relationships move.

Eunha now keeps a remote actor's `alsoKnownAs` and `movedTo` whenever it reads
the actor, as `ProcessAccountService` does; before, it kept neither, and so
believed no Move at all.


What moves
----------

This is upstream's `MoveWorker`, run in the background.

Between two local accounts, the follows themselves are rewritten. Pending
requests to the new account from the old one's followers are approved first.
A local account that already follows both keeps both, and its lists that held
the old account gain the new one. Every other local follower's follow, and the
list memberships on it, are pointed at the new account, and the two accounts'
follower counts move by as many.

Otherwise each local follower follows the new account
(`FollowMigrationService`) with the reblog, notification and language settings
it followed the old one with, past the follow limit, and past a locked account's
approval when the new account is local; its lists gain the new account; and
once the new follow is made or asked for, the old one is ended without clearing
the home feed. A remote new account is followed with a `Follow`, and the old
account is unfollowed as soon as that is queued, not once it is delivered (the
`migrated-follow-unfollows-on-queueing` divergence).

Then, for every local account:

 -  a note about the old account is copied onto the new one under *This user
    moved from …, here were your previous notes about them:*, joined to a note
    it already had there, and dropped where the result would pass 2,000
    characters;
 -  a block of the old account becomes a block of the new one, unless the new
    one is already blocked or followed, with the note *This user moved from …,
    which you had blocked.* if there is no note yet;
 -  likewise a mute, keeping whether it hid notifications.

The notes are written in the author's language where eunha has it (English and
Korean, from upstream's `move_handler.*` strings).
