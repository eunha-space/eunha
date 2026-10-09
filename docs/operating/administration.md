Administration
==============

Mastodon's web admin has pages for running the server as well as moderating
it: the server settings, rules, roles, announcements, the servers it federates
with, relays, invites, webhooks, follow recommendations, software updates and
the dashboard. None of them has an API upstream. Eunha serves each over REST
endpoints of its own, at the paths Mastodon's admin API would use, asking for
the permission Mastodon's policy asks for and writing the audit log entries
Mastodon's controllers write. The web client's administration pages under
`/admin` are built on them. They are recorded as
`server-administration-rest-api` in *divergences.toml*; no Mastodon client
calls them. For the moderation half, see [moderation](./moderation).


Server settings
---------------

`GET /api/v1/admin/settings` returns every key of Mastodon's
`Form::AdminSettings`, and `PATCH` saves the keys it is given and no others,
as each of Mastodon's settings pages posts only its own. Both need
`manage_settings`. The values are the ones in Mastodon's `settings` table,
YAML-encoded the same way, so a database edited here reads the same in
Mastodon. The web client splits them across Mastodon's six pages: branding,
about, registrations, discovery, content retention and appearance.

A save is validated as Mastodon validates it, and refused whole with a 422 and
Mastodon's messages, such as
`Validation failed: Registrations mode is not included in the list`. A contact
username must name a local account, and the contact address and username must
both be present once either is given. Saving settings is not logged, since
Mastodon does not log it either.

The thumbnail, mascot, app icon and favicon are uploaded with the settings as
files in a multipart body. Each is a `site_uploads` row, stored where
Paperclip stores it and rendered in Mastodon's styles: the thumbnail at
1200×630 and 2400×1260 with a blurhash, the app icon at each Apple and Android
size, the favicon at 16, 32 and 48 pixels.
`DELETE /api/v1/admin/site_uploads/:id` removes one.

With no thumbnail uploaded, the instance API names the web frontend's own
picture, `/images/preview.png`, and describes it, where Mastodon names and
describes its mascots; with no app icon, it names the frontend's
`/icons/android-chrome-<size>x<size>.png` at each Android size. Both are cut
from the brand symbol by `mise run site:images`. The `icon_url` key of the
instance configuration, which eunha showed before it had uploads, is no longer
read, and a server that finds it set warns.

### What the settings drive

The web client keeps the last loaded instance branding while navigating,
so the sidebar's domain, title and icon stay visible while it refreshes the
instance details.

The desktop sidebar and mobile drawer omit the federated timeline entry for
both visitors and signed-in members. The timeline remains available at
`/public` and as a column.

