Account security
================

What a member can do to keep their own account safe, as Mastodon offers it on
its web settings pages. Mastodon has these only as server-rendered forms behind
a browser session; eunha's settings page is a single-page app holding an OAuth
token, so each is also served under `/api/eunha/v1/`, and each of those is a
recorded divergence (`divergences.toml`).


Two-factor authentication
-------------------------

An authenticator app (TOTP) first, then security keys on top of it, as
Mastodon has it. Everything is stored the way Mastodon 4.7 stores it, so either
can read what the other wrote:

 -  `users.otp_secret`: a 32-byte base32 secret behind `encrypts :otp_secret`,
    the Rails `ActiveRecord::Encryption` envelope. Eunha needs the instance's
    `active_record_encryption` keys to set it up or to check a code; without
    them the settings page says two-factor authentication is not available,
    as a Mastodon without the keys cannot offer it either.
 -  `users.otp_backup_codes`: bcrypt digests of ten 16-character recovery
    codes, each usable once.
 -  `users.consumed_timestep`: the time step of the last code used, so a code
    works only once. A code is accepted thirty seconds either side of now.
 -  `webauthn_credentials` and `users.webauthn_id`: a key's id, its COSE
    public key and counter, base64url as webauthn-ruby writes them. The
    relying party is the instance's domain over HTTPS; ES256, PS256 and RS256
    keys are accepted.

Mastodon asks for no attestation, so browsers mostly send the `none` format.
Whatever statement does come is verified as webauthn-ruby 3.4.3 verifies it
under Mastodon's configuration, which adds no trust roots of its own, so each
format is checked against the roots its gem ships:

 -  `none` must carry an empty statement, and `packed` self attestation must
    be signed by the new key.
 -  `packed` with a certificate chain, and `fido-u2f`, are checked and then
    refused: their gems ship no roots, so no chain can be trusted.
 -  `android-key` is checked against the Google hardware attestation root,
    which expired in May 2026, so it is now refused too.
 -  `android-safetynet` is checked against Google's six roots, its response
    must be no more than a minute old, and the device must match a
    compatible profile.
 -  `tpm` is checked against the TPM vendors' roots, and `apple` against
    Apple's WebAuthn root.
 -  Any other format is refused.

Certificate chains are checked as OpenSSL's store checks them (validity
periods, CA flags, path lengths, key usage, unknown critical extensions and
signatures), except that name constraints are not enforced.

The API, each with a `read:accounts` or `write:accounts` token:

| Request                                                     | Mastodon's                                         |
| ----------------------------------------------------------- | -------------------------------------------------- |
| `GET /api/eunha/v1/two_factor_authentication`               | the methods page                                   |
| `POST …/otp` with `password`                                | `OtpAuthenticationController#create`               |
| `POST …/otp/confirm` with `otp_attempt`                     | `ConfirmationsController#create`                   |
| `POST …/recovery_codes` with `password`                     | `RecoveryCodesController#create`                   |
| `DELETE …` with `password`                                  | `TwoFactorAuthenticationMethodsController#disable` |
| `POST …/webauthn_credentials/options` with `password`       | `WebauthnCredentialsController#options`            |
| `POST …/webauthn_credentials` with `credential`, `nickname` | `#create`                                          |
| `DELETE …/webauthn_credentials/:id` with `password`         | `#destroy`                                         |

Each change mails what Mastodon's `UserMailer` mails: two-factor enabled,
disabled, recovery codes regenerated, security keys enabled or disabled, a key
added or deleted.

### Signing in

The OAuth authorization page and the account pages (`/account/login`) take the
password, then ask for the second factor when the user has one: a code from the
app or a recovery code, or a security key. Mastodon keeps that half-finished
sign-in in its session; eunha keeps it in Redis under a random token the form
carries (`sign_in_attempt:*`), for an hour. As upstream does:

 -  a change to the user in between (`users.updated_at`) sends the person back
    to the password;
 -  ten wrong codes in an hour (`2fa_auth_attempts:<user>:<hour>`) stop further
    attempts until the hour is over;
 -  each attempt is recorded in `login_activities` with its method, and a
    failed second factor mails the user, at most once an hour
    (`2fa_failure_notification:<user>`);
 -  a sign-in without TOTP from an address unlike any the user signed in from
    before mails them that it happened (`SuspiciousSignInDetector`).

As in Mastodon, `/oauth/token` takes only the `authorization_code` and
`client_credentials` grants; any other, the password grant among them, answers
400 `unsupported_grant_type`, so a password alone never gets a token.

