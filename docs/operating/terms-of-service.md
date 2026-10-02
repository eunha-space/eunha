Terms of service and privacy policy
===================================

Both documents work the way Mastodon's do, from the same storage: terms of
service are versions in the `terms_of_services` table, and the privacy policy is
the `site_terms` setting. An instance moved onto eunha from Mastodon keeps both
without doing anything.


Terms of service
----------------

A version is a draft until it is published, and live from its effective date.
`GET /api/v1/instance/terms_of_service` answers with the current version —
the latest live one, or while none is live yet the first one to come — and is a
404 when nothing has been published.
`GET /api/v1/instance/terms_of_service/:date` answers with the published
version effective on that date. Both are Mastodon's
`REST::TermsOfServiceSerializer`:

 -  `content` is the text with `%{domain}` replaced by the instance's domain,
    rendered from Markdown with HTML escaped and images left as written, which
    is what Redcarpet's `escape_html` and `no_images` do. Eunha renders with
    pulldown-cmark, which accepts the same Markdown Redcarpet does without
    extensions; the two differ in whitespace in the HTML they emit, not in what
    it says.
 -  `effective` is true only once the effective date has *passed*. Terms going
    live today are the current terms but are not yet `effective`, as upstream
    has it.
 -  `succeeded_by` is the effective date of the latest other published version
    effective on or after this one — the latest, not the next, which is how
    upstream's query reads.

`/api/v2/instance` lists `urls.terms_of_service`, the web page at
*/terms-of-service*, only while there are current terms; `urls.privacy_policy`
is always */privacy-policy*. The web client serves both pages, and
*/terms-of-service/:date* for each version.

### Writing them

The admin pages are under *Moderation → Terms of service*, for roles with
`manage_settings` (Mastodon's `TermsOfServicePolicy`). They are a client of
the admin REST API Mastodon would have if its forms were endpoints — see the
`terms-of-service-rest-api` divergence:

| Endpoint                                               | Mastodon's page                         |
| ------------------------------------------------------ | --------------------------------------- |
| `GET /api/v1/admin/terms_of_service`                   | the latest published version            |
| `GET /api/v1/admin/terms_of_service/history`           | *History*                               |
| `GET`, `PUT /api/v1/admin/terms_of_service/draft`      | *Draft*: save, or `action_type=publish` |
| `GET`, `POST /api/v1/admin/terms_of_service/generate`  | *Use template*                          |
| `GET /api/v1/admin/terms_of_service/:id/preview`       | the notification preview                |
| `POST /api/v1/admin/terms_of_service/:id/test`         | *Send preview to* yourself              |
| `POST /api/v1/admin/terms_of_service/:id/distribution` | *Send emails*                           |

The draft is the latest unpublished version, or a new one holding the live
text and effective ten days from now. Saving checks upstream's validations: text
is required; publishing also requires a changelog and an effective date; no two
versions share an effective date (drafts included, and a missing date counts as
a date); and the date may not be earlier than the live version's, or today's
when nothing is live. A refusal is a 422 whose `error` lists upstream's
messages, such as
`Validation failed: Effective date is too soon, must be later than 2026-10-02`.
Publishing writes a `publish` entry to the audit log.

The template is Mastodon's *config/templates/terms-of-service.md*. Generating
from it needs every field — domain, minimum age, jurisdiction, choice of law,
the legal-notice and DMCA addresses, and where arbitration notices go — and
creates a draft; nothing is published until an administrator publishes it.

### Telling users

Once a version is published, *Notify users* previews and sends
`UserMailer#terms_of_service_changed`. It goes to confirmed users who signed up
before the version was published, whose account is not suspended, and who
signed in during the year before it. Everyone else who signed up before it —
suspended, or not seen for a year — is flagged with `require_tos_interstitial`
instead, and the web client shows them the new terms the next time they open
it, until they have opened the terms page. A version is distributed once:
`notification_sent_at` records it, and the endpoints refuse to send again.

Mastodon serves the interstitial by replacing its web pages; eunha's web client
asks `/api/eunha/v1/terms_of_service/interstitial` instead (the
`terms-of-service-interstitial-api` divergence). Clients other than eunha's own
see neither, as with Mastodon.

The mails go out one at a time from the server process, through the same
sender as every other eunha mail, in English.

### Terms from the instance configuration

Before eunha read the table, an instance's terms were the `terms_of_service`
text in its configuration, served as effective on 2025-01-01. Eunha no longer
reads it: with nothing published, `/api/v1/instance/terms_of_service` is a 404
and `urls.terms_of_service` is null, as in Mastodon.
`eunha settings import-config` (see
[administration](./administration#settings-and-the-instance-configuration))
publishes it once, while nothing else is published, as the version it was
served as: effective on 2025-01-01, already notified so nobody is mailed about
terms they were already served, and with an empty changelog. The key can be
deleted afterwards.


Privacy policy
--------------

`GET /api/v1/instance/privacy_policy` serves `site_terms` when it is set, as
Mastodon does, rendered the same way as the terms, and when it is blank the
policy Mastodon ships (*config/templates/privacy-policy.md*), dated 2022-10-07
as upstream dates it. The configuration's `privacy_policy`, where eunha kept
the policy before, is read only by `eunha settings import-config`, which copies
it into a `site_terms` nobody has saved.

The policy is written on the about page of the
[server settings](./administration#server-settings), as in Mastodon.


Percent signs
-------------

Both texts go through Ruby's `format(text, domain:)` to fill in the domain, as
upstream does: `%{domain}` and `%<domain>s` become the domain and `%%` a single
`%`. A stray `%` is read as Ruby reads it, so write `%%` for a literal one. The
`% s` in “100% sure” prints the argument hash (`{domain: "example.com"}`), and
what Ruby refuses — an unknown `%{name}`, a trailing `%`, a second bare
conversion, a bare one beside `%{domain}` — makes the endpoint answer 500, as
upstream's unrescued exception does.


What is not here
----------------

Mastodon's sign-up form asks new users to agree to the terms and privacy policy
(`agreement`) and, when `min_age` is set, for a date of birth. Eunha's sign-up
does not enforce either yet; that belongs to sign-up rather than to the terms.
