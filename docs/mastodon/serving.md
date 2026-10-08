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
key a peer has to fetch before it can sign. A post, and its replies, likes
and shares, is served only when it is public or unlisted, by an account that
is still there, to a signer its author does not block (`StatusPolicy#show?`
for a reader who may be anyone). An account's collections are not there for
a signer the account blocks; its featured posts and hashtags are shown empty
to one, in authorized fetch mode, as `ActivityPub::CollectionsController`
shows them.


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
    posts sixty to a page, each by its URI.


An account's collections
------------------------

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

Eunha still answers at `/users/{username}/collections` and
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
`tagged_objects` name, as `REST::CollectionSerializer` writes them. Mastodon
4.7.1 never makes one for a local post (`ProcessLinksService` is never
called). It makes a remote post's as its `Create` is processed, resolving an
unknown collection later (`TaggedCollectionResolveWorker`); eunha does not
yet, so it shows only the ones a Mastodon on the same database made.
