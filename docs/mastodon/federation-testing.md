Federating with a live Mastodon
===============================

The differential harness asks whether the two servers answer a client the same
way. This one asks whether they can talk to *each other*: eunha's other
federation tests run eunha against eunha, where both sides share eunha's reading
of ActivityPub, so a misreading is invisible. This builds a pair that shares
nothing but the specification.

~~~~
scripts/federation_test.sh
scripts/federation_test.sh --keep      # leave both up to poke at
~~~~

Forty-five checks, both directions: resolve the account, follow, deliver a
status, favourite it, boost it, delete it, and see each land on the other side;
then report an account on the other server with `forward`, and move an account.

**A forwarded report** is a `Flag` from the reporting server's instance actor,
and arrives as a report the receiving server's moderators see. Each side's
admin API is asked for it by the comment it was sent with, which is why
`alice` and `masto` are owners with the admin scopes.

**A move** should take the old account's followers on the other server with it:
Mastodon's `MoveWorker`, which has each of them unfollow the old account and
follow the new one. eunha's `alice` moves to `carol` through eunha's move API,
and Mastodon's `masto` must end up following `carol`; Mastodon's `masto` moves
to `masto2` through a `bin/rails runner` standing in for the settings form,
which has no API, and eunha's `bob` must end up following `masto2`. Each side
must also show the old account `moved`. `alice` has no password, so her move
is confirmed by her username, as an account that signs in some other way
confirms one. The moves come last, because a moved account is restricted
afterwards.

**Both servers run inside the container network.** eunha used to run on the host
behind a host-side Caddy, and that cannot work: `mastodon.test` is a network
alias, so a host process cannot resolve it — eunha could not fetch an actor's
public key and rejected every inbound activity with a 401. In the network they
resolve each other by name, and the harness needs no `/etc/hosts` entry, no
certificate trusted on the host, and no eunha built for this machine. The
certificates are a throwaway CA made with `openssl`, trusted by both sides
through `SSL_CERT_FILE` and by nothing else; the script drives both servers over
plain published ports, which is why nothing on the host has to trust them.

Three things that cost a while, all recorded in the script:

 -  **Rails refuses a request whose `Host` is not its `LOCAL_DOMAIN`** — 403 on
    every endpoint, including those needing no authentication. Since
    `mastodon.test` does not resolve on the host, the only way in is the
    published port with the name supplied by hand. Without it the readiness
    loop spins forever against a Mastodon that is up and answering.
 -  **A follow commits at different moments on the two sides.** The sender
    listing the receiver as a follower is what makes it *deliver*; the receiver
    having committed the follow is what makes it *keep* what arrives, because
    Mastodon drops an activity from an account no local account follows yet.
    Waiting on the sender alone leaves a gap in which a status is delivered,
    accepted with a 2xx, and silently discarded — one run missed by 2.9ms, and
    it read as eunha failing to deliver.
 -  **Mastodon serves an actor from a cache that does not notice it changed.**
    `render_with_cache` keeps an actor document for three minutes under the
    account's `cache_key`, which carries no timestamp, so an alias added after
    eunha first fetched `masto2` was missing from every fetch for the next three
    minutes — the one that checks the `Move` included — and eunha refused a
    move its target did not confirm, as it should. The alias is made when the
    account is, before anything fetches it.

The run also exercises what nothing else did: eunha delivers to Mastodon's
**shared inbox** (`https://mastodon.test/inbox`), not its per-actor one.
Mastodon delivers to eunha's per-actor inbox, which is its own choice when there
is a single recipient on the far side.
