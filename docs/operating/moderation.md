Moderation
==========

Moderation follows Mastodon's admin API: the same endpoints, the same
permission checks, and the same rows written to the same tables. A database
moderated from eunha reads the same in Mastodon's admin interface, and the
other way round. Eunha's web client has a moderation section under `/admin`
built on that API alone. Anything Mastodon offers only in its own
server-rendered admin pages, eunha does not have yet.


Who may do what
---------------

Each endpoint asks for the permission its Mastodon policy asks for, judged by
the role's `computed_permissions` (see [invites](./invites) for how the
everyone role and `administrator` feed into that). A disabled user has no
permissions at all. Some examples:

| Permission          | Bit       | What it opens                                 |
| ------------------- | --------- | --------------------------------------------- |
| `manage_reports`    | `1 << 4`  | reports, and acting on accounts from them     |
| `manage_federation` | `1 << 5`  | domain blocks and allows                      |
| `manage_blocks`     | `1 << 7`  | IP, email domain and canonical email blocks   |
| `manage_taxonomies` | `1 << 8`  | hashtags and trends review                    |
| `manage_users`      | `1 << 10` | accounts: approving, enabling, lifting limits |
| `delete_user_data`  | `1 << 19` | erasing a suspended account's data now        |

A role position decides nothing by itself. What a role may do comes from its
permission bits alone. Acting against an account (warning it, limiting it,
suspending it, freezing its login) also needs the actor's role to be
positioned above the target's role. So a moderator cannot act on an admin, or
on itself. A token needs the `admin:read` or `admin:write` scope as well, or
the narrower `admin:read:accounts` and similar scopes.


Account actions
---------------

`POST /api/v1/admin/accounts/:id/action` does what Mastodon's
`Admin::AccountAction` does. It takes a `type`: `none` (a warning),
`disable`, `sensitive`, `silence` or `suspend`. Optional fields are a
`report_id`, a `warning_preset_id`, a `text`, and `send_email_notification`.
Each action:

 -  records a strike in `account_warnings`, citing the report's posts;
 -  writes an `admin_action_logs` entry for the action, and one for each
    report it resolves;
 -  resolves the report it came from, if it is a warning, and otherwise every
    open report about the account;
 -  for a local account, sends a `moderation_warning` notification, and an
    email unless `send_email_notification` is false.

`disable` freezes the login (`users.disabled`) and leaves the account
visible; `POST …/enable` undoes it. `suspend` hides the account and records
a deletion request. Thirty days later the data is erased, unless
`POST …/unsuspend` came first. Only a suspension made on this instance can be
lifted here. Suspending an account also does the following:

 -  for a local account, sends the new state of the actor to every server
    that knows the account;
 -  for a remote account, makes it stop following local accounts. This cannot
    be undone.

A remote server marking one of its own actors `suspended` suspends it here
too, with a remote origin, and lifting it there lifts it here.

Eunha used to serve `/silence`, `/suspend` and `/sensitive` routes of its
own. They are gone: Mastodon has no such routes, and clients use the action
endpoint.


What a held-back login can still do
-----------------------------------

A token keeps working whatever happens to its user, as in Mastodon; what
changes is what it may be used for.

 -  A suspended or deleting account's token is refused on every API
    endpoint with 403 `Your login is currently disabled`, even one that
    needs no token. Lifting the suspension restores it without a new
    sign-in.
 -  Where Mastodon's controllers run `require_user!`, which is most
    endpoints that act or read on the user's behalf (posting, following,
    the home timeline, notifications, settings), a user who is not fully
    functional gets a 403 that says why:
     -  `Your login is missing a confirmed e-mail address`;
     -  `Your login is currently pending approval`;
     -  `Your login is currently disabled`, for a disabled login, a
        memorial, a moved account, or a role requiring two-factor
        authentication the user has not set up.
 -  Reading public posts, profiles and threads needs no such check, so a
    disabled user's token still reads them.
 -  An application's own token, with no user, gets 422
    `This method requires an authenticated user` where a user is needed.
 -  The streaming API refuses a disabled user's token outright.

A moved account may still undo its redirect and manage its aliases; see
[account moves](./account-moves).


