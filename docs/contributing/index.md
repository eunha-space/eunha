Contributing
============

All Mastodon tables should go in the `public` schema, while tables needed for
Eunha goes in the `eunha` schema.

Use mise for all tasks. See `mise.toml`.

Use [shadcn/ui] CLI when adding components. Don't hand-roll components.

[shadcn/ui]: https://ui.shadcn.com


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
hash) from *src/api/mastodon/extractors.rs*, never with axum's `Json` or
`Form`, which refuse the other encodings. A form gives every value as a
string, so a field that is not one reads through the `rails` casts there:
`opt_bool` casts as `ActiveModel::Type::Boolean` does (`"1"`, `"t"` and
`"on"` are true, blank is nil), `opt_int` as `to_i`, and `strings` takes an
array, a lone value, or a form's hash by index. Query structs use the same
casts, as `truthy_param?` reads the query string.


Web client navigation
---------------------

Back and forward navigation restore the window's scroll position for each
history entry. New navigation starts at the top. Restoration waits for
asynchronously loaded content and stops when the reader interacts with the page.

Timeline and profile post feeds retain their loaded pages and pagination state
when opening a post, so returning can restore a position beyond the first page
without fetching those pages again. These snapshots are scoped to the history
entry, account token and feed parameters, live only in memory, and retain at
most thirty feeds. A fresh visit loads current posts; reloading the browser
clears the snapshots. Profile details and pinned posts still refresh on return.
