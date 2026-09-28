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
