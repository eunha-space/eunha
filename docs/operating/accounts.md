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


The OAuth server
----------------

`/.well-known/oauth-authorization-server` describes the OAuth server as
Mastodon's does (RFC 8414): its endpoints, the scopes Doorkeeper is configured
with, the `code` response type, the `authorization_code` and
`client_credentials` grants, `client_secret_basic` and `client_secret_post`,
PKCE with `S256`, and `/api/v1/apps` as the non-standard
`app_registration_endpoint`.

The authorization page takes Doorkeeper's parameters: `response_type=code`
(anything else, or none, is refused), `state`, which comes back with the code,
`response_mode` (`query`, `fragment` or `form_post`), and a PKCE
`code_challenge` with `code_challenge_method=S256`, whose `code_verifier`
`/oauth/token` then requires. A client whose redirect URI is
`urn:ietf:wg:oauth:2.0:oob` is shown the code at `/oauth/authorize/native` to
copy.

`/oauth/token`, `/oauth/revoke` and `/oauth/introspect` find the client as
Doorkeeper does: by HTTP Basic, taken as it decodes with no URL-decoding, or
by `client_id` and `client_secret` in the request, and a public client
(`confidential` false) by its id alone. Giving a secret both ways, or naming
two clients, is 400 `invalid_request`; an unknown client or a wrong secret is
401 `invalid_client`. The token endpoint follows Mastodon's Doorkeeper
configuration: tokens never expire and come without a refresh token, a used
code is revoked rather than deleted, and `reuse_access_token` hands back the
newest unrevoked token the client already holds for the same owner and scopes,
with its original `created_at`, rather than making another. The client
credentials grant without `scope` gets the default scopes the application has,
`read`. Errors are Doorkeeper's: a missing parameter is `invalid_request`, a
code that is unknown, expired, used, another client's, or given with another
redirect URI or a wrong PKCE verifier is `invalid_grant`, both 400.

`POST /api/v1/apps` validates the application as `Doorkeeper::Application`
does under Mastodon's configuration, and answers 422
`Validation failed: …` naming everything wrong: a name, at most 60
characters; redirect URIs, a line each or an array, which may not be left
out, each `urn:ietf:wg:oauth:2.0:oob` or an absolute URI without a fragment,
not `data:`, `javascript:` or `vbscript:`, and `http` as well as `https`;
scopes among those Doorkeeper is configured with, separated by spaces, `read`
when none are given; and a website, if any, an `http` or `https` URL.

A signed-out browser is asked to sign in on the authorization page itself,
where Mastodon sends it to its sign-in page; signing in starts a session and
comes back to the page with a 302. A signed-in user is shown Mastodon's
choice: the client's name, the permissions it asks for grouped as Mastodon
groups them, and **Authorize** and **Deny**, with who is signed in and a
logout that comes back to the page. The page answers at once, without asking,
for the instance's own app (`superapp`), and for a confidential client the
user already holds an unrevoked token of with the same scopes, unless the
request says `force_login=true`. The code, and a denial, come back with a
302, as Rails redirects. **Deny** posts `_method=delete`, which is
`DELETE /oauth/authorize`: from a signed-in browser it turns the client away,
sending `access_denied`, with its `state`, to a redirect URI it registered (an
unregistered one is refused, where Doorkeeper would follow it).

Before any of that, the page, the authorize button and deny each run
Mastodon's `require_functional!` for a signed-in user who is not functional:
one whose role requires two-factor authentication they lack is taken through
setting it up, then back to the page; one pending approval, a memorial or
moved goes to the account page; and one with an unconfirmed address to
`/auth/setup`. Each keeps the page to come back to.

`POST /oauth/revoke` is RFC 7009 revocation as Doorkeeper answers it. The
request names its client, by HTTP Basic or in the request, with the client's
secret unless the application is a public one (`confidential` false); without
that it is refused with 403 `unauthorized_client`, and a secret given both ways
is 400 `invalid_request`. A token issued to another client is refused the same
way; one issued to no client may be revoked by any. A token nobody holds, or
one already revoked, is a 200 `{}`. The token is looked up as an access token,
then as a refresh token, or only as a refresh token with
`token_type_hint=refresh_token`.

