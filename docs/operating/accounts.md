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