| Setting                                                          | What reads it                                                                          |
| ---------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| `site_title`                                                     | both versions of `/api/v1/instance`, NodeInfo, the web client's page title             |
| `site_short_description`                                         | `/api/v2/instance`'s `description`, `/api/v1/instance`'s `short_description`, NodeInfo |
| `site_extended_description`                                      | `/api/v1/instance/extended_description`, rendered as Markdown                          |
| `site_contact_email`, `site_contact_username`                    | the instance's contact address and account                                             |
| `site_terms`                                                     | the privacy policy (see [terms of service](./terms-of-service))                        |
| `registrations_mode`                                             | sign-ups, approval of new accounts, the instance API, NodeInfo                         |
| `closed_registrations_message`                                   | `/api/v2/instance`'s `registrations.message` while sign-ups are closed                 |
| `require_invite_text`                                            | a sign-up waiting for approval must give a reason                                      |
| `min_age`                                                        | the sign-up age check (see [account security](./accounts)) and the instance API        |
| `status_page_url`                                                | `/api/v2/instance`'s `configuration.urls.status`                                       |
| `thumbnail`, `thumbnail_description`                             | the instance thumbnail                                                                 |
| `app_icon`, `favicon`                                            | the instance's icons, and the web client's favicon                                     |
| `custom_css`                                                     | `/custom.css`, linked from every page of the web client                                |
| `allow_referrer_origin`                                          | the web client's `Referrer-Policy`                                                     |
| `profile_directory`                                              | `/api/v1/directory`, a 404 when off                                                    |
| `trends`, `trendable_by_default`                                 | trends (see [moderation](./moderation#trends))                                         |
| `*_feed_access`                                                  | the public, hashtag and link timelines                                                 |
| `show_domain_blocks`, `show_domain_blocks_rationale`             | `/api/v1/instance/domain_blocks`                                                       |
| `peers_api_enabled`, `activity_api_enabled`                      | `/api/v1/instance/peers` and `/api/v1/instance/activity`                               |
| `authorized_fetch`                                               | signed fetches, unless the instance configuration decides                              |
| `media_cache_retention_period`, `content_cache_retention_period` | the daily vacuum, below                                                                |
| `wrapstodon`                                                     | annual reports, and `/api/v2/instance`'s `wrapstodon` from 10 to 31 December           |
| `bootstrap_timeline_accounts`                                    | follow suggestions (see [below](#follow-recommendations))                              |
| `noindex`                                                        | the default of each member's `noindex` (see [account security](./accounts))            |
| `backups_retention_period`                                       | archive takeouts are deleted after it (see [import and export](./import-export))       |

The rest are saved for a Mastodon on the same database and read by nothing in
eunha yet: `theme` (Mastodon 4.7 has only the default), `landing_page`,
`mascot`, `preview_sensitive_media` and `captcha_enabled` (eunha has no
CAPTCHA).

### Settings and the instance configuration

The settings are read from the `settings` table alone, as Mastodon reads them;
a setting nobody has saved has `config/settings.yml`'s default: the title
`Mastodon`, blank descriptions and contact, no contact account, and
registrations `none`. As in Mastodon, an invite to a server that is not open
lets its holder sign up, but approves them only if whoever wrote it may bypass
approval (see [invites](./invites)). `site_contact_username` may name a remote
account (`@name@domain`).

Eunha's instance configuration carried the title, descriptions, contact
address, whether registrations are open, the privacy policy and the terms of
service before eunha read the settings, and served them until an administrator
saved the settings. It no longer reads them. An instance upgrading has them
copied into the database once, by the first `eunha migrate` of the release
that stopped reading them (see [migrations](./migrations#site-settings)). The
same import can be run by hand:

~~~~ sh
eunha settings import-config            # --dry-run to see what it would write
eunha --tenants /etc/eunha settings import-config --instance example.com
~~~~

It writes each setting nobody has saved and keeps every saved one, so it is
safe to run again:

| Configuration                                            | Setting                                                      |
| -------------------------------------------------------- | ------------------------------------------------------------ |
| `title`                                                  | `site_title`                                                 |
| `short_description`, or `description` when that is blank | `site_short_description`                                     |
| `description`                                            | `site_extended_description`, `site_description`              |
| `contact_email`                                          | `site_contact_email`                                         |
| `privacy_policy`                                         | `site_terms`                                                 |
| `registrations_open = false`                             | `registrations_mode` `none`                                  |
| `registrations_open = true`, `approval_required = true`  | `registrations_mode` `approved`                              |
| `registrations_open = true`, `approval_required = false` | `registrations_mode` `open`                                  |
| the local account with the highest role                  | `site_contact_username`, the contact eunha showed by default |

and, while nothing is published, publishes `terms_of_service` as the version it
was served as (see
[terms of service](./terms-of-service#terms-from-the-instance-configuration)).
The server never does this. A server whose configuration still sets any of
these keys logs a warning naming them; remove them once imported.

`authorized_fetch` set in the instance configuration, or forced by limited
federation mode, decides whatever is saved; the settings then list it under
`overridden`, and the web client shows it disabled, as Mastodon's form does
when its environment variable is set.

### Registrations close themselves on an idle server

Once an hour each instance runs Mastodon's
`Scheduler::AutoCloseRegistrationsScheduler`. When registrations are `open`
and no one whose role can manage reports has been active for eight days (a
week, and the day a sign-in time may lag), it saves `registrations_mode` as
`approved` and mails everyone whose role can manage settings that it did
(`AdminMailer#auto_close_registrations`). Nothing reopens them: an
administrator who wants open registrations back saves the setting again.
Registrations that are `approved` or `none`, configured or saved, are left
alone.

This changes sign-ups on an instance whose moderators have gone quiet, an
instance that was configured open and never had its settings saved included:
after an upgrade it may start asking for approval within the hour. To keep
registrations open regardless, as Mastodon's
`DISABLE_AUTOMATIC_SWITCHING_TO_APPROVED_REGISTRATIONS=true` does, set in the
instance configuration:

~~~~ toml
[instance]
disable_automatic_switching_to_approved_registrations = true
~~~~

A moderator counts as active by `users.current_sign_in_at` alone, as in
Mastodon. Signing in on the authorization page sets it (and counts the sign-in),
and every authenticated request, eunha's own client's included, moves it again
once it is a day old (`UserTrackingConcern`), keeping the previous one in
`last_sign_in_at`. Such a request also counts a confirmed user towards the day's
logins in Redis (`activity:logins:<day>`), as Mastodon's `ActivityTracker` does,
and has its home feed regenerated when the sign-in before was more than a week
ago (see [accounts](./accounts.md)).

### Activity counts

Eunha keeps Mastodon's `ActivityTracker` counters in Redis, a key per day named
by the day's midnight in UTC, each kept six months: the users who signed in
(`activity:logins:<day>`), sign-ups (`activity:accounts:local:<day>`), local
public and unlisted posts, boosts included (`activity:statuses:local:<day>`),
and interactions (`activity:interactions:<day>`): a new favourite, a boost, a
follow or follow request, a poll vote, and a reply to someone else's post. A
Mastodon process sharing the Redis and the key prefix counts into the same keys.

They are what the numbers that sound like activity are made of:

 -  `GET /api/v1/instance/activity` is always twelve weeks, the first from now
    back seven days, with that week's posts, logins and registrations. It is
    rendered once a day and kept in Redis under Mastodon's own cache key,
    `cache:api/v1/instances/activity/show`.
 -  `usage.users.active_month` in `GET /api/v2/instance`, and the active users
    in NodeInfo, are the users who signed in over the last four weeks (and
    twenty-four, for the half year), cached for ten minutes.
 -  The `active_users` and `interactions` measures of
    `POST /api/v1/admin/measures` add up the days asked for, and the same
    number of days before them.

Counting starts when eunha does: a database imported from Mastodon brings no
history unless its Redis comes along.

### The dashboard's metrics

`POST /api/v1/admin/measures`, `/dimensions` and `/retention` answer what
Mastodon's `Admin::Metrics` classes do, key for key; a key Mastodon does not
have is left out of the answer. The measures are `active_users`,
`interactions`, `new_users`, `opened_reports`, `resolved_reports`, the tag
measures `tag_accounts`, `tag_uses` and `tag_servers` (each with
`<key>[id]`), and the instance measures below; the dimensions are `languages`,
`sources`, `servers`, `space_usage`, `software_versions`, `tag_servers`
and `tag_languages` (with `<key>[id]`), and the instance dimensions. As in
Mastodon:

 -  The range starts no more than two years before it ends, and the period it
    is compared with is the same number of days before it. A counted total
    covers its dates up to midnight of the last one.
 -  Each day of a counted measure is dated `YYYY-MM-DD`, and of one counted
    in Redis `YYYY-MM-DDT00:00:00Z`. Only `instance_media_attachments` has a
    `human_value`.
 -  New users, servers and the languages of posts are counted by the
    snowflake ids of accounts and posts. The local server is listed under its
    domain, and sign-ups through no application under `web`, named in the
    asker's language.
 -  A dimension's `limit` is read as a number; without one, every row is
    listed.
 -  Retention requires both ends of its range, starts no more than 31 days
    (or 12 months) before the end, and gives each cohort's periods as
    `YYYY-MM-DDT00:00:00+00:00` and its rate unrounded.

`software_versions` lists eunha's version string (under `mastodon`),
PostgreSQL's, the Redis-compatible store's (Redis, Valkey or Dragonfly), the
search cluster's when search is on, and FFmpeg's when `ffprobe` runs; there is
no Ruby or libvips to list. `space_usage` lists the database, the store's
memory when the Redis is the instance's alone (see [Redis](./redis)), the
media (attachments, custom emoji, preview cards, avatars and headers, archive
takeouts and site uploads), and the instance's search indexes.

Mastodon caches each answer for five minutes; eunha computes it every time.
Asked without a range, eunha covers the last week, where Mastodon fails.

### Content retention

Once a day each instance runs what Mastodon's `VacuumScheduler` runs for the
retention settings. A period that is not a positive number of days keeps
everything.

 -  With `content_cache_retention_period`, posts from other servers older than
    that many days are deleted, boosts and replies included, whatever local
    users did with them. Private mentions are taken out of their conversations
    first.
 -  With `media_cache_retention_period`, remote media cached longer than that
    is forgotten; its remote URL stays. Link preview images not refreshed for
    that long are forgotten too.
 -  Uploads that were never attached to a post are deleted after a day, as
    Mastodon deletes them. Media waiting on a scheduled post is attached.
 -  The home and list feeds of members who have not signed in for a week are
    removed from Redis, and rebuilt when they return (see
    [accounts](./accounts.md#the-home-feed-while-away)).
 -  Access tokens and authorization grants that have expired or been revoked
    are deleted (`Vacuum::AccessTokensVacuum`).

Eunha does not cache remote media itself, so the media retention matters only
for a database that came from Mastodon. The [maintenance
commands](./maintenance) do the same on demand, with their own thresholds.

### Deleted posts

A post is removed as Mastodon's `RemoveStatusService` removes it, whether its
author deletes it, a moderator does, its server sends a `Delete` (or an `Undo`
of a boost), or its server answers 404 for a public post. It is discarded at
once, with the boosts of it, taken off every feed, stream and featured tag
count, and, when local, deleted on the servers that have it. Then the row is
destroyed with its favourites, bookmarks, mentions, poll, quote, edits and the
notifications about them; only then does it stop counting in its author's
posts and its parent's replies.

A post cited by an unresolved report or by a strike is kept, discarded, for
moderators, and so is a local post a moderator deleted from a report. The
daily user cleanup (Mastodon's `Scheduler::UserCleanupScheduler`) queues a
`RemovalWorker` that destroys every post discarded more than thirty days
earlier. Until then the API, its ActivityPub URLs and oEmbed answer 404 for
it, as for a post that is gone, and as Mastodon answers.

Deleting a post through the API keeps its media for delete-and-redraft: the
attachments are left unattached, for the new post to take, and the daily
vacuum deletes them if nothing does. `delete_media=true` deletes them, and
their files, with the post. A kept post keeps its media.

### Addresses

Once a day each instance runs Mastodon's `IpCleanupScheduler`, which keeps
what is known of people's addresses for a year:

 -  web sessions not used for a year are signed out, their access tokens and
    web push subscriptions with them, and their streams closed;
 -  a session's address, a user's sign-up address (once they have not signed
    in for a year) and the address an access token was last used from are
    cleared a year on;
 -  sign-in attempts older than a year are deleted;
 -  IP blocks whose expiry has passed are deleted.

Mastodon reads the year from `IP_RETENTION_PERIOD` and
`SESSION_RETENTION_PERIOD`; eunha keeps the default year and has no setting for
either.

### Other clean-ups

 -  Every hour, collection items that were rejected or revoked more than a day
    ago are deleted (`CollectionItemCleanupScheduler`), and their collections'
    item counts recounted.
 -  A second after the instance starts, and then daily, remote collections
    stored under an account other than the one they are attributed to are
    fetched again and given to that account
    (`RepairRemoteCollectionsScheduler`); see
    [inbound statuses](../mastodon/inbound-statuses.md#pins-featured-hashtags-and-collections).
 -  Every minute, the posts that members' automated deletion policies say
    should go are deleted; see
    [accounts](./accounts.md#automated-post-deletion).


Server rules
------------

`/api/v1/admin/rules` lists, creates, edits and deletes the server rules, for a
role with `manage_rules`; `…/move_up` and `…/move_down` reorder them. A rule's
text is required and at most 300 characters, and so is each translation's,
one per language. Translations are given as Rails' nested attributes,
`translations_attributes`, each with an `id` to change or `_destroy` to remove
an existing one; one with blank text is ignored. Deleting a rule discards it
(`deleted_at`), so reports that cite it still show it. Moving the first rule
up puts it last, as Mastodon's `Rule#move!` does. As in Mastodon, none of this
is logged.


Roles
-----

`/api/v1/admin/roles` lists the roles a user can be given, lowest first, and
creates one; `/api/v1/admin/roles/:id` shows, edits and deletes one. The
everyone role, id -99, is reached by its id. All of it needs `manage_roles`,
and each role says whether the acting role may edit (`can_update`) or delete
(`can_destroy`) it, as Mastodon's `UserRolePolicy` decides:

 -  a role may be edited only by a role positioned above it, or by its own
    holder;
 -  a role may be deleted only by a role above it, and never by its own holder.
    The everyone role cannot be deleted. Deleting a role leaves its users with
    none, which is the everyone role.

A role's permissions are given by name, as `permissions_as_keys`, and a save
is checked against the editor's own role, with Mastodon's messages:

 -  it may not grant a permission the editor's role lacks, or be positioned
    above the editor's role;
 -  the editor may not change the permissions or position of their own role,
    nor whether it requires two-factor authentication unless it is an
    administrator role;
 -  the everyone role may hold only `invite_users` and `invite_bypass_approval`;
 -  a name is required (except for the everyone role), a color must be a CSS
    hex color, and the collection limit may not be negative.

Creating, editing and deleting a role are logged.


Announcements
-------------

`/api/v1/admin/announcements` lists announcements, newest first (`?published=1`
or `?unpublished=1` to narrow it), and creates one; `…/:id` shows, edits and
deletes one, and `…/:id/publish` and `…/:id/unpublish` do what they say. All
need `manage_announcements`. An announcement needs text, and a start once it
has an end, and the other way round. Creating, editing, publishing,
unpublishing and deleting are logged, the publishing as updates, as in
Mastodon.

A new announcement is published at once unless `scheduled_at` is later. Every
minute each instance publishes the scheduled announcements that are due, and
unpublishes the ones whose `ends_at` has passed. Publishing an announcement, or
editing a published one, links the posts its text names (`status_ids`, which
`/api/v1/announcements` serves as `statuses`) and sends it to every signed-in
user's stream as an `announcement` event. Unpublishing or deleting sends
`announcement.delete`, and a reaction sends `announcement.reaction` with its new
count. `/api/v1/announcements` lists the published ones in Mastodon's order,
by start, schedule or publication.

`…/:id/preview`, `…/:id/test` and `…/:id/distribution` mail a published
announcement, as Mastodon's announcement notifications do: the preview counts
the confirmed users who are not suspended, the test mails the moderator alone,
and the distribution mails everyone once. They need `manage_settings` as well,
and are refused once the announcement has been mailed.


Federation
----------

`/api/v1/admin/instances` lists the servers this one knows, as Mastodon's
`instances` view has them: every domain with accounts here, a domain block or a
domain allow, with the most accounts first and forty a page (`?page=`). It
takes Mastodon's filters, `limited` (blocked domains, newest block first),
`by_domain` and `availability` (`failing` or `unavailable`); in limited
federation mode it lists only the allowed domains. Each server comes with its
block, its allow, whether it is unavailable, and how many days deliveries to
it have failed. All of this needs `manage_federation`.

A domain given to these endpoints, to the domain block and allow APIs, to
the audit log's `target_domain` filter, and in a report's
`forward_to_domains` is written as Mastodon's `TagManager#normalize_domain`
writes it: stripped, one trailing `/` removed, lower case, in its ASCII
form, and with its port, if it has one, kept. A domain with a space, `/` or
`@` in it is one Mastodon's URL library refuses, and is refused as Mastodon
refuses it there: a domain allow with
`422 Validation failed: Domain is invalid`; a domain block, the audit log
filter and a report with a 500, which nothing in Mastodon rescues; the peers
search with no domains.

A domain block covers its domain and every subdomain of it, port included, as
Mastodon's `DomainBlock.rule_for` matches them: a block on `example.com`
covers `a.example.com` but not `example.com:8080`, an account on a server at
that port, and a block on `example.com:8080` covers only that port. Where a
URL is checked rather than an account's domain, as when an incoming request is
signed, only its host counts, without the port.

The `instances` materialized view itself is refreshed every hour, as
Mastodon's `Scheduler::InstanceRefreshScheduler` does, so a Mastodon sharing
the database, or anything else reading the view, sees current servers. The
schema creates it empty, so the first refresh fills it plainly and later ones
run concurrently.

`/api/v1/admin/instances/:domain` is one server's page: the same, plus the
fourteen days of delivery failures Mastodon's availability strip shows, and
its moderation notes. On it:

 -  `…/clear_delivery_errors` forgets the failures;
 -  `…/stop_delivery` marks the domain unavailable now, and is logged;
 -  `…/restart_delivery` lifts that mark and the failures, and logs the mark's
    removal;
 -  `DELETE` purges the domain, as `PurgeDomainService` does: every account
    from it is deleted, its custom emoji with them, and the severed
    relationships recorded against it are marked purged. It is logged as
    destroying the instance. The web client offers it for a domain that is
    unavailable or suspended.
 -  `…/moderation_notes` adds a note of at most 2,000 characters, and
    `…/moderation_notes/:id` deletes one, for its author or a role that
    manages federation and outranks the author's. Notes are not logged.

The counters on the page are Mastodon's instance measures,
`instance_accounts`, `instance_statuses`, `instance_media_attachments`,
`instance_follows`, `instance_followers` and `instance_reports`, which
`POST /api/v1/admin/measures` now serves with a `domain` parameter for each,
as Mastodon's admin API does; `POST /api/v1/admin/dimensions` serves
`instance_accounts` and `instance_languages` the same way. Their totals are of
all time, and with `include_subdomains` take in the subdomains too (the daily
series, as in Mastodon, still the domain alone).

Finding the failing domains scans this instance's delivery failure keys in
Redis, which is why `SCAN` is among the commands a tenant's Redis user needs
(see [shared Redis](./redis)).

### Exporting and importing blocks

`/api/v1/admin/export_domain_blocks/export` downloads the domain blocks with
any limitation as Mastodon's CSV, with the columns `#domain`, `#severity`,
`#reject_media`, `#reject_reports`, `#public_comment` and `#obfuscate`, and
`/api/v1/admin/export_domain_allows/export` the allowed domains under
`#domain`. A file Mastodon exported imports here, and the other way round.

`…/export_domain_blocks/import` takes such a file, as the multipart field
`data`, and answers with the blocks it would create, without creating any, as
Mastodon's import shows a form to confirm: a domain already covered by a block
is left out, a row Mastodon would refuse is reported, and each block's private
comment says which file it came from and when. The domains among them that
local accounts follow, or are followed from, are listed apart. The moderator
then creates the ones they keep with `POST /api/v1/admin/domain_blocks`.
`…/export_domain_allows/import` allows every domain in its file at once,
logging each. A file without a `#domain` header is read as one domain a line.
Both need `manage_federation`, and refuse a file of more than 20,000 rows.


Relays
------

`/api/v1/admin/relays` lists and adds relays, and `…/:id/enable`,
`…/:id/disable` and `DELETE …/:id` do the rest, for a role with
`manage_federation`; each is logged. A relay's inbox URL must be an http or
https URL no other relay has.

Adding or enabling a relay sends it a `Follow` of the public collection from
the instance actor, as Mastodon's `Relay#enable!` does, and the relay is
pending until it answers: its `Accept` of that `Follow` enables it, and a
`Reject` marks it rejected. Disabling sends the `Undo` of the `Follow`, and
deleting an enabled relay disables it first. Both start the relay's host's
delivery failures afresh.

An enabled relay gets the instance's public posts, its accounts' profile
updates, deletions and moves, as Mastodon sends them, with their authors'
[Linked Data signatures](../mastodon/http-signatures#linked-data-signatures)
so that the servers the relay passes them to can tell who wrote them. An
`Announce` from an enabled relay brings the post it names here without making
it a boost. A post a relay passes on as its author wrote it is taken on its
author's Linked Data signature, from an account nobody here follows too, as
long as the relay is enabled. A relay needs unsigned fetches, and in
authorized fetch mode posts go without their Linked Data signatures, as
Mastodon sends them, so a relay does not work while authorized fetch is on.


Invites
-------

Every invite on the server is listed, and all of them deactivated, at
`/api/v1/admin/invites`, for a role with `manage_invites`; see
[invites](./invites#every-invite).


Webhooks
--------

`/api/v1/admin/webhooks` lists and adds webhooks, `…/:id` shows, edits and
deletes one, `…/:id/enable` and `…/:id/disable` switch one, and
`…/:id/secret/rotate` gives it a new secret, all for a role with
`manage_webhooks`, as Mastodon's `WebhookPolicy` allows. Editing or deleting a
webhook also needs the permissions its events need: `manage_users` for the
account events, `manage_reports` for the report events, and `view_devops` for
the status events; each webhook says whether the acting role may
(`can_update`). A new webhook gets a random secret, as Mastodon makes one.

A webhook needs an http or https URL no other webhook has, and at least one of
the events [moderation](./moderation#webhooks) lists, each one the moderator's
role could see. Its template, if any, must parse as Mastodon's does: text, and
<code v-pre>{{path.to.value}}</code> expressions of lower-case names and array
indices. As in Mastodon, none of this is logged.


Follow recommendations
----------------------

`/api/v1/suggestions` and `/api/v2/suggestions` suggest whom to follow from
Mastodon's sources, each suggestion with the sources that put it there:

 -  `featured`: the accounts the `bootstrap_timeline_accounts` setting names;
 -  `friends_of_friends`: accounts followed by the accounts one follows, most
    often first. As in Mastodon, an account whose `hide_collections` is unset
    counts as hiding whom it follows;
 -  `most_followed` and `most_interactions`: the server's recommendations,
    those mostly posting in the user's language first.

Only discoverable accounts that are not limited, suspended, moved or a
memorial are suggested, and never one the user follows, has asked to follow,
blocks, is blocked by, mutes, dismissed, or whose domain the user blocks. The
list is shuffled, and keeps its order for a quarter of an hour, so paging
through it with `offset` sees one list, as Mastodon's cached list does.
Mastodon's similar-profiles source needs Elasticsearch and its FASP source a
FASP provider; eunha has neither, as a Mastodon without them has neither.

Once a day each instance refreshes the recommendations as Mastodon's
`FollowRecommendationsScheduler` does: it records the language and
sensitivity each discoverable, unlocked account mostly posts with
(`account_summaries`), then recommends the accounts at least five recently
active local users follow, and those whose posts drew at least five boosts
and favourites this month (`global_follow_recommendations`), leaving out
sensitive ones.

`/api/v1/admin/follow_recommendations` lists the recommendations, those in a
`language` first (the moderator's own by default), or with
`status=suppressed` the accounts kept out of them, for a role with
`manage_taxonomies`. `…/suppress` and `…/unsuppress` with `account_ids` keep
accounts out and let them back, at once. As in Mastodon, neither is logged.


Software updates
----------------

`/api/v1/admin/software_updates` lists the newer releases the update check
recorded; see [update notices](./update-notices).


Donation campaigns
------------------

Mastodon's web interface can show a donation banner, which it asks for at
`GET /api/v1/donation_campaigns`. Without a campaign API configured the answer
is `204 No Content`, and no banner is shown. To ask one, as Mastodon's
`DONATION_CAMPAIGNS_URL` and `DONATION_CAMPAIGNS_ENVIRONMENT` do (which eunha
also reads from the environment):

~~~~ toml
[instance.donation_campaigns]
api_url = "https://api.joinmastodon.org/v1/donations/campaigns/active"
environment = "production"  # optional
~~~~

For a signed-in user, eunha asks the API with `platform=web`, the
`environment`, the user's interface locale, and a seed from 0 to 99 that the
user's account id always gives (Ruby's `Random.new(id).rand(100)`, so a user
gets the same seed from Mastodon); any query the URL had is replaced. A `200`
with JSON is the campaign, kept an hour in Redis under Mastodon's own cache
keys (`cache:donation_campaign_request:<seed>:<locale>` and
`cache:donation_campaign:<id>:<locale>`); anything else is `204`. The request
goes through the same guard as federation, so a private address needs
`allowed_private_networks`. Ten failures in a row turn the circuit breaker
(`stoplight:donation_campaigns:*`) red for a minute, during which the endpoint
answers `503`.


Annual reports
--------------

While the `wrapstodon` setting is on, from 10 to 31 December, members can
generate a report of their year (`POST /api/v1/annual_reports/:year/generate`,
see [remote replies](./remote-replies) for how the client waits for it). The
report is Mastodon's schema 2, made from its sources over the posts whose ids
fall in the year: an archetype from the counts of posts, replies, boosts and
polls; the most boosted public or unlisted post that has stats (the most
favourited and most replied are left empty, as Mastodon leaves them); the
posts of the year and the follows made in it; and the hashtag used most, more
than once, by its display name. The API answers with the report's accounts
(for schema 2, its owner; for a Mastodon 2024 report, the accounts it names)
and its posts. Reports eunha made before migration 033 were labelled schema 1;
see [migrations](./migrations). Each report gets a random share key, and the
API's `share_url` is the page that shows it to anyone with the link, at
Mastodon's address:
`https://<domain>/@<username>/wrapstodon/<year>/<share_key>`. A report imported
without a share key has no `share_url`.

The page is the web app, carrying the report as Mastodon's does: in a
`<script id="wrapstodon-data">` holding the same JSON as the annual reports
API, serialized for no viewer, with the instance's `domain`. It is marked
`noindex` and cached publicly for ten minutes. A wrong share key, year or
account, or an account that is unconfirmed or awaiting approval, is a `404`; a
suspended account is a `403` while the suspension can be undone and a `410`
once it cannot. Eunha's web app draws the report its own way; what that changes
from Mastodon's page, and what limited federation mode does to it, is recorded
as the `shared-wrapstodon-page` divergence.


Custom emoji
------------

`/api/v1/admin/custom_emojis` lists, uploads, lists or unlists, enables or
disables and deletes the server's own custom emoji, with the
`manage_custom_emojis` permission (`admin-custom-emoji-rest-api`). An upload
is a multipart `shortcode`, `image` and optional `visible_in_picker`, checked
as `CustomEmoji` checks one: a PNG, GIF or WebP image under 256 KB (the type
is read from the bytes, as Paperclip asks `file`), a GIF no larger than
1280×720 pixels, and a shortcode of letters, digits and underscores, two to
128 characters long, that no other local emoji has. What fails answers `422`
with Mastodon's messages, such as
`Validation failed: Shortcode has already been taken`.

The image is stored as Paperclip stores it, so a Mastodon on the same database
and bucket shows it: `image_file_name` (sixteen random hex digits and the
type's extension), `image_content_type`, `image_file_size`,
`image_updated_at` and `image_storage_schema_version`, and the files at
`custom_emojis/images/<id partition>/original/<name>` as uploaded and
`custom_emojis/images/<id partition>/static/<name>.png`, a PNG of its first
frame without the original's metadata. `url` and `static_url` are those two,
and an emoji's ActivityPub `icon` the original, at the emoji's own
`/emojis/<id>`. Deleting an emoji deletes both files. Each change is written
to the audit log as Mastodon's pages write it: `create`, `update` for listing,
unlisting or a new shortcode, `enable`, `disable` and `destroy`.

`GET /api/v1/custom_emojis` lists what Mastodon's does: local emoji that are
enabled and visible in the picker, each with its `category` and whether it is
the category's `featured` one when it has a category. A remote emoji is shown
from the server it came from rather than from a copy
(`remote-account-images-not-downloaded`).


The dashboard
-------------

`GET /api/v1/admin/dashboard`, for a role with `view_dashboard`, is what
Mastodon's dashboard shows above its charts: how many reports are unresolved,
users await approval, hashtags await review and appeals await a decision, and
the system checks the role may see, each with its message and where to act on
it. The web client's dashboard shows each count to a role that may act on it.
The checks are Mastodon's, less the two for services eunha does not use
(Elasticsearch and Sidekiq):

 -  `software_version_check`, and its critical and patch variants, while the
    update check has recorded a newer release, for `view_devops`;
 -  `upload_check_privacy_error_object_storage`, when asking the media
    bucket's public address, or its S3 endpoint, for a listing gets one, for
    `view_devops`. It is critical: anyone could list every upload;
 -  `database_schema_check`, while migrations are pending, for `view_devops`;
 -  `rules_check`, while the server has no rules, for `manage_rules`.


Mail
----

Eunha's mail is shaped as Mastodon's mailers send theirs. What the staff are
told (new reports, appeals, accounts and trends to review, closed
registrations, software updates and the end of support) is plain text alone,
as `AdminMailer`'s templates are; every other mail is `multipart/alternative`,
its text beside its HTML. The text is made from the HTML, and the wording is
eunha's own, in English and Korean, rather than Mastodon's translations.

Every mail but those to an account about itself (`UserMailer`, a
`Devise::Mailer`) carries `ApplicationMailer`'s
`Auto-Submitted: auto-generated`, `Precedence: list` and
`X-Auto-Response-Suppress: All`, so that an out-of-office reply is not sent
back. The mail about a critical software update, and the one saying the tracked
release is out of support, carry `Importance: high`, `Priority: urgent` and
`X-Priority: 1` besides; the earlier end-of-support warnings do not.