`GET /oauth/token/info` describes the bearer token as Doorkeeper does (its
scopes, owner, application and remaining lifetime), or answers 401
`invalid_token` saying whether it is unknown, revoked or expired.
`POST /oauth/introspect` is RFC 7662 introspection: a client, by HTTP Basic or
its id and secret in the request, or a bearer token other than the one asked
about, learns whether a token of its own application is active, with its
scope, client and issue time, and gets `{"active":false}` about anything else.
The `/oauth/applications` pages answer 403 to everyone, as Mastodon configures
Doorkeeper's `admin_authenticator`; authorized apps are under
[their own heading](#authorized-apps-and-sign-in-history).

`GET` and `POST /oauth/userinfo` is OpenID Connect's UserInfo endpoint, for a
token with the `profile` scope (401 without a token, 403 without the scope):
`iss` the instance's root URL, `sub` the actor's URI, `name`,
`preferred_username`, `profile` the profile page, and `picture` the avatar.


Push notifications
------------------

`POST /api/v1/push/subscription` destroys the token's subscription and makes
a new one, with a new id, under the member's `lock:push_subscription:<user>`
lock; a request that finds it held is a 503. The endpoint must be an `http` or
`https` URL and the keys must be able to encrypt a message, or the request is a
422 (the old subscription stays destroyed, as in Mastodon). The endpoints take
JSON or a form (`subscription[keys][auth]`, `data[alerts][mention]`).

A subscription's `data` is stored as given: the `policy` and the
`alerts` named in Mastodon's notification types, nothing else, and no
defaults. An alert that is not given is off. `PUT` replaces the data
wholesale, and blank data is stored as `{}`, whose policy reads `all`. The
response reads each alert back cast as Rails casts a boolean, so a form's
`"1"` reads `true`.

A notification is pushed to each of the recipient's subscriptions whose alert
for its type is on and whose policy allows the sender: `all`, `followed`
(the recipient follows the sender), `follower` (the sender follows the
recipient) or `none`. Every type can be pushed, the staff types
(`admin.sign_up`, `admin.report`), `moderation_warning` and
`severed_relationships` among them.

A push is sent as Mastodon's `Web::PushNotificationWorker` sends it:
`aes128gcm` with RFC 8292 VAPID for a subscription made with `standard`,
`aesgcm` otherwise, with a 48-hour `TTL`, `Urgency: normal`, and an
`Unsubscribe-URL`. That URL, `DELETE /api/web/push_subscriptions/:token`,
removes the subscription for whoever calls it while the token is good, 48
hours. An endpoint answering a 4xx other than 408 or 429 has the subscription
removed; any other failure is tried again, up to five times.

When the push is sent, not when it is queued, the worker checks again: a
notification last updated more than 48 hours ago, one whose post or other
activity is gone, or one the subscription no longer wants is dropped, and a
subscription that is no longer valid is removed. The payload is rendered
then too, as `Web::NotificationSerializer` renders it: the subscription's
access token, the subscriber's locale, the notification's id and type, the
sender's avatar, a title from `notification_mailer.<type>.subject` naming the
sender in the subscriber's locale (English or Korean; any other locale gets
the English subject), and a body that is the post's content warning or text,
or else the sender's bio, without tags and cut to 140 characters. A type
Mastodon has no subject for, `annual_report`, is titled as Rails titles a
missing translation; the collection types are titled with what happened
instead (see *divergences.toml*).


Multiple accounts in the web client
-----------------------------------

The account menu in the desktop sidebar and mobile drawer lists accounts signed
in on this instance in this browser. Choose **Add account** to sign in through
OAuth without removing the current account, then choose an account in the menu
to switch to it. It asks with `force_login=true`, so the authorization page
always asks; when the browser is still signed in to the account pages, use the
page's logout to sign in as the other account. Signing in again to the same
account replaces its saved token. Only completed OAuth logins are saved in the
switcher. Legacy `eunha:token` logins are no longer read or migrated; sign in
again. Tokens live only in `eunha:accounts`, and `eunha:active-account` selects
the account used for API requests.

Accounts and their OAuth tokens are kept in this browser's local storage, scoped
to the instance's origin. Switching reloads the home page and clears the
previous account's cached profile, open composer and streaming connections.
Other tabs on the same origin follow the switch. Theme and layout preferences
remain shared.

