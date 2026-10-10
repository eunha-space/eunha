Rate limits
===========

Eunha limits requests as Mastodon does, in two ways: the `Rack::Attack`
throttles every request passes (*config/initializers/rack\_attack.rb*), and
the `RateLimiter` families that count what an account creates through the API
(*app/lib/rate\_limiter.rb*). Both are counted in Redis under the instance's
[key prefix](./redis), so every instance in a process has counts of its own
and one instance's clients cannot use up another's.


Throttles
---------

Each throttle counts requests by something about them — the address, the
user behind the access token, the token itself, an email address — in a fixed
period, and the request past its limit is answered `429`. The address is the
client's, read through trusted proxies as [moderation](./moderation)
describes, an IPv6 one cut to its /64:

| Throttle                                      | Counts                                                            | Limit | Period |
| --------------------------------------------- | ----------------------------------------------------------------- | ----: | -----: |
| `throttle_authenticated_api`                  | API requests, per user                                            | 1,500 |  5 min |
| `throttle_per_token_api`                      | API requests, per access token                                    |   300 |  5 min |
| `throttle_unauthenticated_api`                | API requests without a user's token, per address                  |   300 |  5 min |
| `throttle_api_media`                          | `POST /api/v*/media`, per user                                    |    30 | 30 min |
| `throttle_media_proxy`                        | `/media_proxy`, per address                                       |    30 | 10 min |
| `throttle_api_sign_up`                        | `POST /api/v1/accounts`, per address                              |     5 | 30 min |
| `throttle_authenticated_paging`               | requests with `page`, `min_id`, `max_id` or `since_id`, per user  |   300 | 15 min |
| `throttle_unauthenticated_paging`             | the same without a user's token, per address                      |   300 | 15 min |
| `throttle_api_delete`                         | deleting a post or undoing a boost, per user                      |    30 | 30 min |
| `throttle_oauth_application_registrations/ip` | `POST /api/v1/apps`, per address                                  |     5 | 10 min |
| `throttle_sign_up_attempts/ip`                | `POST /auth`, per address                                         |    25 |  5 min |
| `throttle_password_resets/ip`                 | `POST /auth/password`, per address                                |    25 |  5 min |
| `throttle_password_resets/email`              | the same, per email address                                       |     5 | 30 min |
| `throttle_email_confirmations/ip`             | confirmation mail asked for again, and `/auth/setup`, per address |    25 |  5 min |
| `throttle_email_confirmations/email`          | confirmation mail asked for again, per address or user            |     5 | 30 min |
| `throttle_auth_setup/email`                   | `/auth/setup`, per email address given                            |     5 | 10 min |
| `throttle_auth_setup/account`                 | `/auth/setup`, per signed-in user                                 |     5 | 10 min |
| `throttle_login_attempts/ip`                  | sign-ins, per address                                             |    25 |  5 min |
| `throttle_login_attempts/email`               | sign-ins, per pending sign-in's user or email address             |    25 | 1 hour |
| `throttle_password_change/account`            | password changes, per signed-in user                              |    10 | 10 min |

A request is counted by the throttles in that order until one of them has had
too many, and the ones after it do not count it, as `Rack::Attack` stops at
the first. Periods are fixed windows on the clock: a five-minute period ends
on a multiple of five minutes since the epoch, whenever its first request
came.

An email address is counted as the email blocks read it, lower-cased and
with the dots and any `+tag` taken out of its local part, so that
`F.o.o+1@example.com` and `foo@example.com` share a count, as they do since
Mastodon 4.7.3.

Mastodon's sign-in, password and setup pages are its own server-rendered
ones; eunha's are at other paths, so the throttles count eunha's: a sign-in is
`POST /account/login`, or `POST /oauth/authorize` with an email, a password or
a pending second-factor step; a password change is `POST /account/password`;
and `/auth/setup` is counted for a `POST` as for a `PUT`. A pending sign-in's
user, which Mastodon keeps in the session as `attempt_user_id`, is the one
eunha keeps for the form's `attempt` token. The streaming API is not
throttled, as Mastodon's separate streaming server is not.

The counts are kept where Mastodon's `Rails.cache` keeps them, in the
`cache` namespace:
`cache:rack::attack:<epoch / period>:<throttle>:<what is counted>`,
lower-cased, expiring when the period ends. A Redis that cannot be reached
counts nothing, as `Rails.cache` fails safe.


Families
--------

Some of what an account creates through the API counts towards a limit of
its own, per account:

| Family     | Counts                                                                                          | Limit | Period   |
| ---------- | ----------------------------------------------------------------------------------------------- | ----: | -------- |
| `statuses` | posts made with `POST /api/v1/statuses`, and their edits                                        |   300 | 3 hours  |
| `follows`  | follows and follow requests made with `POST /api/v1/accounts/:id/follow`, and hashtags followed |   400 | 24 hours |
| `reports`  | nothing: only its headers are given                                                             |   400 | 24 hours |

Only what is new counts: following someone already followed, a hashtag
already followed, an edit that changes nothing, a scheduled post or a post
replayed for its `Idempotency-Key` do not. Posts made any other way — a
scheduled post when it is published, an import, a post from another server —
do not either. Past the limit, the request is answered `429` and nothing is
created. The count is `rate_limit:<account id>:<family>:<epoch / period>`.


Headers
-------

An API response says how much is left of the throttle closest to its limit:

~~~~ http
X-RateLimit-Limit: 300
X-RateLimit-Remaining: 299
X-RateLimit-Reset: 2026-10-09T12:35:00.123456Z
~~~~

`X-RateLimit-Reset` is when the period ends, with the microseconds of the
moment it was asked, as Ruby's `iso8601(6)` writes it. Posting, editing,
boosting, following an account or a hashtag, and reporting say their
family's count instead, unless a throttle has fewer left. A path no route
answers says nothing, and neither do pages outside the API.

A request a throttle refuses is answered with that throttle's limit, nothing
remaining, and

~~~~ json
{"error": "Too many requests"}
~~~~

and so is one a family refuses, with the family's. The message is
Mastodon's `errors.429` in the instance's `default_locale`, whatever the
request asked for, as Mastodon gives it outside any controller's locale:
eunha has it in English and Korean (요청 횟수 제한에 도달했습니다), and gives
any other locale the English one.


Turning them off
----------------

`rate_limits = false` in `[limits]` turns both off for an instance, for a
load test or a deployment that limits requests in front of eunha instead.
Mastodon has no such switch, and eunha's own tests are the reason it has one:
they come from one address and post far more than a person does, so each test
instance has the limits off but for the tests of the limits themselves.
