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

The OAuth password grant, which Mastodon does not offer and eunha does, refuses
an account with a second factor, since the grant cannot carry one.

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
    (`reset_password_within`). `users.reset_password_token` holds the token's
    SHA-256, not the token; Devise keys its digest with `SECRET_KEY_BASE`,
    which eunha does not have, so a link one of them mailed does not work on
    the other.
 -  Setting the new password there, or with `PUT /auth/password`
    (`reset_password_token`, `password`, `password_confirmation`), ends every
    session, revokes every token and grant with their push subscriptions and
    streams (`User#revoke_access!`), and mails `password_change`. The person is
    not signed in afterwards (`sign_in_after_reset_password = false`).


Deleting the account
--------------------

`/account/delete`, and `DELETE /api/v1/accounts` for the settings page, are
Mastodon's `Settings::DeletesController`: the password (or, for an account
without one, the username) as the challenge, then `Account#mark_deleted!` —
`requested_deletion_at` and the suspension that hides the account at once — the
purge on a background task as `AccountDeletionWorker` runs it, with the username
kept reserved, and the browser signed out. The page warns a confirmed, approved
member that this is irreversible and their username stays taken; a member not
yet confirmed or approved is told instead how to fix their address and that the
username becomes available again, as upstream's page does. Eunha's
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
 -  `notification_emails` turns the staff mails eunha sends on or off:
    `report`, `pending_account`, `trends`, `appeal`, `end_of_support`, and
    `software_updates` (`none`, `critical`, `patch` or `all`).

`source[sensitive]` is kept under Mastodon's `default_sensitive` key in
`users.settings`; eunha used to write `web.default_sensitive`, which it still
reads.
