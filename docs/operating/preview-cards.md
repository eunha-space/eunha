Preview cards
=============

A post that links somewhere gets a preview card: the page's title,
description and image, shown under the post. Eunha builds cards as
Mastodon's `FetchLinkCardService` does, into the same `preview_cards` and
`preview_cards_statuses` rows, so a Mastodon booted on the database shows
the same cards, and the same images.


Which link
----------

A local post's card comes from the first URL in its text, found with the
URL expression Mastodon uses for counting characters. A remote post's comes
from its FEP-8967 `Link` attachment when it has one, and otherwise from the
first link in its content that is not a hashtag or a mention. Links to the
instance itself, and posts with media or a quote, get no card. A remote
post's fetch waits up to a minute, at random, so that a link many servers
see at once is not fetched by all of them at the same moment.

Editing a post's text forgets its card and fetches one again.


Fetching
--------

At most one fetch of a URL runs at a time, under the Redis lock
`lock:fetch:{url}`. A card already known is used as it is unless it is two
weeks old or has no image, in which case the page is fetched again.

The page is asked for as `text/html`, with `Accept-Language: en, *;q=0.5`,
and a `User-Agent` ending in `Bot`. Only a `200` answered with `text/html`
is read, and only its first megabyte. Redirects are followed, and the card
is kept under the URL they end at; `preview_cards_statuses.url` keeps the
URL the post gave, which is what the API shows as the card's `url`. Every
request goes through the same client as federation, so a page or image on a
private address is not fetched.

A page that names an oEmbed endpoint in a `<link>` is described by the
endpoint: a link or a video, or a photo. The endpoint is remembered for the
page's domain for a day, in Redis under `oembed_endpoint:{domain}`, when its
query names the page. Rich embeds are ignored, since they rely on scripts.
Otherwise the card comes from the page itself: its JSON-LD (`NewsArticle`
or `WebPage`), its OpenGraph and Twitter tags, and its `<title>`. A page
with neither a title nor a player makes no card. A canonical URL on another
host is not believed.

The language is kept only when Mastodon recognises it. A `twitter:player`
makes the card a video, with an `<iframe>` for its player. HTML in a card is
sanitized as Mastodon sanitizes oEmbed: only `audio`, `iframe`, `source` and
`video`, with `http` and `https` sources, and every frame sandboxed.


Images
------

A card's image is downloaded, up to 8 MB, if it is a JPEG, PNG, GIF or
WebP. One of more than 640×360 pixels' worth is shrunk to that many pixels,
keeping its shape, and a GIF is turned into a JPEG under its original name.
The image is stored, with a blurhash, at
`cache/preview_cards/images/{id}/original/{name}`, where Mastodon keeps it.
A link card takes the stored image's size. An image that cannot be fetched
or read leaves the card without one rather than leaving it unsaved.


Authors
-------

A page's `fediverse:creator` names the account that wrote it. The account
is looked up, over WebFinger if it is not yet known, and becomes the
card's author when it lists the page's domain, or a domain above it, among
its attribution domains, or when the publisher is approved for trends. A
local account that does not list the domain is recorded as the unverified
author, and sees `missing_attribution` on the card, as a prompt to add it.


Differences from Mastodon
-------------------------

 -  Mastodon guesses a page's character set with ICU before believing the
    `Content-Type`. Eunha takes a byte-order mark, then UTF-8 when the bytes
    are UTF-8, then the header's charset, then a `<meta charset>`, and only
    then guesses.
 -  URLs are normalized by the WHATWG URL standard rather than Addressable,
    which can differ in how a non-ASCII path is written.
 -  A remote edit fetches the card again when its text changed, where
    Mastodon also does when its warning, media or poll did.
 -  A WebP that has to be shrunk is stored lossless.