### A role that requires it

A role with `require_2fa` (and the everyone role, id -99, if it has it) leaves a
member without two-factor authentication unable to use the API: every request
`require_user!` guards answers 403, `Your login is currently disabled`. Signing
in on the authorization page or the account pages leads straight into setting up
an authenticator app, then shows the recovery codes, then carries on with the
authorization, as Mastodon's `require_functional!` sends the person to the setup
first. The settings page says so too, and its setup works for such a member.


Sessions
--------

A sign-in on the account pages starts a session in `session_activations`, as a
Mastodon web sign-in does: a random `session_id` in the `account_session`
cookie, the browser's address and user agent, and an access token for the
instance's web app with `read write follow`. Signing out ends it; at most ten
are kept, the oldest purged first. Changing the password ends every other
session.

`GET /api/eunha/v1/sessions` lists them, newest activity first, with the
browser and platform Mastodon would name and which one the asking token
belongs to; `DELETE /api/eunha/v1/sessions/:id` ends one. Eunha's own web
client signs in through OAuth rather than a session, so it appears among the
authorized apps below.


Authorized apps and sign-in history
-----------------------------------

`GET /api/eunha/v1/authorized_applications` lists every app holding a token the
member has not revoked (Doorkeeper's `authorized_for`), with its scopes and when
a token of it was last used. Eunha records that as Mastodon does, at most once a
day per token (`oauth_access_tokens.last_used_at` and `last_used_ip`).
`DELETE /api/eunha/v1/authorized_applications/:id` revokes the app's tokens and
grants for the member, removes their web push subscriptions and closes their
streaming connections; the instance's own web app (`superapp`) is not offered,
as Mastodon's page does not offer it. Revoking a single token at
`/oauth/revoke` removes its push subscriptions and closes its streams too.

`GET /api/eunha/v1/login_activities` is the sign-in history, newest first, paged
with `max_id` and `limit`: each attempt's method (`password`, `otp`,
`webauthn`), whether it succeeded and why not, the address and the browser.


Signing up
----------

