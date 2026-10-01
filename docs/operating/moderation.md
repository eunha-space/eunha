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