**Sign out** removes only the current account from this browser and selects a
remaining account, if there is one. It does not revoke the OAuth authorization;
use **Authorized apps** to revoke it on the server.


Sessions
--------

A sign-in on the account pages starts a session in `session_activations`, as a
Mastodon web sign-in does: a random `session_id` in the `account_session`
cookie, the browser's address and user agent, and an access token for the
instance's web app with `read write follow`. Signing out ends it; at most ten
are kept, the oldest purged first. Changing the password ends every other
session.

A page that needs a session, such as an archive's download link, sends a
signed-out browser to `/account/login` with a `302` and remembers where it
was going in the `account_return_to` cookie, Devise's `user_return_to`;
signing in then lands there rather than on `/account`.

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
The “Authorized on” date is the application's creation date, as on Mastodon's
page. Application creation records both Rails timestamps. Migration 027 repairs
older Eunha rows that omitted them, using the earliest recorded token when
possible; the original creation time cannot be recovered exactly.

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

The sign-up and sign-in forms mark the email field with
`autocomplete="username"`, because the email address is the login identifier
password managers should save. The public handle on the sign-up form uses
`autocomplete="off"`, and both password fields use `new-password`. Password
managers may apply their own heuristics, so these hints cannot guarantee what
every manager saves.

A sign-up is saved the moment it is sent, as Mastodon's `AppSignUpService` and
`Auth::RegistrationsController` save it: the account with its signing key, and
a `users` row with no `confirmed_at`, its `confirmation_token` and
`confirmation_sent_at`, whether it is approved (`User#set_approved`), the
address it came from (`sign_up_ip`), its reason (`user_invite_requests`) and
the app it came through (`created_by_application_id`). An invite's use is
counted then, and the `account.created` webhook goes out. The username and the
address are taken from that moment, by a user confirmed or not. The link the
mail carries confirms the user within two days of being sent (Devise's
`confirm_within`); confirming welcomes an approved user in (the welcome mail,
`admin.sign_up` to staff) or puts one awaiting approval before the staff. A
user whose link went out a week ago and was never followed is removed by the
daily user cleanup, as Mastodon's `Scheduler::UserCleanupScheduler` removes it.

There are two ways in, as on Mastodon:

 -  `POST /api/v1/accounts`, for an app with a client-credentials token
    carrying `write:accounts` (a user's token is refused with Mastodon's
    `This method requires an client credentials authentication`). It answers
    with an access token for the new user, with the app's scopes. Until the
    address is confirmed that token authenticates, but where Mastodon's
    `require_user!` runs it is refused with `403` and
    `Your login is missing a confirmed e-mail address`. The link mailed to a
    user who signed up through an app carries `redirect_to_app=true`, and
    following it sends the browser to the app's first redirect URI as it
    stands; the app learns the address is confirmed by asking
    `GET /api/v1/emails/check_confirmation` with its token. Without an app
    the link leads to the sign-in page with a confirmation result, even when
    another account is already signed in. This keeps the account switcher's
    current account from hiding the result and lets the user sign in to the
    newly confirmed account.
 -  `POST /auth`, which the sign-up page posts to (form-encoded or JSON, no
    token). The new user is signed in and sent to `/auth/setup`, Mastodon's
    `Auth::SetupController`: it names the address the link went to and lets the
    user correct it and have the link sent again. An unconfirmed user who signs
    in later is sent there too, from the sign-in page or the authorization
    page an app sends them to, and so is one who opens the account pages.

`POST /api/v1/emails/confirmations` is Mastodon's: for the app the user signed
up through, while the address awaits confirmation, it sends the link again,
and with `email` it puts the address right first; the new address waits in
`unconfirmed_email` and the link goes to it, twice, as Devise sends it: once
for the address held back, once as the resend. Correcting the address on
`/auth/setup` does the same.
`GET /api/v1/emails/check_confirmation` answers whether the address is
confirmed.

