Contributing
============

All Mastodon tables should go in the `public` schema, while tables needed for
eunha goes in the `eunha` schema.

Use mise for all tasks. See `mise.toml`.

Use [shadcn/ui] CLI when adding components. Don't hand-roll components.

[shadcn/ui]: https://ui.shadcn.com


Building the web frontend
-------------------------

For a server run directly from the checkout, build its web frontend too:

~~~~ sh
mise run frontend
~~~~

This installs the frontend dependencies and writes `frontend/dist`. Run the
server from the repository root so it can find that directory. See
[Building the CLI](../operating/trying.md#building-the-cli) for the Rust binary.
`eunha try` uses the frontend in its container image instead.


Federation
----------

For all federation related tasks, we use [ojak], and extend it when necessary.

The extension eunha designs for the places ActivityPub scales badly is recorded
in [the protocol extension](../design/protocol.md): the dereference storm a
boost sets off, the absence of backfill, and identity that cannot outlive a
hostname. It is a design record rather than a description of what eunha does
today, and it says which is which.

[ojak]: https://github.com/eunha-space/ojak


Request parameters
------------------

A Mastodon endpoint reads its parameters as Rails' `params` does: the body,
whether JSON, form-encoded or multipart, merged with the query string, whose
top-level keys win (`request_parameters.merge(query_parameters)`). Bracketed
form names nest as Rack nests them, so `poll[options][]` is an array under
`poll` and `keywords_attributes[0][keyword]` a hash by index.

Take a handler's parameters with `Params<T>` (or `NestedParams` for the raw
hash, or `Parts` for a body that may carry files) from
*src/api/mastodon/extractors.rs*, never with axum's `Json` or `Form`, which
refuse the other encodings. All three merge the query string the same way. A
form gives every value as a string, so a field that is not one reads through
the `rails` casts there: `opt_bool` casts as `ActiveModel::Type::Boolean` does
(`"1"`, `"t"` and `"on"` are true, blank is nil), `opt_int` as `to_i`, and
`strings` takes an array, a lone value, or a form's hash by index. Query
structs use the same casts, as `truthy_param?` reads the query string.


Web client navigation
---------------------

Back and forward navigation restore the window's scroll position for each
history entry. New navigation starts at the top. Restoration waits for
asynchronously loaded content and stops when the reader interacts with the page.

Timeline, hashtag, profile post and notification feeds retain their loaded
pages and pagination state when opening a post, so returning can restore a
position beyond the first page without fetching those pages again. These
snapshots are scoped to the history entry, account token and feed parameters,
live only in memory, and retain at most thirty feeds. A fresh visit, such as
choosing Notifications in the sidebar again, shows the newest forty items the
feed held last while it loads current ones; reloading the browser clears the
snapshots. Profile details and pinned posts still refresh on return.


Web client translations
-----------------------

The web client's text goes through [react-intl], as Mastodon's does. Each
message has an id and its English in the code:

~~~~ tsx
const messages = defineMessages({
  saved: { id: 'tabs_bar.saved', defaultMessage: 'Saved' },
  feeds: { id: 'eunha.navigation.feeds', defaultMessage: 'Feeds' },
})
~~~~

Where a message means what one of Mastodon's web client means, it takes
Mastodon's id, so Mastodon's translations of it can be used; its English stays
eunha's own wording. Search the tracked release's
*app/javascript/mastodon/locales/en.json* for one. A message Mastodon has no
counterpart for gets an id under `eunha.`.

The catalogs are in *frontend/src/locales/*:

 -  *en.json* lists every message, as `pnpm run i18n:extract` finds them.
 -  *mastodon/ko.json* is Mastodon's Korean for the ids eunha reuses, written
    by `pnpm run i18n:import-mastodon` from the release *mastodon.toml*
    tracks. It needs a Mastodon checkout at `$MASTODON_REPO` or
    *~/Git/mastodon*. Don't edit it by hand.
 -  *ko.json* is eunha's Korean: every `eunha.` message, and any of
    Mastodon's eunha words differently. It wins over *mastodon/ko.json*.

After changing messages:

~~~~ bash
cd frontend
pnpm run i18n:extract
pnpm run i18n:import-mastodon
pnpm run i18n:check
~~~~

CI runs `i18n:check`, which fails when *en.json* is stale, when *ko.json*
lacks an `eunha.` message, or when a reused id was never imported. A message
missing from a catalog shows in English.

The interface language is the account's `users.locale`, chosen in the account
menu, then the language chosen in the browser, then the browser's own, then
English (*frontend/src/i18n.tsx*). The client is offered in English and Korean
only, and only the sidebar and account menu are translated so far; see
`web-interface-languages` in [divergences](../mastodon/divergences.md). The
browser tests run in English.

[react-intl]: https://formatjs.github.io/docs/react-intl/


Web client tests
----------------

The web client's browser tests live in *frontend/e2e/*. They run against the
Vite dev server, and each test answers the API calls it needs itself, so no
backend is involved. CI runs them on every push along with the typecheck:

~~~~ bash
cd frontend
pnpm run build
pnpm exec playwright test
~~~~

A fake API response should have every field the real one always has. The
client reads them without checking, as it may.
