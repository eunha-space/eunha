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

### What the settings drive

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
| `wrapstodon`                                                     | annual reports                                                                         |
| `bootstrap_timeline_accounts`                                    | follow suggestions (see [below](#follow-recommendations))                              |
| `noindex`                                                        | the default of each member's `noindex` (see [account security](./accounts))            |
| `backups_retention_period`                                       | archive takeouts are deleted after it (see [import and export](./import-export))       |

The rest are saved for a Mastodon on the same database and read by nothing in
eunha yet: `theme` (Mastodon 4.7 has only the default), `landing_page`,
`mascot`, `preview_sensitive_media` and `captcha_enabled` (eunha has no
CAPTCHA).

### Settings and the instance configuration

Eunha's instance configuration carried the title, descriptions, contact
address and whether registrations are open before eunha read these settings.
The configuration now stands where Mastodon's `config/settings.yml` stands: it
supplies the default, and a value saved in the settings wins over it, blank
included. So an instance runs as configured until an administrator saves the
settings, and from then on as saved, as Mastodon would. Registrations map as:

| Configuration                                            | `registrations_mode` |
| -------------------------------------------------------- | -------------------- |
| `registrations_open = false`                             | `none`               |
| `registrations_open = true`, `approval_required = true`  | `approved`           |
| `registrations_open = true`, `approval_required = false` | `open`               |

As in Mastodon, an invite to a server that is not open lets its holder sign up,
but approves them only if whoever wrote it may bypass approval (see
[invites](./invites)). With no `site_contact_username` saved, the contact
account is the local account with the highest role, as before. This is the
`site-settings-default-to-configuration` divergence.

`authorized_fetch` set in the instance configuration, or forced by limited
federation mode, decides whatever is saved; the settings then list it under
`overridden`, and the web client shows it disabled, as Mastodon's form does
when its environment variable is set.

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

Eunha does not cache remote media itself, so the media retention matters only
for a database that came from Mastodon.


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
all time.

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
updates, deletions and moves, as Mastodon sends them. An `Announce` from an
enabled relay brings the post it names here without making it a boost.
Relays that forward posts signed by someone else rely on Linked Data
signatures, which eunha does not verify, so such a post is taken only when
its author's own server sends it. A relay needs unsigned fetches, so it does
not work while authorized fetch is on.


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
