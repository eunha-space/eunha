Protocol extension
==================

This file records design decisions for an ActivityPub extension aimed at the
places the protocol scales badly. It is a design record, not a description of
what eunha does today: most of what follows is unbuilt, and the sections say
which is which. Nothing here is a divergence from Mastodon in the
`divergences.toml` sense — those are behavioural differences inside the API
eunha reproduces, whereas this is a layer above it that degrades to plain
ActivityPub for peers that ignore it.

The rule the rest of the file follows: an extension must be invisible to a peer
that does not implement it. Every mechanism below is an optimisation two
consenting servers can take, never a requirement for federation to work.


The problem
-----------

When an account with a large following boosts a post from a small instance, the
small instance is hit by the rest of the network at once. The mechanism is worth
stating precisely, because the expensive part is not where it is usually
assumed to be.

An `Announce` carries the *URI* of the boosted object, not the object itself
(`src/federation/activity.rs`, and Mastodon does the same). Receivers verify the
HTTP Signature against the *booster's* key, which never touches the origin. What
touches the origin is the dereference: every instance that does not already hold
that status fetches it.

Two corrections to the usual telling:

 -  The amplification is *per instance*, not per follower. Two hundred thousand
    followers spread over eight thousand instances is eight thousand fetches,
    and `sharedInbox` has already collapsed the delivery side.
 -  The object fetch is cheap; media is not. Mastodon downloads and re-hosts
    every attachment it sees, so one four-megabyte image boosted across eight
    thousand instances is some thirty gigabytes of egress from a server that
    has no reason to be provisioned for it. Signature verification is a
    rounding error beside it.

The root cause of the dereference is that an ActivityPub object does not
authenticate itself. An HTTP Signature authenticates the *connection an activity
arrived over*, which says nothing about a copy that was relayed, so a receiver
that wants to know bob wrote something has no choice but to ask bob's server.

Two further problems come from the same direction and are worth naming, because
a fix for the first does not touch them:

 -  **No backfill and no reconciliation.** There is no way to ask for everything
    an actor published since a point in time, and no way to detect that you have
    silently diverged. This is why a thread looks different on every instance
    and why replies go missing. In practice it hurts users more than raw load
    does.
 -  **Cost scales with the union of what an instance's users follow**, not with
    its user count. A five-user instance following widely pays a
    disproportionate share, which pushes the network toward consolidation. This
    one is economic rather than technical and no protocol change fixes it.


What already helps
------------------

Some of the cost is Mastodon's architecture rather than ActivityPub's design,
and eunha already avoids parts of it. These exist today.

 -  **Remote media is not re-hosted.** Ingest stores `remote_url` and serves the
    origin's URL to clients (`src/api/ap/inbox/create.rs`). Mastodon downloads
    every attachment it federates. This is the single largest difference in
    running cost, and it trades many server-side downloads for client-side ones
    — usually a large win, but not unconditionally, and it is currently
    undocumented. It needs a `divergences.toml` entry.
 -  **Home feeds are populated lazily.** `src/feed.rs` gates on
    `is_feed_populated` rather than fanning out to every follower on write.
    Mastodon writes into every follower's Redis list whether or not that account
    has logged in this year.
 -  **Per-account Ed25519 keys already exist.** `src/federation/keypair.rs`
    provisions them in `keypairs` under `#ed25519-key` and publishes them as a
    FEP-521a `Multikey` under `assertionMethod`.
 -  **FEP-8b32 object integrity proofs are signed and verified.** Outgoing
    activities carry an `eddsa-jcs-2022` proof when `sign_integrity_proofs` is
    on; inbound verification falls back to the proof when the HTTP Signature
    does not check out (`src/api/ap/inbox.rs`). Mastodon 4.7 verifies these and
    produces none.

The last two matter most here: the cryptographic groundwork for everything below
is already in the database and already on the wire.


Design decisions
----------------