Who may read the public feeds
-----------------------------

The `local_live_feed_access` and `remote_live_feed_access` settings govern
the public timeline, and `local_topic_feed_access` and
`remote_topic_feed_access` the hashtag and link timelines. Each is
`public`, `authenticated` (functional users only) or `disabled` (users whose
role may `view_feeds` only). A feed that is not `public` asks for a
functional user as above; one the viewer may not see is left out, so asking
for both local and remote posts returns only the half the viewer may read.
`/api/v2/instance` reports the settings under `timelines_access`.

The link timeline answers only for a link that is trending and allowed,
and shows only posts by discoverable accounts.


Domain blocks
-------------

An admin domain block covers the domain and its subdomains. When it is
created or changed, it applies to the accounts already known from them, as
Mastodon's `BlockDomainService` does:

 -  *silence* limits each account, as an account action would;
 -  *suspend* suspends each account and purges its data. It records the
    follows this cuts, and tells each local account that lost any with a
    `severed_relationships` notification;
 -  *noop* changes nothing about the accounts, but can still carry
    `reject_media` and `reject_reports`.

An account first seen from a blocked domain starts out limited or suspended.
Changing a block's severity lifts what the old severity did, and removing the
block lifts all of it. A suspend block also stops traffic both ways.
`reject_media` forgets the domain's cached media and custom emoji, and
`reject_reports` drops reports from the domain.

Creating a block for a domain that already has one, or one that is weaker
than a block on a parent domain, fails with a 422. The response carries the
existing block.

`/api/v1/instance/domain_blocks` follows the `show_domain_blocks` and
`show_domain_blocks_rationale` site settings, which live in Mastodon's
`settings` table. Both default to `disabled`, so the list is a 404 until an
administrator sets them. Setting them to `users` shows the list to signed-in
users, and `all` shows it to everyone. Eunha has no settings editor, so
these are set in the database, for example
`INSERT INTO settings (var, value, created_at, updated_at) VALUES ('show_domain_blocks', E'--- all\n', now(), now())`.

