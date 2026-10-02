Email subscriptions
===================

Mastodon 4.7 lets chosen local accounts put a form on their profile that a
visitor without an account fills in to receive the account's public posts by
email. Eunha implements it on Mastodon's own `email_subscriptions` table, with
the same switches.


Turning it on
-------------

Three things have to agree before an account can be subscribed to:

1.  The instance offers the feature. Mastodon's `DISABLE_EMAIL_SUBSCRIPTIONS`
    environment variable is eunha's per-instance `email_subscriptions` flag,
    on by default:

    ~~~~ toml
    [instance]
    email_subscriptions = false   # nothing can enable the feature
    ~~~~

    It is read when the instance starts: a tenant whose file changes is
    restarted on `SIGHUP` (see [instances](./instances.md)), and a single
    instance needs a restart.

2.  An administrator enables it. The `email_subscriptions` site setting
    (Mastodon's `Setting.email_subscriptions`, off by default) is set from
    **Moderation → Email newsletters**, which asks for Mastodon's two
    agreements first: that the instance will send a good deal more email, and
    that its privacy policy and terms of service now cover the addresses
    collected. The page needs the `manage_settings` permission.

3.  The account may and wants to. Its role must carry
    `manage_email_subscriptions` (1 << 22), and its user must have turned
    the feature on. Eunha has no role editor (see [Invites](./invites.md)), so
    granting the permission is an `UPDATE`:

    ~~~~
    UPDATE user_roles SET permissions = permissions | (1 << 22) WHERE id = …;
    ~~~~

    The user turns it on in **Settings → Send posts via email**, which
    appears only once the first two hold and the role allows it. An
    administrator can turn it on or off for them from the account's page under
    Email newsletters, as Mastodon's admin can.

While all three hold, the account's API entity carries
`"email_subscriptions": true` (and `false` for every other account; the
attribute is absent while the feature is off), and eunha's profile page shows
signed-out visitors the form. Every serialized account carries the
attribute, as with Mastodon's serializer: the account endpoints, and the
accounts embedded in statuses and notifications.


What a subscriber goes through
------------------------------

`POST /api/v1/accounts/:id/email_subscriptions` with an `email`, which asks
for no token, as upstream's does. It answers an empty 404 whenever one of the
three switches above is off or the account is suspended or deleting, a 422 in
Mastodon's `ValidationErrorFormatter` shape (`ERR_BLANK`, `ERR_INVALID`,
`ERR_TOO_LONG`, `ERR_TAKEN`, and `ERR_UNREACHABLE` or `ERR_BLOCKED` from the
same MX check and email domain blocks sign-ups get), or `{}`. The address is
squished and lowercased first, and stored with the request's locale.

The subscriber is mailed a link to `/email_subscriptions/confirmation`;
following it confirms the subscription. Nobody is mailed posts until then, and
a subscription nobody confirms within seven days is deleted by a daily pass,
as Mastodon's `UserCleanupScheduler` does.

Every email carries a link to `/unsubscribe`, which asks before unsubscribing,
and `List-Unsubscribe` and `List-Unsubscribe-Post` headers so that a mail
client can unsubscribe in one click. The link's token is the subscription's
confirmation token: Mastodon signs a GlobalID with `secret_key_base`, which
eunha does not have (the `email-subscription-unsubscribe-token` divergence).
A link mailed by Mastodon before the switch to eunha therefore no longer
works, and one mailed by eunha does not expire after a month as Mastodon's
does.


Distribution
------------

A post goes into its account's next batch when it is public and not a reply to
somebody else (a self-reply counts), and the account offers subscriptions. The
first post in a batch starts a five-minute wait, Mastodon's
`EMAIL_DISTRIBUTION_DELAY`; posts made during it join the same email. The batch
is the Redis set `email_subscriptions:<account id>:next_batch`, kept an hour,
and the wait is held by `email_subscriptions:<account id>:distribution`, both
on the coordination Redis (see [Shared Redis](./redis.md)). When the wait is
over, the batch's posts that are still public, not replies to others and not
boosts are mailed, newest first, to each confirmed subscriber.

The wait runs inside the eunha process rather than in a durable queue (the
`email-distribution-in-process` divergence). An instance stopped during it
leaves the batch in Redis for its next post to send along; a process that dies
outright holds the next batch back for at most eleven minutes.

The emails go out through the instance's `[smtp]` settings, from its `from`
address, in English or, for a subscriber who signed up in Korean, with
Mastodon's Korean strings where it has them. The footer links the privacy
policy (eunha's `/about`) and carries the **additional footer text** an
administrator sets on the Email newsletters page, Mastodon's
`Setting.email_footer_text`.


The admin API
-------------

Mastodon's admin pages for this are web forms; eunha's admin is a single-page
app, so it serves what they do under `/api/v1/admin/email_subscriptions` (the
`email-subscriptions-rest-api` divergence). Every call needs
`manage_settings`, which is upstream's `EmailSubscriptionPolicy`, and the
`admin:read` or `admin:write` scope.

| Call                                      | What it does                                                                                                                      |
| ----------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| `GET /api/v1/admin/email_subscriptions`   | Whether the feature is available and enabled, the footer text, the roles that may use it, and the local accounts with subscribers |
| `POST …/setup`                            | Enable it, given `agreement_email_volume` and `agreement_privacy_and_terms`; 404 when the instance does not offer it              |
| `POST …/disable`                          | Turn the site setting off                                                                                                         |
| `POST …/purge`                            | Delete every subscription                                                                                                         |
| `PUT …/additional_footer_text`            | Set `email_footer_text`                                                                                                           |
| `GET …/accounts/:id`                      | One account's status and subscriber count                                                                                         |
| `GET …/accounts/:id/subscriptions`        | Its subscribers, `Link`-paginated                                                                                                 |
| `POST …/accounts/:id/enable`, `…/disable` | Turn the account's own setting on or off                                                                                          |
| `DELETE …/:id`                            | Remove one subscriber                                                                                                             |

An account's own switch is `GET` and `PUT /api/eunha/v1/email_subscriptions`
(`read:accounts` and `write:accounts`), since Mastodon keeps it on a web
settings page with no API.
