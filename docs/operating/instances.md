Several instances in one process
================================

Instances may share an S3-compatible media bucket when every one has a stable,
unique object namespace. Set `media_storage.key_prefix` to prepend that
namespace to every object read, write, delete and public URL. Leave it empty
for the historical dedicated-bucket layout:

~~~~ toml
[media_storage]
bucket = "eunha-media"
key_prefix = "tenants/9bd0de00b92141828d4bd2d36222f70c"
base_url = "https://r2.eunha.space"
~~~~

The prefix is an ownership boundary for object layout, not authorization;
bucket credentials can still access other prefixes in the same bucket.

Every Eunha process serves a registry of instances and hands each request to
one of them by its `Host` header. Run without arguments, it serves the single
instance in `config.toml` and the environment, as it always has, and answers
whatever host it is asked by. Given a directory, it serves one instance per
`*.toml` in it, each answering to its `instance.domain`, and to its
`web_domain` and aliases when it has them:

~~~~
eunha --tenants /srv/eunha/tenants migrate
eunha --tenants /srv/eunha/tenants
~~~~

Each instance keeps its own database, Redis prefix, background tasks and
signing keys; what they share is the process and its routes, built once rather
than per instance. Three things follow from that:

 -  **Tenant files are read on their own.** Environment variables belong to the
    process, so none of them overrides a tenant's file.
 -  **Every tenant must agree on what the process owns:** `bind_address`,
    because there is one listener. Eunha refuses to start otherwise, and when
    two files claim one domain. `allowed_private_networks` is each tenant's
    own: every instance has its own SSRF-guarded client, which reaches only
    the private networks its own file names.
 -  **A tenant that cannot start does not stop the rest.** One whose database is
    behind this binary, or that fails to start, is left out and its host
    answers 503; a host no tenant serves answers 421. A lone instance still
    refuses to start, as it always has.

`eunha --tenants <dir> migrate` migrates every tenant's database, and
`--check` exits non-zero if any of them is behind.

Sharing a process also means sharing its capacity, and these limits keep one
instance from taking more than its share:

 -  **Requests in flight, per instance.** Past `max_concurrent_requests` in
    `[limits]`, an instance's requests are answered at once with 503 and
    `Retry-After` rather than queued, and its neighbours carry on. Unset, a
    lone instance has no limit and one among several has 64. A streaming
    connection counts only while it is being opened.
 -  **Mastodon's [rate limits](./rate-limits), per instance.** Each instance
    counts its own clients' requests under its own key prefix, so one
    instance's clients never use up another's.
 -  **Deliveries in flight, per process.** `process_delivery_concurrency` in
    `[workers]`, 256 by default, caps outbound ActivityPub deliveries across
    every instance, first come first served, so one with a large fan-out
    waits its turn instead of opening thousands of connections. Every
    instance in a directory must name the same value.
 -  **Open files, per process.** Every socket counts against the process's
    limit on open files: deliveries in flight, connections kept for reuse,
    database and Redis connections. A service launchd starts has a soft
    limit of 256 unless its plist sets another, which a fan-out passes, so
    Eunha raises its own soft limit to the hard one at startup — 92,160 on
    macOS — and warns if that is still under 4,096.

And a process refuses to start with more than it can hold, before any instance
is started:

 -  **At most 50 instances**, or `process_max_tenants` in `[limits]`. Every
    instance in a process goes down with it, so this is how many one crash
    may take — a decision about the failure domain, not merely about density.
 -  **Pools that fit their database server.** Eunha asks each PostgreSQL
    server its instances use how many connections it accepts —
    `max_connections` less the reserved ones — and refuses when their
    `database_pool.max_connections` add up to more. Overrun, that budget
    would fail whichever instance happened to ask last. It sees only its own
    process, so processes sharing a server divide it between them with
    `process_database_connections` in `[limits]`.

Every instance in a directory must name the same value for both, and a lone
instance is held to them too.

Everything an instance does is logged inside a `tenant{domain=…}` span — its
requests, its streaming connections, its background queues and every task any
of them starts — so one process's log can be read one instance at a time. A
lone instance's lines carry it too. A task started with Tokio directly would
begin outside every span, so `clippy.toml` refuses `tokio::spawn` and
`spawn_blocking` in favour of `tenants::spawn` and `tenants::spawn_blocking`,
which carry the span along.