Blocks created before this behaviour existed were never applied to accounts
already known. Once their severity is right (see
[migrations](./migrations#domain-blocks-written-before-migration-015)),
saving each one again with a `PATCH` applies it.


Sign-ups and addresses
----------------------

A sign-up passes the same checks it would on Mastodon:

 -  An IP block with `sign_up_block` refuses the sign-up with a 403.
 -  An email domain block covers the domain and its parents. The domain's
    mail exchangers count too, and a domain that resolves to nothing is
    refused, as Mastodon's MX check refuses it.
 -  A canonical email block refuses an address that differs only in case,
    dots in the local part, or a `+tag`.
 -  A username block refuses a username that matches it, either exactly or
    by containing it, depending on the block. Before comparing, both are
    lowercased and digits are read as the letters they stand in for, so
    `4dm1n` matches `admin`.

Blocks of these kinds marked as needing approval (`sign_up_requires_approval`
IP blocks, `allow_with_approval` email domain and username blocks) let the
sign-up through into the approval queue instead. A sign-up through a valid
invite skips the email provider checks. The reason given for joining
becomes the account's invite request, which the admin API shows.

Eunha records the address each account signed up from (`users.sign_up_ip`)
and each password sign-in to the web (`login_activities`), which the admin
API reports and filters by. A `no_access` IP block answers every request
from its range with a 403.

The address is the client's as Rails reads it: the nearest address in
`X-Forwarded-For` that is not a trusted proxy. Loopback and private
addresses are trusted. A proxy anywhere else, such as a CDN, is trusted
once its ranges are listed in the `TRUSTED_PROXY_IP` environment variable,
comma-separated, as on Mastodon.


Reports
-------

`POST /api/v1/reports` stores the category as Mastodon's integer, accepts
`rule_ids` only if they name rules, and attaches only posts the reporter can
see. With `forward`, it sends a `Flag` from the instance actor to the
reported account's server, and to the servers of anyone the reported posts
reply to, as far as `forward_to_domains` allows.

A remote server's `Flag` becomes one report per account it names. The posts
it names are grouped under the account they belong to. A domain blocked with
`reject_reports` is ignored.

Staff whose role carries `manage_reports` get an `admin.report` notification
and an email, the email unless they turned report emails off. They hear
nothing about a second report while an earlier one about the same account
is still open.

Server rules come from the `rules` table. `/api/v1/instance/rules` serves
them, and so do both versions of `/api/v1/instance`. Eunha has no endpoint
for editing them, so they are edited in the database.


Webhooks
--------

Rows in Mastodon's `webhooks` table are called as Mastodon calls them, when
the webhook is enabled and subscribed to the event:

| Event              | When                                               |
| ------------------ | -------------------------------------------------- |
| `account.created`  | a sign-up becomes an account                       |
| `account.approved` | an account is approved                             |
| `account.updated`  | a profile is edited, or a moderator acts on it     |
| `report.created`   | a report is filed, locally or by another server    |
| `report.updated`   | a report is resolved, reopened, assigned or edited |
| `status.created`   | a local post is published                          |
| `status.updated`   | a local post is edited                             |

The request is a POST of `{"event", "created_at", "object"}`. The object is
the admin account, the admin report, or the status entity. Each request is
signed with the webhook's secret in `X-Hub-Signature: sha256=…`, and a
`template` with <code v-pre>{{object.id}}</code>-style placeholders replaces
the body. A failed delivery is retried on Sidekiq's schedule, sixteen times,
but only while the process keeps running. Eunha has no editor for webhooks, so
they are added in the database.


Trends
------

Trends show only what may trend, as Mastodon's `allowed` trends do:

 -  a hashtag that is approved, and usable;
 -  a post that is approved itself, or whose account is approved;
 -  a link that is approved itself, or whose publisher is approved.

Nothing is approved until a moderator says so, unless the
`trendable_by_default` site setting is on. Posts trend only from
discoverable accounts that are neither limited nor marked sensitive. A
post that is a reply, marked sensitive, or behind a content warning does
not trend either. With the `trends` setting off, the public trends are
empty.

Moderators with `manage_taxonomies` see everything that would trend,
whether approved or not, each marked with `requires_review`, at
`/api/v1/admin/trends/{tags,statuses,links}`. The `approve` and `reject`
endpoints there decide. Link publishers are reviewed at
`/api/v1/admin/trends/links/publishers`.

Trends are scored as Mastodon scores them. Each use is counted as it
happens: the distinct people using a hashtag or link each day go into
its history in Redis, and every hashtag, link and post used today is
noted. Every five minutes Eunha rescores what trended before and what
was used today, and keeps the result in `tag_trends`,
`preview_card_trends` and `status_trends`:

 -  a hashtag or link scores once five people use it in a day and more
    use it than the day before. The peak score is kept for two days and
    halves every four hours for a hashtag, every eight for a link;
 -  a post scores once its boosts and favourites reach five, and the
    score halves every hour since it was posted;
 -  each trend's `allowed` is whether it may trend at that moment, so
    an approval or rejection shows at the next rescoring.

The public sees the allowed trends, those in the viewer's chosen
languages, or else the request's language, first; one post per account.
A link trends only from a preview card with a language that Mastodon
recognizes, an article with a title, a description, an image and a
publisher name.

Every hour, unless `trendable_by_default` is on or trends are off, Eunha
looks for trends awaiting review that score above the allowed trend
ranked third in their language. It marks each one as asked about and
mails every moderator with `manage_taxonomies` who has trend emails on.


Changes from earlier versions
-----------------------------

Up to and including migration 015, eunha:

 -  turned a self-deletion into a suspension. It now records
    `requested_deletion_at`, as Mastodon 4.7 does;
 -  treated `/enable` as an unsuspension;
 -  let any role positioned at 100 or above use every admin endpoint.

Report categories and IP block severities eunha stored in its own numbering
are converted by migration 015. Domain block severities cannot be converted
automatically; see
[migrations](./migrations#domain-blocks-written-before-migration-015).

Earlier versions also ranked trends by recent use rather than by score,
and counted hashtag histories from posts. Histories now come from Redis,
so after an upgrade they start empty, and trends build up again only
from the uses that follow.
