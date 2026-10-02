HTTP signatures
===============

Outgoing requests are signed with the draft-cavage signatures the network still
runs on, and double-knocked with [RFC 9421] HTTP Message Signatures when a peer
answers 400 or 401 — the order Mastodon 4.7 uses. Inbound requests are verified
either way: a `Signature-Input` alongside the `Signature` selects RFC 9421,
where the covered components must include the body's `content-digest` and the
signature must be fresh, just as the draft path requires a covered `digest` and
a recent `Date` or signed `(created)`; neither may have passed an `expires` its
signer set. Both live in [ojak], which also refuses a request signed for a host
other than the instance's domain or one of its aliases, and a key the signer's
actor document does not publish under the key ID that signed: an RSA
`publicKey`, or for RFC 9421 an Ed25519 Multikey in its `assertionMethod`. A
key ID that is the actor's own `id`, as PeerTube signs, names its `#main-key`.

What gets signed matches Mastodon rather than merely satisfying the spec: the
same covered headers in the same order, `(request-target)` last and carrying
any query string, `Content-Type` bound to deliveries, and no `alg` parameter on
RFC 9421 signatures. Order is not a correctness matter — a verifier rebuilds
from the header list it is given — but emitting what the rest of the network
emits keeps eunha clear of anything that verifies more strictly than it should.

[RFC 9421]: https://www.rfc-editor.org/rfc/rfc9421.html
[ojak]: https://github.com/eunha-space/ojak


Linked Data signatures
----------------------

An HTTP signature says who delivered an activity, which for one a relay passes
on, or a server forwarding a reply, is not who wrote it. Mastodon puts an
`RsaSignature2017` in the activity itself for those: a signature by the
author's main key over the canonical RDF of the activity (URDNA2015), carried
under `signature`, made the moment it is queued and good for two days. Eunha
makes and checks them as Mastodon does, with [ojak]'s `linked_data`, whose
signatures are byte for byte the `json-ld` gem's.

Eunha signs where `Payloadable#serialize_payload` signs: a public or unlisted
status's `Create`, `Update` and `Announce` and its poll's `Update`, an
account's profile `Update`, a `QuoteRequest` and a `FeatureRequest`, unless
authorized fetch is on; and, whatever the mode, the `Delete` of a public or
unlisted status, the `Undo` of such a boost, an account's own `Delete`, and
the `Delete` of a revoked quote's `QuoteAuthorization` ([quotes](./quotes.md)).
Follows, likes, blocks, reports, moves, direct and followers-only posts go
unsigned, as Mastodon sends them. With `sign_integrity_proofs` on, the
FEP-8b32 proof is attached first and the signature covers it, the order
Fedify uses; a verifier of either leaves the other out.

An activity delivered by a server other than its actor's, with no proof that
holds, is taken on such a signature by a key its actor publishes, as
`ActivityPub::ProcessActivityService` takes it; failing that, ojak fetches it
from its own server, and drops it if that does not work either. The
signature covers what the activity means, not how its keys are spelled, so
one taken on it is read as JSON-LD processing reads it, as Mastodon compacts
it before reading it. A post from an account nobody here follows is taken
when it came through an enabled relay (`requested_through_relay?`). Its
contexts have to be ones ojak ships, Mastodon's preloaded ones among them,
since nothing is fetched to read a signature; one naming another context is
not verified, where Mastodon would fetch it.
