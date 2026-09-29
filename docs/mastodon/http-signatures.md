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
