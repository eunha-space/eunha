Content formatting
==================

Posts, bios and profile fields reach a client as HTML, and eunha writes that
HTML as Mastodon does, in *src/formatter/*. What is stored is what came in: a
local account's text as it typed it, plain, and a remote account's as the HTML
its server sent. Either is formatted every time it is served.


What is formatted where
-----------------------

`HtmlAwareFormatter` picks between two formatters by where the text came from:

 -  Local text goes through `TextFormatter`, which escapes it, links its URLs,
    hashtags and mentions, and wraps it in paragraphs.
 -  Remote HTML goes through `Sanitize` with `MASTODON_STRICT`, which keeps a
    small set of elements and attributes and rewrites the rest.

That is what a status's `content` is, in the API and in the ActivityPub `Note`
eunha serves, and each version in its edit history; an account's `note`, in
the API, in `/api/v1/profile` and as the actor's `summary`; and each profile
field's `value`. A remote field that has been verified is shown instead as a
link to the URL it was verified for. Announcements, and the warning and
announcement emails, use `TextFormatter` alone (`linkify`). A remote
collection's description and a translated post are sanitized with
`MASTODON_STRICT`, and an oEmbed preview with `MASTODON_OEMBED`.

Going the other way, `PlainTextFormatter` turns a remote post into the plain
text that keyword filters match against.


Local text
----------

URLs, hashtags and mentions are found with twitter-text's expressions, as
Mastodon amends them:

 -  A URL needs one of `http`, `https`, `dat`, `dweb`, `ipfs`, `ipns`, `ssb`,
    `gopher` or `gemini` before `://`; `xmpp:` and `magnet:` URIs are found
    too. A bare `example.com` is not linked.
 -  A URL's top-level domain must be on twitter-text 3.1.0's lists, the
    version Mastodon bundles (*src/formatter/tlds.rs* holds them), or be a
    punycode one, and be followed by none of a letter, digit, `@`, `+` or
    `-`. A URL under a domain newer than the lists, or under `.example`, stays
    text, as upstream. Picking a local post's preview card link reads URLs the
    same way.
 -  A link shows at most 30 characters of the URL after its scheme and a
    leading `www.`, the rest hidden in a span with the class `invisible` and
    the cut marked with the class `ellipsis`, so copying the link copies all
    of it. Its `rel` is `nofollow noopener`, with `me` added in a profile
    field.
 -  A hashtag links to `/tags/` and the tag as written, percent-encoded.
 -  A mention links only when the account is known. In a post, the known
    accounts are its author and the accounts it mentions; elsewhere the
    account is looked up by username and domain. The link reads `@username`,
    with the domain added in a profile field or when another of those
    accounts has the same username.
 -  A local post that quotes another starts with
    `<p class="quote-inline">RE: ` and a link to the quoted post, unless the
    text already holds that link, for clients that cannot show the quote.

Text is split into paragraphs at blank lines, and a single line break becomes
`<br />`; a profile field stays on one line.


Remote HTML
-----------

`MASTODON_STRICT` keeps `p`, `br`, `span`, `a`, `del`, `s`, `pre`,
`blockquote`, `code`, `b`, `strong`, `u`, `i`, `em`, `ul`, `ol`, `li`, `ruby`,
`rt` and `rp`. Any other element is dropped and its contents kept in its place,
except `script`, `style`, `iframe`, `svg`, `math` and the like, which go with
everything in them; a block element such as `div` leaves a space either side.
On the way:

 -  classes other than microformats ones (`h-`, `p-`, `u-`, `dt-`, `e-`) and
    `mention`, `hashtag`, `ellipsis`, `invisible` and `quote-inline` are
    removed;
 -  `translate` is kept only as `translate="no"`;
 -  MathML written as FEP-dc88 describes is replaced by its TeX annotation
    between dollar signs, or else its plain-text one;
 -  a heading becomes a paragraph in bold;
 -  a link whose `href` is relative, missing, or of a scheme other than the
    ones above is replaced by its text;
 -  every link gets `rel="nofollow noopener"` and `target="_blank"`.

Attributes keep the order they arrived in. HTML nested more than 400 elements
deep is shown as nothing, as Nokogiri refuses to parse it.


Differences from Mastodon
-------------------------

 -  A domain is checked with UTS 46 IDNA rather than libidn's IDNA 2003,
    which can disagree about a few unusual characters.
 -  A local bio's or field's mentions are looked up wherever an account is
    filled in with its counts — the account endpoints and lists, every status
    list, the profile endpoints and the actor document — and left as text
    where an account is serialized without its counts.
    Announcements in the admin API are likewise linked without looking their
    mentions up.
 -  The quote fallback is not added to versions in a post's edit history.
