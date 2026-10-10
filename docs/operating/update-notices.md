Update notices
==============

Mastodon polls an update server for newer releases and for the end of support
of the branch it runs, and records both in `software_updates` and
`software_deprecations`. Eunha asks the same server the same question about the
Mastodon release *it implements*: eunha builds 4.7.3's schema and serves its
API, so when that branch stops receiving fixes, what eunha reproduces is what
is going out of support.

Eunha asks nobody anything unless `software_update_url` names a server, which
nothing does by default. The notices are shown to the administrators on the
admin software updates page, recorded for a Mastodon that may later boot on the
database, and mailed to the instance's own administrators — who on a hosted
instance cannot act on them, because only whoever runs the binary can take a
release up. Operators who want the end-of-support warning name the server:

~~~~ toml
software_update_url = "https://api.joinmastodon.org/update-check"
~~~~

The request then carries the Mastodon version being asked about and eunha's own
`User-Agent`; it does not claim to be Mastodon. It is asked every half hour, as
Mastodon's `SoftwareUpdateCheckScheduler` asks. A process asks once however
many instances it serves — the question is about the release the binary
implements, so it is the same for all of them — and records the answer in each
of their databases. Instances naming different servers are asked separately,
and the set is read afresh each time, so a reload's arrivals and departures are
picked up. Whoever tracks releases for a fleet is better served by
`mise run mastodon:status`.

`GET /api/v1/admin/software_updates` shows the recorded releases newer than the
one eunha implements, by version, to a role with `view_devops`, as Mastodon's
software updates page does; like that page, it is a 404 while no update server
is configured. The web client's *Software updates* page under `/admin` reads
it, and the dashboard warns about such releases (see
[administration](./administration#the-dashboard)).
