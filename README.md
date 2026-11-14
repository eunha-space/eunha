eunha
=====

Rust re-implementation of Mastodon, with drop-in database compatibility.

eunha aims for 100% Mastodon database schema compatibility, so that eunha can
be a drop-in replacement on top of your existing Mastodon database.

We track the latest Mastodon release, and provide migration path from old eunha
database schema to updated Mastodon database schema.

It's not eunha's goal to completely mimic Mastodon's feature set or its
implementation detail, and eunha may contain behavioral differences.


Try eunha
---------

The easiest way to try eunha is to [sign up at social.eunha.social][signup],
a Mastodon-compatible instance that runs on eunha. Any Mastodon app works
with it, and it federates with the rest of the fediverse.

To run a trial instance of your own instead, see [the trial
guide](./docs/operating/trying.md).

[signup]: https://social.eunha.social


Documentation
-------------

The documentation lives in [*docs/*](./docs/index.md) and is published at
<https://eunha.social/docs/>: running an instance, how eunha tracks Mastodon
and checks itself against it, and the design records for what comes next.

~~~~ sh
mise run docs:dev     # preview it locally
~~~~


Contributing
------------

See [*Contributing*](./docs/contributing/index.md).


License
-------

[MIT](./LICENSE).

The files copied from Mastodon keep Mastodon's license, the GNU AGPL-3.0:
the schema snapshots in *mastodon/*, and the terms of service and privacy
policy templates in *src/templates/*.
