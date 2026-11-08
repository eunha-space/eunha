HTTP signatures
===============

Eunha defaults to draft-cavage for outgoing federation requests, matching
Mastodon 4.7. It can also send and verify RFC 9421 signatures. See Ojak's
guides to [sending] and [the inbox] for signature negotiation and verification.

For personal inbox POSTs, Eunha checks that the local account exists and
can receive federation without rendering the actor or recording an actor
fetch. Duplicate delivery checks are scoped to each inbox.

Eunha resolves delivery destinations before stripping blind addressing
(`bto` and `bcc`) and attaching document signatures. Ojak also checks
embedded portable object proofs before dispatching incoming activities.

[sending]: https://ojak.dev/guide/sending
[the inbox]: https://ojak.dev/guide/inbox


Linked Data signatures
----------------------

Eunha uses Mastodon-compatible Linked Data signatures (`RsaSignature2017`)
so relays and other forwarding peers can pass along verifiable activities.
Eunha signs when it queues the activity, with a two-day lifetime.

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

For forwarded activities, Eunha checks the Linked Data signature before an
FEP-8b32 proof, matching Mastodon's forwarding behavior. If neither verifies,
Ojak tries the original at the activity's origin. An enabled relay can
deliver posts from accounts nobody on the instance follows.

Eunha enables bounded remote context loading for Linked Data verification
and caches those contexts in Redis. An activity accepted on its Linked Data
signature is read in its verified, normalized form. See [the inbox] for
verification details and [Shared Redis](../operating/redis.md) for cache
configuration.
