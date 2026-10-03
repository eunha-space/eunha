Remote actors
=============

A remote account's row is written the way Mastodon's
`ActivityPub::ProcessAccountService` writes it, so that a database eunha has
federated on looks to Mastodon as if Mastodon had. Every path that learns about
a remote actor ends in *src/federation/process\_account.rs*:

 -  the actor document ojak fetches to verify a first activity's signature,
    which creates the account (`key_fetched`);
 -  a key that no longer verifies, fetched again: only the keys are refreshed,
    or the whole account when it was last refreshed a day ago
    (`keypair_refresh_key!`);
 -  an `Update` of the actor, from the actor itself;
 -  a search, a mention, a boost or anything else that names an unknown actor
    (`FetchRemoteActorService`);
 -  a `Move`, which fetches the target again (`FetchRemoteAccountService`).


Identity
--------

The account is identified by its `id`, its `uri`. Its handle is taken from the
actor's [FEP-2c59] `webfinger` property, or from `preferredUsername` and the
host of its `id`, and is believed only once WebFinger agrees that the handle
names this actor, following one redirect. A document WebFinger disowns is not
stored; neither is an `Update` whose handle changed and cannot be confirmed.
When the handle is confirmed and differs, the account is renamed, and any other
account holding the handle is left with an invalid one (`! {id}`) and fetched
again. Other accounts with the same `uri` are merged into this one
(`AccountMergingWorker`).

The domain is normalised as `TagManager#normalize_domain` does it: lower case,
an international domain in its ASCII form. New accounts are limited per domain
and per request as upstream limits them: no more than ten new subdomains of one
registrable domain a minute, counted in Redis under
`unique_subdomains_for:{domain}`, and no more than 400 new accounts for one
request, under `discovery_per_request:{id}`.

A portable actor ([FEP-ef61]) has no host to ask WebFinger: its proof by the
key its id names vouches for it, and it is reached at its first gateway.

[FEP-2c59]: https://codeberg.org/fediverse/fep/src/branch/main/fep/2c59/fep-2c59.md
[FEP-ef61]: https://codeberg.org/fediverse/fep/src/branch/main/fep/ef61/fep-ef61.md


Columns
-------

| Column                                                      | From                                                                                                                              |
| ----------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| `inbox_url`, `outbox_url`, `followers_url`, `following_url` | the property, the first of several, an embedded object's `id`; `''` unless an `http(s)` URL                                       |
| `shared_inbox_url`                                          | `endpoints.sharedInbox`, or `sharedInbox`                                                                                         |
| `url`                                                       | `url`, preferring a link whose `mimeType` is `text/html`, only on the actor's own host; the `id` otherwise                        |
| `actor_type`                                                | `type`, the first supported one of several                                                                                        |
| `created_at`                                                | `published`, when given                                                                                                           |
| `feature_approval_policy`                                   | `interactionPolicy.canFeature`, as `InteractionPolicyParser` reads it                                                             |
| `display_name`, `note`                                      | `name` and `summary`, cut at 2,048 and 20,480 characters                                                                          |
| `locked`, `discoverable`, `indexable`, `memorial`           | `manuallyApprovesFollowers`, `discoverable`, `indexable`, `memorial`; `false` when absent                                         |
| `show_featured`, `show_media`, `show_media_replies`         | `showFeatured`, `showMedia`, `showRepliesInMedia`, only when present                                                              |
| `fields`                                                    | the `PropertyValue`s of `attachment`, at most 50, with their `name` and `value` as sent; `{}` when there is no `attachment` array |
| `also_known_as`                                             | `alsoKnownAs`, at most 256 ids                                                                                                    |
| `attribution_domains`                                       | the strings among the first 256 of `attributionDomains`                                                                           |
| `featured_collection_url`, `collections_url`                | `featured`, `featuredCollections`                                                                                                 |
| `avatar_remote_url`, `avatar_description`                   | `icon`: an `Image`'s `url`, and its `summary` or `name`, cut at 10,000 characters                                                 |
| `header_remote_url`, `header_description`                   | `image`, likewise                                                                                                                 |
| `account_stats` counts                                      | the `totalItems` of `outbox`, `following` and `followers`                                                                         |
| `hide_collections`                                          | whether `following` or `followers` has no `first` page                                                                            |
| `moved_to_account_id`                                       | `movedTo`, fetched if unknown; its own `id` marks it as moved to itself                                                           |
| `last_webfingered_at`                                       | now, unless only the keys were refreshed                                                                                          |
| `protocol`                                                  | `activitypub`                                                                                                                     |
| `public_key`                                                | `''`: keys live in `keypairs`                                                                                                     |