### Objects authenticate themselves

An object carries a proof signed by its *author's* key, so any server can relay
it and any receiver can verify it without contacting the origin. This is the one
change that removes the dereference storm, and FEP-8b32 already specifies it.

The consequence for boosts: an `Announce` carries the full signed object rather
than its URI, receivers verify against the author's key, and the origin serves
nothing. Receivers that do not understand the embedded form fall back to the URI
they were going to fetch anyway, so this is safe to emit unilaterally.

### JSON-LD in shape, never in processing

Three separate things get called JSON-LD and only one of them is dangerous.

The `@context` key is a namespacing convention. Eunha emits it, inlines its term
definitions (`src/api/ap/note.rs`), and never resolves it. This stays.

JSON-LD *processing* — expansion, compaction, remote context resolution — eunha
does not do, and will not. Fetching a context at verification time is an SSRF
surface, an availability dependency on someone else's web server, and a source
of nondeterminism in a security path.

Feder's inbox normalises every activity by default; eunha asks it not to
(`read_inbox_as_written`) and reads the activity as its sender wrote it. The
cost was measured as well as argued: with normalisation, a burst of 1,000
activities a second spent more than half of eunha's CPU on it (see
[benchmarking](./benchmarking)).

JSON-LD *canonicalisation* for signatures (URDNA2015/RDFC) is the dangerous one:
graph normalisation as a signature input is where the LD Signature forgery bugs
came from. Eunha signs with JCS (RFC 8785) via the `eddsa-jcs-2022` cryptosuite,
which is what the `-jcs-` in the name means and what Mastodon 4.7 verifies.
`RsaSignature2017` is tolerated on inbound documents and never produced.

Extension terms are defined inline in `@context`, never hosted. Publishing a
context document at a eunha URL would mean other implementations' processors
fetch a file from us indefinitely, and any downtime would silently change how
our posts are interpreted on servers we never hear from. Pure liability.

Endpoints that are not ActivityStreams documents — the log below — carry no
`@context` at all and are served as `application/json`. Putting one on a
document that is not meant to be expanded invites a processor to expand it to
nothing.

### Signed bytes are frozen bytes

JCS signs the concrete document, not the semantic graph. Key *order* does not
matter, because JCS sorts. The key *set* and the value *shapes* do: adding a
field, or turning `"x"` into `["x"]`, breaks the signature even though a JSON-LD
processor would call the documents equivalent.

Three consequences, all of which bite the embedded-object design specifically:

 -  **Embedded objects must keep their `@context`.** Eunha strips it from
    embedded objects today, which is correct AP practice and asserted in
    `src/federation/activity.rs`'s tests. An object signed while served
    standalone was signed *with* its context, so a proof-carrying embed has to
    keep it. Harmless to receivers, but the convention has to be opted out of
    deliberately.
 -  **Forwarding requires the original bytes.** Ingest normalises into columns
    and anything re-emitted is re-serialised, which will not match what was
    signed. There is no raw-document store today. Forwarding a proof-carrying
    object needs one, and that is the real storage cost of this design.
 -  **Floats are the likeliest interop break.** `focalPoint` and `duration` are
    `f64` (`src/api/ap/note.rs`). JCS's number rule is ECMAScript's
    double-to-string, the fiddliest part of RFC 8785. This wants a round-trip
    test against Fedify, since GoToSocial and hackers.pub are the peers already
    exchanging proofs with us.

### The handle is a nickname, the key is the identity

Identity is a key; the hostname is a location hint and a human-readable label.
Followers follow the key, so a handle change breaks nothing.

This is what makes migration lossless. Today a move carries followers and
abandons posts, because posts are `https://old.example/users/…` and that URL
dies with the host. Objects signed by an account key and addressed by content
survive the hostname; what is lost is the name and the discovery path, not the
history.

