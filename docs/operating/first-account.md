The first account
=================

A new instance gets its first account the way a Mastodon server does, from the
command line rather than by signing up. Migrations seed Mastodon's Moderator,
Admin and Owner roles, and `eunha accounts create` is `tootctl accounts create`:

~~~~ sh
eunha accounts create gardener --email gardener@example.com \
  --confirmed --approve --role Owner
~~~~

It prints the random password the account was given. Sign-ups need not be open,
and the registration checks are bypassed: reserved usernames, and, for a
confirmed account, the email domain blocks. The address must still resolve to a
mail server, as Mastodon's `EmailMxValidator` asks. A process serving a tenants
directory names the instance:

~~~~ sh
eunha --tenants /path/to/tenants accounts create gardener \
  --instance garden.eunha.space --email gardener@example.com \
  --confirmed --approve --role Owner
~~~~

The command acts for the instance as its server would, so it needs the
instance's Redis as well as its database. The mail it sends goes into the job
queue, and the running server sends it; an instance with no mail provider yet
can still be given its first account this way, and the mail waits.


What a new account sets off
---------------------------

With `--confirmed`, the account is confirmed straight away. One that is also
approved is then welcomed as any new user is (`User#prepare_new_user!`): a
welcome mail is queued to go an hour later, and everyone whose role may manage
users is told of the sign-up with an `admin.sign_up` notification. An owner
created on an empty instance is the only one who may, and nobody is told of
their own sign-up, so nobody hears of it. An account not yet approved is mailed
to those staff as a pending account instead, until `--approve` approves it.

Without `--confirmed`, the account waits as a Mastodon sign-up does: a `users`
row with no `confirmed_at`, whose owner is mailed the link that confirms it.
The email domain blocks apply to it, as they do to every user not yet
confirmed. Following the link does what `--confirmed` would have done. An
account whose link went out a week ago and was never followed is removed with
the rest of the daily user cleanup, as `Scheduler::UserCleanupScheduler`
removes it.

Without `--approve`, the account is approved exactly when a sign-up would be:
on an instance with open registrations, unless an email domain or username
block asks for approval.


Taking over a username
----------------------

`--reattach` gives the new user the existing account holding the username, when
that account has no user left, as a deleted account does: the account keeps its
id, its actor and its keys, and is no longer marked deleted or suspended. An
account still in use is left alone, with a message saying so, unless `--force`
is also given, which deletes it, its user and its posts, before a new account
takes the name. Unlike `tootctl`, the new user is checked first, so nothing is
deleted for a command that is going to be refused.


Changing an account
-------------------

`eunha accounts modify` is `tootctl accounts modify`:

 -  `--role NAME` gives the user a role, and `--remove-role` takes it away.
 -  `--email ADDRESS` changes the address once the link mailed to the new one
    is followed.
 -  `--confirm` confirms the user, and the address waiting for confirmation
    with it.
 -  `--enable` lets the user sign in again, and `--disable` locks them out.
 -  `--approve` approves a user awaiting approval.
 -  `--disable-2fa` turns the user's two-factor authentication off.
 -  `--reset-password` prints a new random password, and signs the account out
    of every session and app.

As with `tootctl`, `--disable` and `--approve` only set the user's flags: a
disabled user's open streams stay open, and an approved one is not welcomed.
`--reset-password` is `User#change_password!`: it revokes every session,
authorization and token, deletes the push subscriptions made through them,
mails the user that their password changed, and publishes `kill` on each
revoked token's `timeline:access_token:<id>` channel, so that a stream a
running server holds open with one of them closes at once.