The keys are the `Multikey`s under `assertionMethod` ([FEP-521a]) and the
`publicKey`s, at most ten of each, each fetched when it is not embedded under
the actor's own id. They are stored in `keypairs` by their id, with `type`
`rsa`, `ed25519` or `ml-dsa-44`, and those the actor no longer publishes are
deleted. Signatures are checked against a usable RSA key stored under the
signature's key id, then against the legacy `accounts.public_key` of its owner,
which accounts stored before Mastodon 4.7 still carry. When every key changes
and the document did not come signed with a key already held, or the `id`
changed, local followers follow the account again (`RefollowWorker`), and its
tombstones are cleared.

An account suspended by its own server (`suspended: true`) is suspended here,
and unsuspended when the flag goes; one a moderator here suspended keeps that
suspension, and keeps its keys. While suspended, only the protocol attributes
are taken. A domain blocked at the time an account is first seen starts it out
suspended or limited from the time of the block.

The custom emojis in the profile's `tag` are stored by shortcode and domain,
unless the domain is blocked with `reject_media`. A profile or post shows the
emojis of its author's domain, as `CustomEmoji.from_text` looks them up.

[FEP-521a]: https://codeberg.org/fediverse/fep/src/branch/main/fep/521a/fep-521a.md


After storing
-------------

Every save queues the account for the search index (`update_index`), as the
model's callbacks do, and tells subscribed [FASP](../operating/fasp)s about a
new account, or about a changed one while it is discoverable or when it
stopped being.

Unless only the keys were refreshed or the account is suspended, the workers
upstream queues are queued in the [job queue](../operating/jobs.md), with
their own retries and unique locks:

 -  the posts of `featured` are fetched and become the account's pins, and, when
    the actor has no `featuredTags`, the hashtags there its featured hashtags;
 -  the hashtags of `featuredTags`, at most ten, become its featured hashtags;
 -  the collections of `featuredCollections`, at most 50, are stored;
 -  within ten minutes, the profile fields whose value is a bare link to itself
    are verified against the account's `url`, as a local account's are.

A `Create` or `Update` from an account not refreshed for a week schedules a
refresh some time in the next six hours (`schedule_refresh_if_stale!`). The
refresh is `ResolveAccountService`'s: WebFinger is asked about the account's
handle, following one redirect to a handle that names itself, and the actor
its `self` link names is fetched and processed, so an account whose handle now
names another `id` moves to it. An account whose handle was taken from it is
fetched by its `id` instead. A `410 Gone` from WebFinger suspends the account
as its server's doing and queues its deletion, as a `410` to the actor's own
fetch does; any other failure is dropped, except a server that cannot be
reached, which retries the job. The first refresh of an account eunha stored
before this was written fills in everything above.


Differences
-----------

 -  Avatars, headers and custom emojis are shown from where the remote server
    keeps them, never downloaded: the operator chose not to cache remote
    media. When an image's URL changes, the copy a
    Mastodon sharing the database made of the old one is forgotten, so that it
    downloads the new one.
 -  An RSA key published as a `Multikey` is not read.
 -  Hashtags are compared in lower case without the Unicode compatibility
    folding `HashtagNormalizer` applies.
