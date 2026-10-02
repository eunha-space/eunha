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
| `min_age`                                                        | `/api/v2/instance`'s `registrations.min_age`                                           |
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

The rest are saved for a Mastodon on the same database and read by nothing in
eunha yet: `theme` (Mastodon 4.7 has only the default), `landing_page`,
`mascot`, `noindex`, `preview_sensitive_media`, `captcha_enabled` (eunha has
no CAPTCHA), `bootstrap_timeline_accounts`, and `backups_retention_period`.

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
