Eunha
=====

Rust re-implementation of Mastodon, with drop-in database compatibility.

Eunha aims for 100% Mastodon database schema compatibility, so that Eunha can
be a drop-in replacement on top of your existing Mastodon database.

We track the latest Mastodon release, and provide migration path from old Eunha
database schema to updated Mastodon database schema.

It's not Eunha's goal to completely mimic Mastodon's feature set or its
implementation detail, and Eunha may contain behavioral differences.


Try Eunha
---------

With Docker Compose and OpenSSH installed:

~~~~ sh
eunha try --email you@example.com
~~~~

This creates a trial instance and an account-free public HTTPS tunnel for
ActivityPub. It prints the instance URL and owner password. Ctrl-C stops the
trial while retaining its data. [The trial guide](./docs/operating/trying.md)
also covers Tailscale Funnel, ngrok and other tunnels.


Documentation
-------------

The documentation lives in [*docs/*](./docs/index.md) and is published at
<https://eunha.social/docs/>: running an instance, how Eunha tracks Mastodon
and checks itself against it, and the design records for what comes next.

~~~~ sh
mise run docs:dev     # preview it locally
~~~~


Contributing
------------

See [*Contributing*](./docs/contributing/index.md).


License
-------

[GNU AGPL-3.0](./LICENSE).