Both check what Mastodon 4.7's `User` validates, and answer a refusal as
`ValidationErrorFormatter` does: `Validation failed: …` with per-attribute
`ERR_*` codes in `details`, the attribute named in the message as Mastodon's
locale names it (`E-mail address`, `Service agreement`, `Reason`). Every
validation runs, the moderation ones among them, so a refusal names
everything wrong with the sign-up at once. The username is kept as entered,
case and all; whether it is taken ignores case, as every lookup by username
does.

 -  The username is letters, digits and underscores, at most 30 characters,
    and not taken by another local account in any case (`ERR_TAKEN`), nor
    reserved by a username block (`ERR_RESERVED`).
 -  The address is not taken by another user, confirmed or not (`ERR_TAKEN`),
    resolves to a mail server, and is not under an email domain block or a
    canonical email block.
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
 -  `locale`, as `AppSignUpService` takes it, becomes `users.locale` when it
    is one of Mastodon's interface locales (`I18n.available_locales`, spelled
    as Mastodon spells them, such as `en` or `pt-BR`); any other value is
    dropped rather than refused, as `User` normalizes it. The web sign-up
    saves the locale the page was asked in instead (`lang`, then
    `Accept-Language`, then the instance's default locale), whatever the
    form says.

Registrations closed with no invite good for use, or an IP block on signing
up, refuse the sign-up with `403`, as `check_enabled_registrations` does.

Eunha used to keep sign-ups in a table of its own, `eunha.pending_signups`,
until their link was followed. `eunha migrate` turns those still within their
day into unconfirmed users before the migration that drops the table, keeping
the token their mail carries, so the link they were sent still works; the rest
go with it (see [Migrations](./migrations.md)).


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
    list clears it. `locale` is the language eunha writes mail in; one that is
    not among Mastodon's interface locales is cleared.
 -  `time_zone` is the zone the times in mail are written in (see
    [Time zones](#time-zones)).
 -  `aggregate_reblogs` (on unless turned off) groups boosts in the home
    timeline and lists (see [Boosts in timelines](#boosts-in-timelines)).
 -  `display_media` (`default`, which hides media marked sensitive,
    `show_all` or `hide_all`), `expand_content_warnings` and `auto_play`
    are the appearance page's `web.display_media`,
    `web.expand_content_warnings` and `web.auto_play`. Apps read them from
    `GET /api/v1/preferences` as `reading:expand:media`,
    `reading:expand:spoilers` and `reading:autoplay:gifs`. The web client
    applies them as Mastodon's does; see [How posts are
    shown](#how-posts-are-shown).
 -  `notification_emails` turns the mails eunha sends on or off: the
    notification emails `follow`, `follow_request`, `reblog`, `favourite`,
    `mention` and `quote` (see [Notification emails](#notification-emails)),
    and for staff `report`, `pending_account`, `trends`, `appeal`,
    `end_of_support`, and `software_updates` (`none`, `critical`, `patch` or
    `all`). `always_send_emails` mails notifications even while the member is
    online.

`source[sensitive]` is kept under Mastodon's `default_sensitive` key in
`users.settings`; eunha used to write `web.default_sensitive`, and before
that `sensitive`, `privacy`, `language` and `quote_policy` for the other
posting defaults, which it still reads when Mastodon's key is missing.
`GET /api/v1/preferences` gives as the posting language the one chosen,
else the member's interface language, else the request's
(`preferred_posting_language`).


How posts are shown
-------------------

The web client loads the signed-in account's `display_media`,
`expand_content_warnings` and `auto_play` when it starts, keeps a copy per
account so the next load paints with them, and takes a new value as soon as
the settings page saves it. Switching accounts reloads the page with the other
account's preferences. Signed out, it uses Mastodon's defaults: sensitive
media hidden, content warnings folded, and nothing animated until hovered.
Each post on a timeline, a thread, a profile, the notifications and search
follows them, as Mastodon's web client does:

 -  Media starts shown with `show_all`, hidden with `hide_all`, and with
    `default` hidden only when the post is marked sensitive (the server
    already marks every post of an account forced sensitive). A custom filter
    that blurs media hides it whatever the setting. The cover says why
    (“Sensitive content”, “Media hidden”, or the filter's name), and Hide puts
    it back. Hidden media is drawn from its blurhash alone: none of it is
    loaded until it is shown.
 -  A post with a content warning starts folded, or open with
    `expand_content_warnings`. The same goes for a quoted post.
 -  With `auto_play`, GIFs (`gifv` attachments), avatars, profile headers
    and custom emoji animate. Without it, a GIF plays while the pointer is
    over it, an avatar shows its `avatar_static` until hovered, a header
    shows its `header_static`, and a custom emoji shows its `static_url`
    until the pointer is over what it is part of: a post's text, its content
    warning, a name, a poll option, a profile's header, or an announcement.

A `:shortcode:` is drawn as an image only when the post, account, poll or
announcement it belongs to lists that shortcode in its `emojis`, and only in
text: a shortcode inside a tag's attribute stays as it is. Its `alt` and
`title` are the shortcode, as in Mastodon. An emoji whose URL is not an
`http` or `https` one is left as text.

The composer suggests the server's custom emoji once a `:` and two more
characters are typed, five at most, best match first, and offers a picker of
them by category beside the media button. Both list what
`GET /api/v1/custom_emojis` does; Mastodon's also offer Unicode emoji, which
eunha's do not (`web-custom-emoji-only-picker`). The home column's
announcements button opens the published announcements, newest first, marks the
one on screen read, and shows their reactions, which can be added to from the
same picker.


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

What may enter a feed is decided as it is written, by Mastodon's
`FeedManager` filters (blocks, mutes, domain blocks, hidden boosts, exclusive
lists, languages and replies); reading the home or a list timeline answers
the posts the feed holds that are not deleted, filtering nothing more.

Nothing else rebuilds a feed. Reading one never fills it: a new member's empty
feed is answered `200` and empty, and a feed Redis lost stays empty, apart from
what is posted afterwards, until the member's next return after a week away.
The feeds are kept under Mastodon's keys alone (`feed:home:<id>`,
`feed:list:<id>`), so a Mastodon process sharing the Redis reads and feeds
the same ones. A follow merges the followed account's recent posts into the
home feed and the lists that hold it (`MergeWorker`), an unmute does the same,
and an unfollow takes them out again (`UnmergeWorker`), as does removing an
account from a list; these run on the [job queue](./jobs.md). Muting an account
takes its posts, boosts of them and posts mentioning it out of the home feed
and every list (`MuteWorker`); blocking it, or muting its notifications too,
does that and also deletes its notifications, notification requests and the
conversations it is in (`BlockWorker`).

A mute for a while is lifted when it expires by the `DeleteMuteWorker` queued
with it, which unmutes as above. Until that job has run the mute stands:
nothing that reads mutes looks at `expires_at`, as nothing in Mastodon does.
Re-muting queues a job for the new expiry, and the old one finds the mute
unexpired and leaves it. Timed mutes that came without their job (a database
from before eunha queued them, or one imported from Mastodon, whose jobs stayed
in its Sidekiq) are given one by migration 030 and by `eunha import-mastodon`.

Unfollowing a hashtag takes its posts out of the home feed (`TagUnmergeWorker`),
except those that are the member's own, from accounts they follow, or tagged
with another hashtag they still follow. Unfollowing an account, however it
happens, also removes the member's endorsement of it, as Mastodon's `Follow`
does when it is destroyed.

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


Automated post deletion
-----------------------

A member can have their old posts deleted automatically, by a row in
`account_statuses_cleanup_policies`: posts older than `min_status_age`
seconds go, except those the policy keeps — by default direct posts, pinned
posts and posts the member favourited or bookmarked themselves, and as chosen
polls, posts with media, and posts with at least `min_favs` favourites or
`min_reblogs` boosts. Mastodon edits the policy on its web settings page
(`/statuses_cleanup`), not through its REST API, and eunha's web client has no
page for it yet; a policy saved by Mastodon on the same database is carried
out all the same.

Every minute, Mastodon's `AccountsStatusesCleanupScheduler` deletes up to five
posts per job thread (`[workers] job_workers × job_concurrency`), at most 300,
and at most five of one member's at a time, going round the members from where
it stopped last time and then back to those who still had posts to delete. It
skips its turn while the `default` job queue is more than five seconds behind,
deliveries ten seconds, or the `pull` queue five minutes. Each member's
progress is remembered in Redis for two weeks (`account_cleanup:<id>`), so
posts already looked at and kept are not looked at again; taking back a
favourite, bookmark or pin that kept a post makes it a candidate again. Each
post is deleted as its author deleting it would, and other servers are told.


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
