Long posts
==========

The web client limits how much of a post's text it draws before the reader asks
for the rest. The same limit applies wherever a post appears: timelines,
notifications, search results, profiles, conversations, quoted posts and a
post's edit history. Only the focused post of a thread is drawn whole, because
that is where a reader goes to read one in full.

A post's text is cut in two ways, and **Read more** lifts both at once:

 -  *Height.* Text taller than 35 of its lines is cut to 35, with its last
    lines faded out, as in Mastodon's web client.
 -  *Length.* Past 5,000 characters, only the first 5,000 are drawn, whatever
    the text's height, and the rest is neither parsed nor laid out until the
    post is opened. A height cut alone would still have the browser build
    every character hidden under it.

**Show less** cuts the text again. In the composer, a post being quoted is
previewed in six lines, as in Mastodon.


Why a length limit
------------------

Mastodon limits a local post to 500 characters, but a remote server can send
far more. An inbox accepts an activity of up to a megabyte, and posts made to
flood timelines come close to that: a whole book in the text, wrapped around
thousands of mentions. Drawing such a post in full in every timeline and
notification it reaches would slow the page for everyone who sees it, so the
limit applies before any of it is rendered.

Where Mastodon's web client offers **View full post**, which goes to the post,
**Read more** here opens it in place.

The limits are in *frontend/src/components/post-content.tsx*.