`POST /api/v1/accounts`, and the sign-up forms in front of it, check what
Mastodon 4.7's `User` validates, and answer a refusal as
`ValidationErrorFormatter` does: `Validation failed: …` with per-attribute
`ERR_*` codes in `details`.

 -  `agreement` must be accepted: `true`, `"true"` or `"1"`. The forms ask for
    it with a checkbox linking the terms of service (when there are any) and the
    privacy policy.
 -  When `Setting.min_age` is set, `date_of_birth` (an ISO 8601 date) is
    required, and someone born less than that many years ago is refused
    (`ERR_BELOW_LIMIT`). The date is not stored; the account's
    `users.age_verified_at` is set when it is created, as
    `User#set_age_verified_at` does. `registrations.min_age` in
    `/api/v2/instance` advertises it.
 -  When `Setting.require_invite_text` is on and registrations need approval,
    `reason` is required unless the invite's creator may bypass approval, and
    is at most 420 characters. `registrations.reason_required` advertises it.
 -  The password is 8 to 72 characters, Devise's `password_length`.
 -  `time_zone`, as `AppSignUpService` takes it, becomes `users.time_zone`
    when it names a zone Rails knows (see [Time zones](#time-zones)); any
    other value is dropped rather than refused.

Eunha writes the account when its email address is confirmed rather than when
the form is sent, so `age_verified_at` follows `Setting.min_age` as it stands
at confirmation.


Passwords
---------

Changing the password on the account pages asks for the current one, ends every
other session and mails Devise's `password_change`. Passwords are 8 to 72
characters everywhere they are set.

A forgotten password is Devise's recoverable module, as Mastodon's
`Auth::PasswordsController` serves it:

 -  `/auth/password/new` asks for the address, linked from both sign-in forms.
    `POST /auth/password` answers the same whether or not the address has an
    account (`config.paranoid`), and mails a link only to a confirmed user
    with a password whose account is not a memorial.
 -  The link, `/auth/password/edit?reset_password_token=…`, works for six hours
    (`reset_password_within`). The token is `Devise.friendly_token`, and
    `users.reset_password_token` holds a digest of it, never the token: with
    the instance's [`secret_key_base`](./instances#mastodon-s-secret-key-base),
    Devise's own, so a link Mastodon mailed works too; without it, the
    token's SHA-256, which is still read once the secret is configured.
 -  Setting the new password there, or with `PUT /auth/password`
    (`reset_password_token`, `password`, `password_confirmation`), ends every
    session, revokes every token and grant with their push subscriptions and
    streams (`User#revoke_access!`), and mails `password_change`. The person is
    not signed in afterwards (`sign_in_after_reset_password = false`).


Changing the email address
--------------------------

Mastodon changes a member's address on its account settings form, Devise's
`update_with_password` with `reconfirmable`; eunha serves the same at
`PUT /api/eunha/v1/email` (`email`, `current_password`, a `write:accounts`
token), and `GET` says what the address is and which one is waiting:

 -  the current password is required, and a suspended account cannot change
    it;
 -  the address is stripped and lowercased, then checked as `User` checks a
    changed email: present, well formed, at most 320 characters, not another
    user's, and — `EmailMxValidator` — a domain that takes mail and that no
    email domain block covers, nor its mail hosts. A user not yet confirmed
    is also held to `UserEmailValidator`: the domain blocks and the canonical
    email blocks. Refusals answer `422` as `Validation failed: …` with
    per-attribute codes, as the sign-up does;
 -  a different address waits in `users.unconfirmed_email`, with a new
    confirmation token, until the link mailed to it
    (`reconfirmation_instructions`, valid two days) is followed; the address
    being left is told (`email_changed`). Mail goes to the old address until
    then.

The admin's change of address (`POST /api/v1/admin/accounts/:id/change_email`)
writes `unconfirmed_email` the same way and mails the same reconfirmation
link, without the notice to the old address, as upstream's does.


Deleting the account
--------------------

`/account/delete`, and `DELETE /api/v1/accounts` for the settings page, are
Mastodon's `Settings::DeletesController`: the password (or, for an account
without one, the username) as the challenge, then `Account#mark_deleted!` —
`requested_deletion_at` and the suspension that hides the account at once — the
purge as an `AccountDeletionWorker` in the [job queue](./jobs.md), with the
username kept reserved, and the browser signed out. The page warns a confirmed,
approved member that this is irreversible and their username stays taken; a
member not yet confirmed or approved is told instead how to fix their address
and that the username becomes available again, as upstream's page does. Eunha's
`DELETE /api/v1/accounts` has no Mastodon counterpart and is recorded as a
divergence.


Privacy and preferences
-----------------------

What Mastodon's privacy page and preference pages set without an API is served
at `GET` and `PATCH /api/eunha/v1/preferences`; what Mastodon's API already
sets (`discoverable`, `locked`, `indexable`, `hide_collections`, the posting
defaults under `source`) stays with `update_credentials`. The settings page's
privacy section uses both.

 -  `noindex` (the privacy form's “include profile page in search engines”,
    posted inverted as `indexable`, which is also accepted) asks search
    engines to stay away: the account entity's `noindex` says so, and the web
    client's profile page carries
    `<meta name="robots" content="noindex, noarchive">`. A member who never
    chose follows `Setting.noindex`.
 -  `show_application` off hides the app a post was sent from from everyone
    but its author, as `show_application?` does.
 -  `chosen_languages` limits public timelines to those languages; an empty
    list clears it. `locale` is the language eunha writes mail in; one Mastodon
    has no translation for is cleared.
 -  `time_zone` is the zone the times in mail are written in (see
    [Time zones](#time-zones)).
 -  `aggregate_reblogs` (on unless turned off) groups boosts in the home
    timeline and lists (see [Boosts in timelines](#boosts-in-timelines)).
 -  `notification_emails` turns the mails eunha sends on or off: the
    notification emails `follow`, `follow_request`, `reblog`, `favourite`,
    `mention` and `quote` (see [Notification emails](#notification-emails)),
    and for staff `report`, `pending_account`, `trends`, `appeal`,
    `end_of_support`, and `software_updates` (`none`, `critical`, `patch` or
    `all`). `always_send_emails` mails notifications even while the member is
    online.

`source[sensitive]` is kept under Mastodon's `default_sensitive` key in
`users.settings`; eunha used to write `web.default_sensitive`, which it still
reads.


Boosts in timelines
-------------------

With `aggregate_reblogs` on, the home feed and each list's feed keep a post
from showing up again and again as people boost it, the way Mastodon's
`FeedManager#add_to_feed` does:

 -  a boost of a post that is among the 80 newest entries of the feed
    (`REBLOG_FALLOFF`) is not added;
 -  nor is a second boost of a post whose first boost is among them; it is
    kept aside instead, and if the first boost is undone or deleted, the
    oldest boost kept aside takes its place;
 -  a post whose boost arrived before it is not added again.

What a feed tracks for this lives in Redis beside the feed, under Mastodon's
key names (`feed:home:<id>:reblogs`, `feed:home:<id>:reblogs:<post>`), and
goes with it. Turning the setting off affects only boosts that arrive
afterwards.


The home feed while away
------------------------

As in Mastodon, Redis keeps the home feed and list feeds only of members who
signed in within the last seven days (`User::ACTIVE_DURATION`). New posts are
not added to anyone else's, and the daily vacuum removes them
(`Vacuum::FeedsVacuum`). Every authenticated request records the sign-in at
most once a day, as `UserTrackingConcern` does; when the sign-in before was
more than seven days ago, the member's feeds are rebuilt in the background
(`RegenerationWorker`). Signing in on the authorization page records a new
sign-in too.

While the home feed is being rebuilt, `GET /api/v1/timelines/home` answers
`206 Partial Content` with what the feed already holds and a
`Mastodon-Async-Refresh` header carrying `retry=5`, whose id the client polls at
`GET /api/v1_alpha/async_refreshes/:id`. The refresh lives in the coordination
Redis under Mastodon's key, `account:<id>:regeneration`. A member who follows no
one is answered the same way after their first follow, until that account's
posts are merged into the feed — for a follow request, once it is accepted, or
for a day at most.
A feed Redis does not hold at all, because Redis lost it or it was never built,
is rebuilt the same way the first time it is read.

When a notification reaches a member, eunha mails it where Mastodon's
`NotifyService#send_email!` would, written as `NotificationMailer` writes it:

 -  only a notification that was delivered, not one the member's notification
    policy filtered;
 -  only the types `NotificationMailer` has: `follow`, `follow_request`,
    `mention`, `quote`, `favourite` and `reblog`, each when the member has
    `notification_emails.<type>` on — by default all but `favourite` and
    `reblog`;
 -  only while nothing of the member's is listening — no streaming connection
    subscribed to their `user` or `user:notification` stream, and no web push
    subscription — unless they set `always_send_emails`;
 -  two minutes after the notification (`deliver_later(wait: 2.minutes)`), and
    only if the notification and its post still exist then and the member is
    still functional: confirmed, approved, not disabled, not suspended, moved
    or a memorial, and with the second factor their role asks for.

A mail names the other account in the subject, shows the post (its date in
the member's time zone) or the account, and links to it, to the follow
requests, to the notification settings and to unsubscribing. It carries
`List-ID: <type.username.domain>`, `List-Unsubscribe` with
`List-Unsubscribe-Post: List-Unsubscribe=One-Click`, the auto-reply
suppression headers every Mastodon mail has, and for a post in a conversation
`In-Reply-To` and `References` so mail clients thread it. English and Korean
are written; other locales get English.

The unsubscribe link is `/unsubscribe?token=…&type=…`, the address email
subscriptions use. `GET` asks, as `UnsubscriptionsController#show` does, and
`POST` — the page's button, or a mail client's one-click request — turns
`notification_emails.<type>` off. With the instance's
[`secret_key_base`](./instances#mastodon-s-secret-key-base), the token is
Mastodon's signed GlobalID of the user, good for a month, and one Mastodon
mailed works. Without it, the token names the user signed with a key derived
from the instance's VAPID key; it does not expire, it is still read once the
secret is configured, and a link Mastodon mailed is not recognised.


Time zones
----------

`users.time_zone` holds what Rails' `ActiveSupport::TimeZone[name]` finds:
one of the friendly names in `ActiveSupport::TimeZone::MAPPING` (`Seoul`,
`Eastern Time (US & Canada)`) or any IANA identifier (`Asia/Seoul`). As
Mastodon's `User` normalizes it, a name Rails would not find is stored as no
time zone at all, which reads as UTC. Eunha carries the mapping as Mastodon
4.7.1's Rails (8.1) has it, and the IANA zones through `chrono-tz`.

`GET /api/eunha/v1/preferences/time_zones` lists the appearance page's
choices, `SettingsHelper#time_zone_options`: every zone in the mapping,
ordered by its standard offset and then its name, labelled with its offset now
(`(GMT+09:00) Seoul`), with the IANA identifier the form submits.

The time zone is where Mastodon puts it to use: the times written into a
member's mail (`:with_time_zone`, as `Mar 05, 2026, 00:06 KST`), in the
failed second factor and new sign-in notices, the appeal decisions, and the
posts quoted in notification emails.