Tenants come and go without a restart. Sent `SIGHUP`, a process serving a
directory rereads it: a new file starts its tenant, a removed one stops its
tenant — whose host then answers 421 — and a changed one restarts it, answering
503 until it is back. The rest serve on untouched. A stopped tenant's
background queues finish the batch they are in, for up to 20 seconds, and its
streaming connections are closed so that clients reconnect.

~~~~
kill -HUP <pid>
~~~~

A reload is all or nothing. It is refused, and the running tenants left as they
were, when the directory could not have been started as it stands — a domain
served twice, too many tenants, pools past their budget, no tenants at all — or
when it would change what the process set up when it started: `bind_address`
and `process_delivery_concurrency` take a restart. A
tenant that fails to start does not fail the reload; its host answers 503.

A tenant that could not start, at a reload or when the process started, is tried
again every 15 seconds, so a host that comes up before its PostgreSQL recovers
on its own, as does a tenant waiting on `eunha migrate`. Each kind of failure is
logged once, and the tenant's coming back is logged as
`tenants that could not start have now started`. A retry uses the configuration
of the last start or `SIGHUP`, and never rereads the directory: a file changed
since then, such as one fixing a mistake in the tenant's configuration, still
takes a `SIGHUP`.

This is the start of the shared-process work planned in
the [multitenancy plan]; what sharing a process
saves is measured in [benchmarking](../design/benchmarking.md).

[multitenancy plan]: https://github.com/eunha-space/space/blob/main/docs/multitenancy.md


Handles on another domain
-------------------------

An instance's accounts may have handles on one domain while it is served from
another: `@alice@example.com`, with the server at `social.example.com`. These
are Mastodon's `LOCAL_DOMAIN` and `WEB_DOMAIN`, and Eunha's `domain` and
`web_domain`:

~~~~ toml
[instance]
domain = "example.com"
web_domain = "social.example.com"
~~~~

A Mastodon `.env.production` sets them as `LOCAL_DOMAIN` and `WEB_DOMAIN`,
which Eunha reads too. Without `web_domain` the instance is served from
`domain`, as it always was.

What each one is used for follows Mastodon:

 -  `domain` is in every handle, WebFinger's `subject`, the actor's
    `webfinger`, and the instance API's `domain` (`uri` in version 1). It is
    the name the server goes by on its pages, in emails, in the terms and
    privacy policy (`%{domain}`) and in an authenticator app, and it is in
    mail `Message-ID`s and `List-ID`s and in OStatus `tag:` ids. A mention of
    `@bob@example.com` is of the local bob.
 -  `web_domain` is in every URL: ActivityPub ids, profile and post links,
    hashtag links, the streaming API, OAuth, and the links in emails.
    Passkeys are registered for it.

Eunha answers on both, and on any aliases, so WebFinger for
`acct:alice@example.com` works whichever host it reaches. Most operators serve
something else at `example.com` and send its WebFinger to the instance instead:

~~~~ nginx
location /.well-known/webfinger {
  return 301 https://social.example.com$request_uri;
}
~~~~

As in Mastodon, the pair cannot be changed safely once accounts have
federated: other servers keep the ids they were given, which are on the web
domain, and the handles they found them by. `eunha rename-domain` and
`previous_domains` carry the ids across a change of web domain; see
[account moves](./account-moves.md).

In the tenants directory, a tenant is still known by its `domain`: the web
domain is one more host it answers on, like an alias.


Answering more than one hostname
--------------------------------

An instance may answer additional HTTP hostnames without changing its canonical
ActivityPub identity by listing `aliases` under `[instance]`, as Mastodon's
`ALTERNATE_DOMAINS` does. Handlers continue to emit URLs on the web domain and
handles on `domain`; aliases only affect the shared runtime's Host dispatch,
and which handles WebFinger answers for.

~~~~ toml
[instance]
domain = "garden.eunha.space"
aliases = ["garden.eunha.site"]
~~~~


Site settings are not configuration
-----------------------------------

The site's title, descriptions, contact address and account, registrations,
privacy policy and terms of service live in the database, as Mastodon keeps
them: the server settings an administrator saves, and the published terms.
A new instance starts with Mastodon's defaults (titled `Mastodon`,
registrations closed) until an administrator saves the settings, or a hosting
console writes them.

