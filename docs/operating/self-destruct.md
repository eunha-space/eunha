Self-destruct
=============

An instance closing for good can leave the federation cleanly, as Mastodon's
self-destruct mode does: it tells every server it knows that each of its
accounts is gone, so that they drop what they cached of them, and meanwhile
refuses nearly everything else. Nothing local is erased. Dropping the database
and the media bucket afterwards is much faster than deleting row by row, so
that is left to the operator.

This cannot be undone. Remote servers delete what they know of the accounts,
but the instance's own data is left as it was, so an instance that went on
serving afterwards would be out of step with everyone else.


Starting it
-----------

`eunha self-destruct` asks for the instance's domain and a confirmation, and
prints the value that turns the mode on (with `--tenants`, pick the instance
with `--instance`):

~~~~ sh
eunha self-destruct
~~~~

Put the value in the instance's configuration and restart it, or reload the
tenants directory, which restarts a changed tenant:

~~~~ toml
[instance]
self_destruct = "…the value printed…"
~~~~

A lone instance also reads it from `INSTANCE__SELF_DESTRUCT` or Mastodon's own
`SELF_DESTRUCT`. The value signs the instance's domain, as Mastodon's does: with
[`secret_key_base`](./instances#mastodon-s-secret-key-base) configured it is
exactly what `tootctl self-destruct` prints for that Mastodon, so a Mastodon
that was already self-destructing keeps doing so on eunha. Without it, the
value is signed under a key derived from the VAPID private key. A value that
does not verify, or signs another domain, does nothing at all.

Run `eunha self-destruct` again to see how far it has got: how many accounts
are still to be announced, whether deliveries are still waiting, whether some
failed and wait for a retry, or that every notice is out and the instance can
be taken down.


What the instance does meanwhile
--------------------------------

Every minute, as Mastodon's `SelfDestructScheduler` does, it takes the next 50
local accounts not yet marked deleted, the instance actor among them, and for
each queues a `Delete` of the actor, signed with its Linked Data Signature, to
the preferred inbox — the shared one if there is one — of every ActivityPub
account it knows, less the servers it no longer delivers to. Each account is
then marked deleted (`requested_deletion_at`) without a deletion request.
Next, it does the same for up to 50 accounts awaiting deletion, removing their
deletion requests. A pass is skipped while more than 10,000 deliveries and jobs
are waiting; Mastodon also waits while Redis is past half its memory, which
eunha's queues, kept in PostgreSQL, do not use.

It runs none of its other schedules (trends, cleanups, the update check), as
Mastodon replaces its whole schedule with this one. The delivery and job queues
keep working, and so does the archive takeout.

Every request is answered with a 410 — `{"error":"Gone"}` to the API, OAuth
and anything asking for JSON, and otherwise a page saying the server is
closing — except what Mastodon still serves:

 -  WebFinger, host-meta, NodeInfo, the OAuth metadata, the manifest, the
    custom CSS, the token and revocation endpoints, the streaming API and the
    web client's static files;
 -  signing in and out, password resets, email confirmation and the security
    key step, the account page and its password change;
 -  the exports and the archive takeout (`/settings/export`,
    `/api/eunha/v1/exports`, `/api/eunha/v1/backups`, `/backups/:id/download`),
    the login history, and the two-factor methods with their security keys.

So members can still sign in and take their data with them. Actor documents
and the inbox answer 410 too, which tells a server that refetches an actor
after its `Delete` that it is gone.
