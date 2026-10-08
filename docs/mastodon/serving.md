What other servers fetch
========================

Eunha serves what Mastodon's ActivityPub controllers serve, at the same
paths, through [ojak]'s federation (*src/api/ap/serving.rs*). A request that
asks for a page rather than ActivityPub goes on to eunha's own routes. Each
route below is served under both of Mastodon's URI schemes,
`/users/{username}` and `/ap/users/{id}`, unless it says otherwise, and names
what it serves by the scheme the account uses.

[ojak]: https://github.com/eunha-space/ojak


Who may fetch what
------------------

In authorized fetch mode every route below needs a signed request
(`require_account_signature!`), except the instance actor, `/actor`, whose
key a peer has to fetch before it can sign. Whoever signed a request is the
reader Mastodon's policies ask about, in either mode. A post, its activity,
and its replies, likes and shares are served as `StatusPolicy#show?` allows
that reader, by an account that is still there: a public or unlisted one to
anyone its author does not block, by account or by domain; a followers-only
one to a follower or an account it mentions; a direct one to an account it
mentions. A quote's stamp is served as its quoted post is. A boost's
activity is its `Announce`, and the boost itself is a `302` to the post it
boosted (`redirect_to_original`). An account's collections
are not there for a signer the account blocks; its featured posts and
hashtags are shown empty to one, in authorized fetch mode, as
`ActivityPub::CollectionsController` shows them.


How long a cache may keep them
------------------------------

Each response says what it varies by and how long it may be cached, as the
controller that serves it in Mastodon says (`vary_by`, `expires_in`):

 -  A response that asks nothing of caches is `private, no-store`, as
    `ApplicationController#set_cache_control_defaults` makes it, and so is
    any that varies by `Signature` and was fetched with one
    (`CacheConcern#enforce_cache_control!`).
 -  The actor, a post, its activity, followers, following, collections and
    their items vary by `Accept, Accept-Language, Cookie`, and by `Signature`
    in authorized fetch mode. Replies, likes, shares, featured posts and
    hashtags, threads and stamps vary by `Signature` in authorized fetch mode
    alone, and an outbox page whenever it is a page. The instance actor varies
    by nothing.
 -  An actor is kept three minutes, publicly unless it was fetched signed in
    authorized fetch mode; a public or unlisted post three minutes (five
    seconds while its quote is pending), and its activity three minutes,
    publicly only then and in public fetch mode; replies, likes and shares
    not at all; featured posts and hashtags, threads, an outbox and a
    collection item three minutes; an outbox page a minute, publicly only
    unsigned; a paged collection's pages not at all; a collection thirty
    seconds after a change and five minutes otherwise; a stamp thirty
    seconds; the instance actor ten minutes. “Publicly” is in public fetch
    mode, unless it says otherwise.


The actor
---------

An actor document is `ActivityPub::ActorSerializer`'s: besides the profile,
it says `webfinger` (`username@domain`), `featuredTags`, `memorial`,
`showFeatured`, `showMedia` and `showRepliesInMedia`, an `interactionPolicy`
saying who may feature the account in a collection without asking (the
account alone when it is not discoverable, its followers when it is locked,
anyone otherwise), `featuredCollections` at
`/ap/users/{id}/featured_collections` whichever scheme it uses, and
`attributionDomains` when it has any. `published` is the day it was created,
at midnight. Its `tag` has the custom emoji of the profile and the hashtags of
its bio, which `update_credentials` keeps in `accounts_tags` as
`UpdateAccountService#process_hashtags` does. `movedTo` and `alsoKnownAs` are
there only when they say something. Its `@context` is folded as
`ActivityPub::Adapter` folds it: `Emoji`, `Hashtag` and `focalPoint` only
when the document has an emoji, a hashtag or an image, and the Multikey
context only when the actor publishes an `assertionMethod`
([integrity proofs](./divergences.md)).

The instance actor, `/actor`, is the same serializer limited to what
`InstanceActorsController` keeps. Its inbox is `/actor/inbox` and its outbox
`/actor/outbox`.


A post's collections
--------------------