Being honest about the limit: **rotation, recovery, and no trusted third party —
pick two.** A raw public key (Nostr) gives no rotation and no recovery. A
DNS-rooted identifier gives both but rents identity from a registrar. An
auditable directory (`did:plc`) gives both at the cost of a trusted party, with
misbehaviour detectable after the fact rather than prevented. A witness quorum
avoids the single authority and requires several organisations to exist, stay
online, and be governed; nobody has shipped it.

Eunha takes the DNS-rooted option with a signed continuity chain: the account
document lives at a hostname, and each `alsoKnownAs` link is signed by the
previous identity key. Losing a domain costs a name and a discovery path, not
the validity of a single post. Recovery is an offline key the user may hold; a
user who does not hold one has chosen instance-dependence, which is what they
have today, so it is not a regression.

### Delegated signing keys

The instance holds a short-expiry key delegated by the account's identity key
and scoped to what a server needs to do. The identity key stays cold. Moving
hosts is issuing a new delegation and letting the old one lapse; posts signed
under a lapsed delegation stay verifiable, because the delegation was valid when
they were signed.

This is also what makes client-side signing survivable. Nostr's UX is what it is
because every client needs the actual private key; a scoped, expiring delegation
means a lost phone is a revocation rather than a lost identity.

Revocation is deliberately **not retroactive.** Proving a key was valid at a
past instant needs a timestamping authority or a transparency log, and we are
not building one. Expiries are short, and a compromise means everything signed
in that window is suspect. This is how the web has treated certificates for
thirty years.

### A hash-chained log, hosted by the instance

Each account has an append-only log: sequence numbers and a hash chain over
signed entries. The instance hosts it — the alternative, everyone running
personal infrastructure, is a fantasy that every deployment of the idea
disproves. Hosting is custody, not authority, which is what delegation above
buys: the host holds bytes and a scoped key, never anything that can redefine
the account.

The log makes reconciliation a solved problem rather than an open one. *Give me
actor X from sequence 4102 to 4190* is a well-formed question with a verifiable
answer, and a gap in the chain is detectable rather than silent. That is the
whole of the backfill problem.

Fanout becomes push-a-notification, pull-the-body: delivery carries *actor X
advanced to sequence N, head hash H*, which is small and safe to drop because
the log makes recovery possible. Bodies are pulled in ranges, or pushed inline
when small — which is safe precisely because they authenticate themselves.

### Media stays the unsolved part

Bytes have to come from somewhere. Content-addressing media means any cache
holding a blob can serve it verifiably, so a peer or relay can absorb the fanout
instead of the origin. That fixes integrity, not availability: somebody still
has to hold the bytes, and if nobody does, the origin is back on the hook. This
is the weakest part of the design and is recorded as such.


What this does not solve
------------------------

Recorded here so that nobody later reads the design as claiming more than it
does.

 -  **Deletion.** Verifiable replication and the right to be forgotten are in
    genuine tension, and content-addressing removes even the polite fiction
    that a delete is enforceable. Tombstones propagate over the log, compliance
    is an operator obligation, and media is encrypted at rest so that key
    deletion is meaningful for the one case where crypto-shredding actually
    works.
 -  **Spam and abuse.** Cheap identity plus cheap replication equals spam. This
    needs reputation, which needs social-graph analysis or stake. Labelling
    should be a first-class composable primitive rather than something the wire
    format pretends to have an opinion about.
 -  **The economics.** Cost still scales with the union of what an instance's
    users follow. An aggregation tier fixes that and concentrates the network;
    the most that can be done is to make the aggregator *untrusted* — signed,
    hash-chained data means a relay can be trusted for availability and latency
    while being unable to forge or silently omit. Centralisation then stays a
    performance choice that can be reversed, rather than a governance capture.
 -  **Latency.** Notify-then-pull costs a round trip. That is the price of not
    embedding everything and it is the right trade.


Where the code goes
-------------------

**A module in eunha first, a separate crate later, and not in feder.**

