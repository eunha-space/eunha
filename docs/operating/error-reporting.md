Error reporting
===============

What goes wrong in an eunha process is written to its log, which only helps
whoever reads it. The process can also count what goes wrong, and send it to
whoever runs it. When the instance is [hosted](./hosted.md), that is the
hosting provider, not the instance's own administrators.

Two things are reported:

 -  every error the log records, such as a request answered 500 or a tenant
    that could not start;
 -  a warning an operator has to act on although nothing failed outright,
    such as a member's TOTP secret that the instance's encryption keys cannot
    open.

Each one names the tenant it happened in, and a `kind`:

| `kind`                 | What happened                                                         |
| ---------------------- | --------------------------------------------------------------------- |
| `internal`             | A request failed in a way that is eunha's fault, and was answered 500 |
| `database`             | A database error answered 500                                         |
| `secret_undecryptable` | A stored secret the configured encryption keys do not open            |
| `encryption_keys`      | The [key check](#the-encryption-key-check) found keys that do not fit |
| `tenant_not_started`   | A tenant could not start, and its host answers 503                    |
| `unclassified`         | Any other error                                                       |
| `other`                | A `kind` not in this list, kept out of the metric's labels            |


Counting
--------

With the [private metrics listener](./instances.md#private-prometheus-metrics)
on, `eunha_errors_total` counts what is reported, labelled with `tenant`,
`level` (`error` or `warning`) and `kind`. An error outside every tenant, such
as one from the process starting, has the tenant `none`. Alert on its rate;
it needs nothing outside the machine.


Sentry
------

Given a DSN, the process also sends each report to a server that speaks
Sentry's protocol: Sentry itself, a self-hosted Sentry, or GlitchTip.

~~~~ sh
eunha --tenants /srv/eunha/tenants \
  --sentry-dsn https://key@o0.ingest.sentry.io/0
~~~~

`SENTRY_DSN` in the environment does the same. Without either, nothing is
sent anywhere. The DSN belongs to the process, not to a tenant, so it is a
command-line option rather than part of a tenant's configuration: every
instance the process serves reports to the same project, each event tagged
with its `tenant` and `kind`. The release is eunha's version.

An event carries its message, its level, the fields it was logged with and
the error behind it, with that error's chain of causes. It carries nothing
that identifies a person: no IP address, cookie, header or request body.
Members appear only by their numeric id, as `user_id`. Nor does an event
carry the log lines that led up to it, because one process serves several
instances and those lines could be another instance's.

Mastodon has no Sentry integration of its own; this is a recorded divergence
(`error-reporting`).


The encryption key check
------------------------

Mastodon encrypts two columns with its ActiveRecord encryption keys:
`keypairs.private_key` and `users.otp_secret`. Keys that do not fit fail
quietly. eunha signs with every key it encrypted itself, while each member
who set up TOTP before is told their code is wrong. That is what happens to an
instance [imported](./importing.md) with new keys instead of its Mastodon's.

So when a tenant starts, and when `eunha migrate` runs, eunha opens the five
oldest and five newest values of each column. Any it cannot open are logged
as an `encryption_keys` error, which goes wherever errors go. `eunha migrate`
also prints a warning naming the column. The fix is the keys the values were
sealed with. Without them, the affected members' TOTP has to be turned off
with `eunha accounts modify --disable-2fa` and set up again.