Older Eunha read `title`, `description`, `short_description`, `contact_email`,
`registrations_open`, `approval_required`, `privacy_policy` and
`terms_of_service` from `[instance]`. They are read no longer, and a server
that finds them set logs a warning. `eunha migrate --tenants <dir>` copies them
into each tenant's database once, as `eunha settings import-config` does, for
every tenant that already had users when the release that stopped reading them
was migrated; each line it prints names the tenant's file (see
[migrations](./migrations#site-settings)). A tenant whose `[instance]` could not
be read then is imported by the next `eunha migrate` that can read it. Run
`eunha settings import-config --instance <host>` by hand for anything else, as
[administration](./administration#settings-and-the-instance-configuration)
describes, then delete the keys. Starting the server never does it.


Authorized fetch and limited federation
---------------------------------------

Mastodon reads these deployment modes from its environment. Instances sharing
a process share its environment too, so Eunha reads them from each instance's
`[instance]` table instead, and a `SIGHUP` reload picks up a change to them:

~~~~ toml
[instance]
domain = "garden.eunha.space"
# Mastodon's AUTHORIZED_FETCH: "none", "actors", "all", or true for "all" and
# false for "none". Unset, the `authorized_fetch` site setting decides, and it
# is `actors` until an administrator changes it.
authorized_fetch = true
# Mastodon's LIMITED_FEDERATION_MODE.
limited_federation_mode = false
# Mastodon's DISALLOW_UNAUTHENTICATED_API_ACCESS.
disallow_unauthenticated_api_access = false
# Mastodon's DISABLE_AUTOMATIC_SWITCHING_TO_APPROVED_REGISTRATIONS (see the
# administration page).
disable_automatic_switching_to_approved_registrations = false
# Mastodon's DISABLE_FOLLOWERS_SYNCHRONIZATION (see followers
# synchronization in serving ActivityPub).
disable_followers_synchronization = false
# Mastodon's EXPERIMENTAL_FEATURES. The one eunha knows is `fasp` (see
# auxiliary service providers); unknown names are ignored.
experimental_features = ["fasp"]
# Mastodon's SELF_DESTRUCT: what `eunha self-destruct` prints (see
# self-destruct). Takes effect when the instance restarts.
# self_destruct = "…"
# Mastodon's DEFAULT_LOCALE: the locale used when nobody has said which they
# want. One of Mastodon's available locales, or else `en`.
default_locale = "en"
# Mastodon's FORCE_DEFAULT_LOCALE: ignore the browser's Accept-Language.
force_default_locale = false
~~~~

`default_locale` is Mastodon's `I18n.default_locale`, and a lone instance
configured from the environment also takes `DEFAULT_LOCALE` itself. It is the
instance's `languages` in both versions of the instance API, the language of
a post that names none, what mail, Web Push notifications, the notes a move
leaves and an email subscription are written in for someone with no locale of
their own, the locale a request names none of, and the language asked for
when fetching a link preview.

`force_default_locale` is Mastodon's `FORCE_DEFAULT_LOCALE`, which a lone
instance configured from the environment also takes, forced only by the value
`true` as Mastodon's check is. When it is on, a request's `Accept-Language` is
not asked: Eunha reads every request as asking for the default locale, so the
locale a web sign-up saves, the language a translation is into, trends, an
email subscription and the server-rendered pages (which speak English or
Korean) all take `default_locale` unless a `lang` parameter or the user's own
locale names another, as `Localized` does.

`experimental_features` turns on what Mastodon keeps behind
`EXPERIMENTAL_FEATURES`; the one Eunha knows, `fasp`, is described under
[auxiliary service providers](./fasp).

Authorized fetch, Mastodon's secure mode, refuses an ActivityPub fetch that
is not signed: actors, statuses and every collection answer 401 to an unsigned
request, and 403 to one signed with a key on a domain this instance does not
federate with, whose key is never fetched. The instance actor and WebFinger stay
open, because a peer has to fetch the instance actor's key before it can sign
anything. A status is not there (404) for a signer its author blocks, or whose
domain the author blocks, and an account's outbox shows such a signer nothing;
in authorized fetch mode its pinned statuses do not either.

Limited federation mode federates only with the domains on the allow list
(`/api/v1/admin/domain_allows`):

 -  an activity from any other domain is dropped, and a fetch signed with a key
    on one is refused, without fetching its key;
 -  accounts and statuses on other domains are never fetched or resolved, and
    no account here can follow one;
 -  authorized fetch is on, whatever `authorized_fetch` and the setting say;
 -  the API needs a signed-in user, as with
    `disallow_unauthenticated_api_access`, and one whose login is unconfirmed,
    pending, disabled or moved is refused everywhere, `/api/v2/instance`
    included;
 -  `/api/v1/instance/*` needs a user too, and the peers, peer search and
    activity APIs answer 404;
 -  `/api/v2/instance` says `"limited_federation": true`;
 -  taking a domain off the allow list suspends its accounts at once, then
    deletes them.

`disallow_unauthenticated_api_access` alone answers 401 to an API request with
no signed-in user, an app's own token included, except for what a client needs
before anyone signs in: `/api/v1/instance` and its subresources,
`/api/v2/instance`, `/api/oembed`, registering an app, signing up, and the peer
search.


Translation
-----------

Mastodon reads its machine translation service from `DEEPL_API_KEY`,
`DEEPL_PLAN`, `LIBRE_TRANSLATE_ENDPOINT` and `LIBRE_TRANSLATE_API_KEY`. Eunha
reads the same settings from each instance's `[instance.translation]` table,
so instances sharing a process can use different services, or none:

~~~~ toml
[instance.translation]
# DeepL, which wins when both are set. The plan is `free` unless it says
# otherwise, and picks api-free.deepl.com or api.deepl.com.
deepl_api_key = "…"
deepl_plan = "free"
# Or a LibreTranslate server. It may be on a private address.
libre_translate_endpoint = "http://127.0.0.1:5000"
libre_translate_api_key = "…"
~~~~

An instance configured from the environment also accepts Mastodon's own
variable names. What is translated, and how, is in
[translation](./translation.md).


Search
------

Mastodon's `ES_*` variables are per-instance settings too, under
`[instance.elasticsearch]`. Instances may share one cluster when each has its
own `prefix`:

~~~~ toml
[instance.elasticsearch]
enabled = true                 # ES_ENABLED=true
host = "localhost"             # ES_HOST, a host or a URL with its scheme
port = 9200                    # ES_PORT
user = "elastic"               # ES_USER
pass = "..."                   # ES_PASS
prefix = "garden"              # ES_PREFIX
preset = "single_node_cluster" # ES_PRESET
ca_file = "/etc/ssl/es-ca.pem" # ES_CA_FILE
query_timeout = "10s"          # ES_QUERY_TIMEOUT
~~~~

A single instance configured from the environment also reads the `ES_*`
variables themselves. A `SIGHUP` reload picks up a change by restarting the
instance; the indexes themselves are created by `eunha search deploy`.
[Search](./search) describes the rest.


Private Prometheus metrics
--------------------------

Metrics are disabled by default. Enable one additional listener per process:

~~~~ sh
# The public tenant listener and private telemetry listener are separate.
eunha --tenants /srv/eunha/tenants \
  --bind-address 127.0.0.1:61000 \
  --metrics-bind-address 127.0.0.1:62000
~~~~

`--metrics-bind-address` accepts numeric loopback addresses only, including
`[::1]:62000`. A non-loopback address or a port already in use fails startup.
The listener serves only `GET /metrics`; it has no tenant dispatch, federation
or public API routes. Scrape using a loopback URL and Host header; a non-local
Host is rejected to protect against browser DNS rebinding. Do not forward the
listener through a public reverse proxy or tunnel. Collect on the same machine,
or use an SSH tunnel when collecting remotely.

Give each blue/green process its own scrape target. Scrape each process
directly so a proxy switching between processes does not mix their metrics.

The initial metrics are:

 -  `eunha_http_requests_total`, with configured canonical `tenant`, matched
    `route`, bounded `method`, and `status_class` labels.
 -  `eunha_http_response_duration_seconds`, a histogram of time until response
    headers, including authentication. It does not measure body transmission
    or the lifetime of a streaming connection.
 -  `eunha_http_in_flight`, requests waiting for response headers per tenant.
    Dropping or cancelling a request releases its count.
 -  `eunha_http_aborted_requests_total`, requests dropped before a response.
 -  `eunha_http_capacity_rejections_total`, requests shed at tenant admission.
 -  `eunha_database_pool_connections` and
    `eunha_database_pool_idle_connections`, open and idle application pool
    connections per tenant. When a pooled database URL is configured, these
    measure client connections to the pooler rather than PostgreSQL backends.
 -  `ojak_delivery_attempts_total`, with canonical `tenant` and bounded
    `outcome` (`delivered`, `failed`, `held`) labels. Successful sends include
    redeliveries after queue acknowledgement failure. A held delivery made no
    HTTP request. These are observations, not unique activities or a durable
    billing ledger.
 -  `eunha_serving_tenants`, the number of tenants currently running.

An alias uses its configured canonical tenant label. Arbitrary Host headers,
raw paths, query strings, credentials and account identifiers are never metric
labels. Unknown routes use `unmatched` and non-standard HTTP methods use
`OTHER`. Pool and in-flight gauges are refreshed every five seconds. Gauge and
histogram series idle for fifteen minutes expire, including those of removed
tenants. Counter series remain for the process lifetime so an idle tenant
cannot silently reset its accumulated sends. Successful-send counters are
initialized to zero for served tenants every five seconds. Process restarts
reset counters, so calculate rates with reset-aware Prometheus functions.
Histogram maintenance runs every five seconds, even without a collector
scraping.

These metrics describe service work, not per-tenant CPU or memory. Those remain
shared-process resources. Other background jobs and media-transfer
instrumentation are not included yet. No Mastodon-compatible responses or
database schemas are changed by enabling the listener.


Email delivery
--------------

Each instance can supply its own `[smtp]` configuration with `host`, `port`,
`username`, `password`, and `from` (the sender email address). Port 465 uses
implicit TLS; port 587 requires STARTTLS. Certificate verification is required.
SMTP is the only email transport. Without SMTP settings, email delivery fails
with a configuration error. Keep tenant configuration
files private because they contain credentials. Reload the tenants directory
after changing email settings.


Mastodon's `secret_key_base`
----------------------------

An instance that replaced a Mastodon can be given that Mastodon's
`SECRET_KEY_BASE`:

~~~~ toml
[instance]
secret_key_base = "…the 128 hex characters from .env.production…"
~~~~

In the environment of a lone instance, `INSTANCE__SECRET_KEY_BASE` or
Mastodon's own `SECRET_KEY_BASE` sets it, so a copied *.env.production* is
enough. With it, Eunha signs and digests what Mastodon 4.7.1 does with it,
byte for byte, and reads what that Mastodon handed out before the switch:

 -  async refresh ids, Rails' `message_verifier('async_refreshes')`;
 -  the unsubscribe links in notification emails and email subscription
    mails, signed GlobalIDs (`to_sgid(for: 'unsubscribe')`) that expire after
    a month;
 -  `users.reset_password_token`, Devise's keyed digest of the token mailed;
 -  the [`self_destruct`](./self-destruct) value,
    `message_verifier('self-destruct')`;
 -  the token in a Web Push's `Unsubscribe-URL`,
    `generate_token_for(:unsubscribe)`, good for 48 hours.

Without it, each of these uses a scheme of Eunha's own (the divergences that
name `secret_key_base`), and what Mastodon issued is not recognised. What
Eunha issued under those schemes keeps working after the secret is
configured, so it can be added at any time. Changing or removing it
invalidates whatever was signed with it, as rotating `SECRET_KEY_BASE` does
on Mastodon; it also signs Mastodon's sessions, so keep it as secret.

*scripts/rails\_signing\_vectors.rb* prints what Mastodon makes from a given
secret, using nothing but Ruby's standard library, for checking an instance
against its Mastodon.


Hosting growth decisions
------------------------

A hosting control plane may supply these optional decisions per tenant:

~~~~ toml
[limits]
registrations = false
media_uploads = false
valid_until = 2000000000 # Unix seconds; the control plane renews the lease
~~~~

Absent flags allow growth. `registrations = false` closes new signups,
including invited signups, without changing the saved registration setting.
`media_uploads = false` returns 422 for new local media attachments, replacement
thumbnails, profile avatars/headers and custom emoji uploads. Descriptions,
deletions, existing media, federation and remote media fetching continue.
Database size and delivery counts cause no automatic restriction.

SIGHUP applies only these decisions in place: a tenant keeps its database pool,
streaming connections and background jobs. Other configuration changes still
follow the usual reload rules. A restriction whose `valid_until` is in the past
allows growth again, even while its old TOML file remains on disk. A standalone
operator may omit the expiry for a deliberately persistent manual restriction;
a hosting control plane should always lease a restriction from a fresh reading.
The control plane owns allowances, warnings, grace periods and billing policy;
Eunha only applies the decision. See `leased-hosting-growth-limits` in the
divergence registry.