Feder implements ActivityPub as it is specified: draft-cavage, RFC 9421,
FEP-8b32. The line is not *general versus ours* — we would want other servers to
adopt this — but **specified versus speculative.** This extension has no
published spec and no second implementation, and its shape will move. Code in
feder is delivered to feder's users, and they should not take delivery of our
experiments.

That gives a promotion rule: a piece graduates into feder when something that is
not eunha implements it. A second implementation is what turns a design into a
specification, whatever the document is called.

Feder needs no extension points for this. It is sans-IO — `feder-core` is a pure
state machine and `feder-runtime` is standalone primitives — and composition
already happens in eunha, which tries the HTTP Signature and falls back to the
integrity proof itself (`src/api/ap/inbox.rs`). An extension supplies a third
primitive and eunha composes three instead of two. No hooks, no inversion of
control.

A test that answers the boundary question by compiling rather than by argument:

> If a piece requires *modifying* feder, it is general and belongs in feder. If
> it composes from feder's existing primitives, it is ours.

Feature-gating it inside feder is the wrong tool: Cargo features separate
compilation, not ownership, and they are additive across the dependency graph,
so an unrelated crate enabling one would switch it on for everybody in the
build.

Split the module by purity, the way feder is split. Verification is stateless
and has the subtle, test-worthy logic; storage is Postgres-shaped and boring:

~~~~
verify_chain(&[SignedEntry]) -> Result<(), Gap>     module
append, query, persist, retention                   eunha
~~~~

Keep the module's imports to `feder::*` and std — no `AppState`, no sqlx types
in signatures — so extraction is a `git mv` and a `Cargo.toml` entry rather than
a refactor. When it is extracted, name the crate after the protocol rather than
after eunha: a second implementer should be able to adopt it without adopting
our branding.

**Eunha will not author a FEP for this.** The process carries a political
overhead we are not taking on, and the fediverse's real extensions have largely
spread without it: `toot:`, Litepub, and Misskey's `_misskey_quote` — which
eunha already emits — became de-facto standards by being implemented and
written down, not by being ratified.

What that costs is review. A FEP puts a design in front of people who find its
mistakes before they are set in a wire format. Nothing here replaces that, so
the substitute is to keep every mechanism optional and versioned for long
enough that a mistake stays cheap to correct.

What it does not cost is reach, as long as the writing still happens. A Rust
crate does nothing for GoToSocial or Fedify, and those are the implementations
that already verify our proofs and are the likeliest to adopt anything built on
top. The deliverable for them is a normative section in this repository — exact
JSON shapes, `@context` terms, verification algorithm, behaviour on failure —
versioned alongside the code implementing it. This file is a design record; that
would be a specification, and it is a different document.

The namespace matters more without a FEP, because nothing else hands us an
identifier. Choose one named after the protocol rather than after eunha, keep it
stable, and define its terms inline the way `note_context()` already does.


Sequence
--------

1.  **Sign the `Move`.** Account migration is currently two hostnames agreeing
    with each other: `src/api/ap/inbox/moderation.rs` verifies a move by
    fetching the new account and checking `alsoKnownAs` contains the old URI.
    If the old host is gone or hostile, the move is unprovable or forgeable.
    Signing it with the account's existing Ed25519 key makes it an attestation
    the *account* made, verifiable by anyone, without either server being
    reachable. It needs no new key infrastructure, degrades to today's
    behaviour for peers that ignore it, and is the first link of the continuity
    chain.
2.  **Raw-document store.** Required before any proof-carrying object can be
    forwarded. `eunha` schema, since it is not a Mastodon table.
3.  **Proof-carrying embedded `Announce`**, with `@context` retained on the
    embedded object and a JCS round-trip test against Fedify.
4.  **The log.** The ambitious half, and the half that needs storage and a
    delivery-format change. Do the first three before deciding its shape.

Write the specification for whichever of these survives contact with a second
implementation.