A Note names its `replies`, `likes` and `shares`, its `context`, `conversation`
and `atomUri`, as `ActivityPub::NoteSerializer` does, and each is served:

 -  `…/statuses/{id}/replies`: the author's own public and unlisted replies
    first, sixty to a page from `min_id`, then everyone else's
    (`only_other_accounts`) by accounts that are not suspended. A local reply
    is embedded as its Note, a remote one named. The collection embeds its
    first page, and a page is asked for with `page=true`, as
    `ActivityPub::RepliesController` pages it. Its `partOf` is the username
    route whichever scheme the account uses, as Mastodon's is.
 -  `…/statuses/{id}/likes` and `…/statuses/{id}/shares`: how many liked or
    boosted the post, and nothing else.
 -  `/contexts/{account}-{status}` and its `items`: a thread started here,
    named by the post that started it, which a post that answers nothing
    becomes (`Status#update_conversation`), with its public and unlisted
    posts sixty to a page, each by its URI. A context of two numbers with no
    conversation behind it is a 500, as Mastodon fails on it.


An account's collections
------------------------

 -  `…/followers` and `…/following`: newest follow first, twelve to a page
    at `?page=N`, each page saying how many there are, as
    `FollowerAccountsController` and `FollowingAccountsController` page
    them. An account that hides them shows only the count, and refuses a
    page with a 403.
 -  `…/outbox`: its posts and boosts, newest first, twenty to a page, each as
    the `Create` or `Announce` that posted it, as `AccountStatusesFilter`
    picks them for the signer (its followers-only posts to a follower, and
    any that mention the signer). Pages are `?page=true`, `?max_id=…&page=true`
    and `?min_id=…&page=true`, as `OutboxesController` writes them, the last
    being the one up from `min_id=0`.
 -  `…/collections/featured`: its pinned posts, newest pin first, a public or
    unlisted one embedded and any other named.
 -  `…/collections/tags`: the hashtags it features, each linking to its posts
    with the tag, named as it was featured.
 -  `/ap/users/{id}/featured_collections`: its collections, five to a page
    (`?page=`), each page embedding them as FeaturedCollections. A collection
    is at `/ap/users/{id}/collections/{c}` and at `/collections/{c}`, an item
    at `/ap/users/{id}/collection_items/{i}`, and the stamp by which a local
    account consented to being in one at
    `/ap/users/{id}/feature_authorizations/{i}`.

Eunha issues stamps at the `/ap/users/{id}` address, as Mastodon does, and
still answers at `/users/{username}/collections` and
`/users/{username}/feature_authorizations/{i}`, where it named these before;
*divergences.toml* records why.


Featured hashtags
-----------------

Featuring a hashtag sends an `Add` of it, and no longer featuring it a
`Remove`, to everyone the account reaches (`AccountReachFinder`), with a
Linked Data signature outside authorized fetch mode, as
`CreateFeaturedTagService` and `RemoveFeaturedTagService` send them. Their
target is the account's featured posts, as Mastodon's is. Featuring one
already featured sends nothing. A featured tag is counted when it is
created: the account's public and unlisted posts with the tag, and when the
latest was posted.


Collections a post links to
---------------------------

A status's `tagged_collections` in the REST API are the collections its
`tagged_objects` name, as `REST::CollectionSerializer` writes them for the
reader: the collection's owner sees its pending items too. Mastodon 4.7.1
never makes one for a local post (`ProcessLinksService` is never called). A
remote post's are the collections its `FeaturedCollection` tags name, ours
or a known account's, fetched when unknown
(`FetchRemoteFeaturedCollectionService`), as its `Create` is processed and
again when it is edited (`update_tagged_objects!`), which drops those it no
longer names. One whose server could not be reached is tried again half a
minute to ten minutes later (`TaggedCollectionResolveWorker`); one whose
server answered with an error is not.


Followers synchronization
-------------------------

A followers-only post of an account with fewer than 25,000 followers is
delivered with a `Collection-Synchronization` header (FEP-8fcf), as
`ActivityPub::DistributionWorker` asks `DeliveryWorker` to send it: the
author's followers collection, the digest of its followers on the receiving
server (`Account#remote_followers_hash`: the XOR of each follower URI's
SHA-256), and `/users/{username}/followers_synchronization`, where that
server, and only it, signed, may list them.

A delivery that carries the header the other way, from a remote account
whose followers collection it names, is compared with the digest of the
account's local followers (`local_followers_hash`). When they differ, its
list is fetched (`FollowersSynchronizationWorker`, ten pages at most): a
local account it lists that does not follow it here has its follow request
accepted, or sends the `Undo` of a follow eunha never knew of; and once the
whole list is read and adds up to the digest, a local account it does not
list stops following it. The header and the digest are ojak's
(`ojak::synchronization`); Mastodon's `DISABLE_FOLLOWERS_SYNCHRONIZATION`
has no counterpart.
