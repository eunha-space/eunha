#!/usr/bin/env python3
"""Compare eunha's API responses against a live Mastodon's.

The other checks in this repo compare eunha to a *reading* of Mastodon — its
serializers, its callbacks, its scopes. This one asks Mastodon itself: the same
request goes to both servers, and the two responses are compared field by field.
A rule misread while writing a test is a rule wrong in the test too; this cannot
make that mistake, because nothing here encodes what the answer should be.

Two things are compared. **Shape** — which fields exist and what kind each holds
— everywhere, because ids, hostnames, timestamps and tokens necessarily differ
between two servers and asking them to agree would drown the signal. And
**values**, but only for fields where two servers genuinely should agree: the
constants and limits an instance advertises about itself, listed in
`COMPARED_VALUES` below.

That second part exists because the first was not enough. eunha advertised
`max_display_name_length: 30` where Mastodon says 40 — so a client would have
refused a display name this server accepts — and a shape comparison passed it,
both being integers.

Usage:
    scripts/differential_test.py --eunha http://localhost:3001 \
                                 --mastodon http://localhost:3000 \
                                 --eunha-token TOKEN --mastodon-token TOKEN
"""
import argparse
import json
import sys
import time
import urllib.error
import urllib.request

import differential_flows

# Endpoints worth comparing: what a client touches on an ordinary session.
# Each is (method, path, needs_auth).
ENDPOINTS = [
    ("GET", "/api/v1/instance", False),
    ("GET", "/api/v2/instance", False),
    ("GET", "/api/v1/instance/rules", False),
    ("GET", "/api/v1/instance/peers", False),
    ("GET", "/api/v1/custom_emojis", False),
    ("GET", "/api/v1/accounts/verify_credentials", True),
    ("GET", "/api/v1/preferences", True),
    ("GET", "/api/v1/filters", True),
    ("GET", "/api/v2/filters", True),
    ("GET", "/api/v1/lists", True),
    ("GET", "/api/v1/markers?timeline[]=home", True),
    ("GET", "/api/v1/notifications", True),
    ("GET", "/api/v1/timelines/home", True),
    ("GET", "/api/v1/timelines/public", False),
    ("GET", "/api/v1/conversations", True),
    ("GET", "/api/v1/bookmarks", True),
    ("GET", "/api/v1/favourites", True),
    ("GET", "/api/v1/follow_requests", True),
    ("GET", "/api/v1/mutes", True),
    ("GET", "/api/v1/blocks", True),
    ("GET", "/api/v1/domain_blocks", True),
    ("GET", "/api/v1/endorsements", True),
    ("GET", "/api/v1/featured_tags", True),
    ("GET", "/api/v1/suggestions", True),
    ("GET", "/api/v2/suggestions", True),
    ("GET", "/api/v1/trends/tags", False),
    ("GET", "/api/v1/trends/statuses", False),
    ("GET", "/api/v1/trends/links", False),
    ("GET", "/api/v1/announcements", True),
    ("GET", "/api/v1/notifications/policy", True),
    ("GET", "/api/v1/scheduled_statuses", True),
]


def request(base, path, token, method="GET", body=None, extra_headers=None):
    """Returns (status, parsed_body_or_None, header_names)."""
    data = None
    if body is not None:
        data = json.dumps(body).encode()
    req = urllib.request.Request(f"{base}{path}", method=method, data=data)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    req.add_header("Accept", "application/json")
    # Mastodon's production environment sets `config.force_ssl`, and would
    # answer 301 to a plain request. This is what a reverse proxy in front of it
    # would send, and is how it is actually deployed.
    req.add_header("X-Forwarded-Proto", "https")
    for key, value in (extra_headers or {}).items():
        req.add_header(key, value)
    try:
        with urllib.request.urlopen(req, timeout=30) as response:
            raw = response.read()
            status, headers = response.status, dict(response.headers)
    except urllib.error.HTTPError as e:
        raw, status, headers = e.read(), e.code, dict(e.headers)
    except Exception as e:
        return None, {"__transport_error__": str(e)}, {}
    try:
        return status, json.loads(raw), headers
    except json.JSONDecodeError:
        return status, {"__not_json__": raw[:200].decode("utf-8", "replace")}, headers


# Fields whose value two servers should agree on: limits and constants an
# instance states about itself, rather than anything derived from its data.
# Matched against the dotted path a field sits at, so `configuration.accounts.
# max_note_length` is compared and `accounts.note` is not.
COMPARED_VALUES = (
    "configuration.accounts.",
    "configuration.statuses.",
    "configuration.polls.",
    "configuration.media_attachments.",
    "configuration.translation.",
    "configuration.reactions.",
)


# Field names whose values two servers cannot agree on, however identical the
# request: identifiers, the hostnames they are built from, and times. Matched on
# the last segment of the path, so `poll.id` and `account.id` are both covered.
VOLATILE_FIELDS = frozenset({
    "id", "uri", "url", "in_reply_to_id", "in_reply_to_account_id",
    "account_id", "status_id", "reblog_of_id", "quoted_status_id",
    "created_at", "updated_at", "edited_at", "expires_at", "last_status_at",
    "published_at", "scheduled_at", "muting_expires_at", "verified_at",
    "acct", "username", "display_name", "domain", "name", "website",
    "token", "client_id", "client_secret", "vapid_key", "public_key",
    "href", "src", "preview_url", "remote_url", "preview_remote_url",
    "text_url", "avatar", "avatar_static", "header", "header_static",
    "blurhash", "group_key", "most_recent_notification_id",
    "sample_account_ids", "page_min_id", "page_max_id",
    "latest_page_notification_at", "content", "text", "emojis", "meta",
    "action_taken_at",
    # Ids by another name. Where which ones matters — the statuses a report
    # names — a flow compares them by the fixture names they stand for.
    "status_ids", "rule_ids", "account_ids", "last_read_id", "application_id",
    # The instance's VAPID key, as each server generated its own.
    "server_key",
    # Totals over everything an instance has ever done, rather than anything
    # this request decided. The two databases hold different histories, so these
    # differ by construction and would drown the fields that do not.
    "statuses_count", "followers_count", "following_count", "favourites_count",
    "reblogs_count", "replies_count", "quotes_count", "votes_count",
    "voters_count", "notifications_count", "usage", "user_count", "status_count",
    "domain_count",
})


def comparable_value(path):
    """Whether two servers, given the same request, should produce the same value.

    Used for writes and interactions, where both servers act on identical input
    — so a count, a flag or a limit that differs is a real difference. This is
    where `voted` and `voters_count` were wrong on a poll, both invisible to a
    comparison that only checked which fields exist and of what type.
    """
    last = path.rsplit(".", 1)[-1].removesuffix("[]")
    return last not in VOLATILE_FIELDS


def values_for_comparison(value, prefix="", out=None):
    """Flatten a response to {path: scalar}, keeping only comparable fields."""
    out = {} if out is None else out
    if isinstance(value, dict):
        for k, v in value.items():
            values_for_comparison(v, f"{prefix}.{k}" if prefix else k, out)
    elif isinstance(value, list):
        # Only the first element: two servers may legitimately hold different
        # numbers of things, and that shows up as a shape difference already.
        if value:
            values_for_comparison(value[0], f"{prefix}[]", out)
    elif comparable_value(prefix):
        out[prefix] = value
    return out


def compare_values(name, left, right, findings):
    left_values, right_values = (
        values_for_comparison(left),
        values_for_comparison(right),
    )
    for field in sorted(set(left_values) & set(right_values)):
        if left_values[field] != right_values[field]:
            findings.append(
                f"{name}: `{field}` is {left_values[field]!r} on eunha, "
                f"{right_values[field]!r} on Mastodon"
            )


def compares_value(path):
    return any(path.startswith(prefix) for prefix in COMPARED_VALUES)


def values_at(value, prefix="", out=None):
    """Flatten a response to {dotted.path: scalar} for the fields we compare."""
    out = {} if out is None else out
    if isinstance(value, dict):
        for k, v in value.items():
            values_at(v, f"{prefix}.{k}" if prefix else k, out)
    elif not isinstance(value, list):
        if compares_value(prefix):
            out[prefix] = value
    return out


def shape(value, depth=0):
    """A value's structure, ignoring content.

    Two servers cannot agree on ids, hostnames or timestamps, and should not be
    asked to. They can agree on which fields exist and what kind each holds.
    """
    if depth > 6:
        return "..."
    if isinstance(value, dict):
        return {k: shape(v, depth + 1) for k, v in sorted(value.items())}
    if isinstance(value, list):
        # A list's shape is its first element's: an empty list on one side says
        # nothing about disagreement, only that the fixture differed.
        return [shape(value[0], depth + 1)] if value else []
    if value is None:
        return "null"
    return type(value).__name__


def compare(path, left, right, findings, where=""):
    """Walk two shapes together, recording where they differ."""
    if isinstance(left, dict) and isinstance(right, dict):
        for key in sorted(set(left) | set(right)):
            at = f"{where}.{key}" if where else key
            if key not in right:
                findings.append(f"{path}: eunha sends `{at}`, Mastodon does not")
            elif key not in left:
                findings.append(f"{path}: Mastodon sends `{at}`, eunha does not")
            else:
                compare(path, left[key], right[key], findings, at)
    elif isinstance(left, list) and isinstance(right, list):
        if left and right:
            compare(path, left[0], right[0], findings, f"{where}[]")
    elif left != right:
        # `null` against a type is not a disagreement: a nullable field simply
        # held a value on one server and not the other.
        if "null" not in (left, right):
            findings.append(f"{path}: `{where}` is {left} on eunha, {right} on Mastodon")


# Things a client does, rather than reads. Each is (method, path, body, name);
# the response entity is compared like any other, and the status code with it.
#
# These matter more than the reads: a GET returns what the server already holds,
# while a POST is the server deciding what to make of a request. eunha and
# Mastodon can agree on every timeline and still disagree on what posting a
# status with a poll produces.
WRITES = [
    ("POST", "/api/v1/statuses", {"status": "a plain status"}, "post a status"),
    (
        "POST",
        "/api/v1/statuses",
        {"status": "with a spoiler", "spoiler_text": "cw", "sensitive": True},
        "post behind a content warning",
    ),
    (
        "POST",
        "/api/v1/statuses",
        {"status": "unlisted please", "visibility": "unlisted"},
        "post unlisted",
    ),
    (
        "POST",
        "/api/v1/statuses",
        {"status": "which?", "poll": {"options": ["a", "b"], "expires_in": 3600}},
        "post a poll",
    ),
    ("POST", "/api/v1/lists", {"title": "a list"}, "create a list"),
    (
        "POST",
        "/api/v2/filters",
        {"title": "a filter", "context": ["home"], "filter_action": "warn"},
        "create a filter",
    ),
    # Rejections are part of the contract too, and are where two servers most
    # easily differ: the same bad request should fail the same way.
    ("POST", "/api/v1/statuses", {"status": ""}, "reject an empty status"),
    (
        "POST",
        "/api/v1/statuses",
        {"status": "x", "visibility": "nonsense"},
        "reject an unknown visibility",
    ),
    ("POST", "/api/v1/lists", {}, "reject a list with no title"),
]


def mastodon_headers(args):
    """The Host Mastodon expects, when it answers on a name we cannot resolve."""
    return {"Host": args.mastodon_host} if args.mastodon_host else None


def compare_writes(args, findings):
    """Send each write to both servers and compare what comes back."""
    compared = 0
    for method, path, body, name in WRITES:
        e_status, e_body, _ = request(args.eunha, path, args.eunha_token, method, body)
        m_status, m_body, _ = request(
            args.mastodon, path, args.mastodon_token, method, body,
            extra_headers=mastodon_headers(args),
        )

        if e_status is None or m_status is None:
            continue
        compared += 1
        if e_status != m_status:
            findings.append(f"{name}: eunha {e_status}, Mastodon {m_status}")
            continue
        # A rejection's body is a message, and two servers word those
        # differently; the status code is the part a client acts on.
        if m_status < 400:
            compare(name, shape(e_body), shape(m_body), findings)
            compare_values(name, e_body, m_body, findings)
    return compared


# Interactions need something to act on, and the two servers cannot share ids.
# So each is given its own statuses and its own second account, and the
# *responses* are compared — favouriting your own post should produce the same
# entity here as there, whatever the ids inside it are.
def compare_interactions(args, findings):
    """Post, then act on what was posted, comparing each response."""
    compared = 0

    def on(server, token, method, path, body=None):
        headers = mastodon_headers(args) if server == args.mastodon else None
        return request(server, path, token, method, body, extra_headers=headers)

    servers = [
        ("eunha", args.eunha, args.eunha_token, args.eunha_other_id),
        ("mastodon", args.mastodon, args.mastodon_token, args.mastodon_other_id),
    ]

    def post_status(text):
        """A status on each server, or None if either would not take one."""
        posted = {}
        for name, base, token, _ in servers:
            status, body, _ = on(base, token, "POST", "/api/v1/statuses", {"status": text})
            if status != 200:
                findings.append(
                    f"interactions: {name} would not accept a status ({status})"
                )
                return None
            posted[name] = body["id"]
        return posted

    # Each do/undo pair acts on a status of its own, rather than ten verbs
    # sharing one.
    #
    # Mastodon does some of an undo in a worker: `unfavourite` queues
    # `UnfavouriteWorker` and `unreblog` discards the reblog and queues
    # `RemovalWorker`, each forcing the flag false in *its own* response while
    # the row itself outlives the request. Any other request touching that
    # status in the meantime reads the row and reports `favourited: true` on a
    # status that was just unfavourited — so with one shared status, one pair's
    # deferred work showed up as a difference in every verb that followed it.
    # That is what nine `favourited` findings and one `reblogged` were, and
    # eunha was blamed for all ten.
    #
    # `unreblog` is the sharper case, because the window is one Mastodon
    # disagrees with itself in: `Status.reblogs_map` is `unscoped` and counts
    # the discarded reblog, while `Account#reblogged?` goes through
    # `default_scope { recent.kept }` and does not. Which of the two a response
    # uses depends on whether its controller passes a relationships presenter,
    # so two endpoints answered differently about the same status.
    #
    # A pair per status leaves nothing to leak: the only state a verb depends on
    # is what its own pair put there.
    status_verb_pairs = [
        [("favourite", "/api/v1/statuses/{id}/favourite"),
         ("unfavourite", "/api/v1/statuses/{id}/unfavourite")],
        [("reblog", "/api/v1/statuses/{id}/reblog"),
         ("unreblog", "/api/v1/statuses/{id}/unreblog")],
        [("bookmark", "/api/v1/statuses/{id}/bookmark"),
         ("unbookmark", "/api/v1/statuses/{id}/unbookmark")],
        [("pin", "/api/v1/statuses/{id}/pin"),
         ("unpin", "/api/v1/statuses/{id}/unpin")],
        [("mute conversation", "/api/v1/statuses/{id}/mute"),
         ("unmute conversation", "/api/v1/statuses/{id}/unmute")],
    ]
    status_verbs = []
    for pair in status_verb_pairs:
        posted = post_status(f"something to {pair[0][0]}")
        if posted is None:
            return compared
        status_verbs.extend((verb, template, posted) for verb, template in pair)

    for verb, template, posted in status_verbs:
        results = {}
        for name, base, token, _ in servers:
            results[name] = on(base, token, "POST", template.format(id=posted[name]))
        compared += 1
        e_status, e_body, _ = results["eunha"]
        m_status, m_body, _ = results["mastodon"]
        if e_status != m_status:
            findings.append(f"{verb}: eunha {e_status}, Mastodon {m_status}")
        elif m_status < 400:
            compare(verb, shape(e_body), shape(m_body), findings)
            compare_values(verb, e_body, m_body, findings)

    # And the verbs that act on an account.
    account_verbs = [
        ("follow", "/api/v1/accounts/{id}/follow"),
        ("mute", "/api/v1/accounts/{id}/mute"),
        ("unmute", "/api/v1/accounts/{id}/unmute"),
        ("block", "/api/v1/accounts/{id}/block"),
        ("unblock", "/api/v1/accounts/{id}/unblock"),
        ("unfollow", "/api/v1/accounts/{id}/unfollow"),
    ]
    for verb, template in account_verbs:
        results = {}
        for name, base, token, other in servers:
            if not other:
                results = {}
                break
            results[name] = on(base, token, "POST", template.format(id=other))
        if not results:
            continue
        compared += 1
        e_status, e_body, _ = results["eunha"]
        m_status, m_body, _ = results["mastodon"]
        if e_status != m_status:
            findings.append(f"{verb}: eunha {e_status}, Mastodon {m_status}")
        elif m_status < 400:
            compare(verb, shape(e_body), shape(m_body), findings)
            compare_values(verb, e_body, m_body, findings)

    return compared


def parse_fans(spec):
    """`account_id:token` pairs, in the order they will act."""
    fans = []
    for part in (spec or "").split(","):
        part = part.strip()
        if part:
            account_id, token = part.split(":", 1)
            fans.append((account_id, token))
    return fans


# Grouping is the part of the notifications API that is not a straight
# translation of a row. Mastodon collapses notifications into groups, and a
# client renders "X and 2 others favourited your post" out of a group's
# `notifications_count` and `sample_account_ids` — so a server that groups
# differently shows a different sentence while every field is present and of the
# right type. Shape cannot see it, and neither can a single-account fixture:
# a group of one is a group either way.
#
# Account ids differ between the two servers, so the samples are compared by
# *who* they name rather than by id. Each fan is known by the position it acts
# in, and a group naming [fan3, fan2, fan1] here has to name [fan3, fan2, fan1]
# there.
def compare_notification_grouping(args, findings):
    e_fans, m_fans = parse_fans(args.eunha_fans), parse_fans(args.mastodon_fans)
    if len(e_fans) < 2 or len(e_fans) != len(m_fans):
        return 0

    def on(base, path, token, method="GET", body=None):
        headers = mastodon_headers(args) if base == args.mastodon else None
        return request(base, path, token, method, body, extra_headers=headers)

    summaries = {}
    for name, base, token, fans in (
        ("eunha", args.eunha, args.eunha_token, e_fans),
        ("mastodon", args.mastodon, args.mastodon_token, m_fans),
    ):
        status, me, _ = on(base, "/api/v1/accounts/verify_credentials", token)
        if status != 200:
            findings.append(f"grouping: {name} would not identify itself ({status})")
            return 0

        # From the same state on both sides. eunha gets a scratch database every
        # run while the Mastodon container is left up between them, so anything
        # a previous run left behind shows up as eunha differing.
        #
        # Two things have to be reset, and the second is easy to miss. Clearing
        # the notifications is not enough: a repeat follow produces no
        # notification at all, so on a second run only the server with a fresh
        # database would report a follow group — which reads as eunha inventing
        # one. Unfollowing first makes the follow new on both.
        for _, fan_token in fans:
            on(base, f"/api/v1/accounts/{me['id']}/unfollow", fan_token, "POST")
        on(base, "/api/v1/notifications/clear", token, "POST")

        status, posted, _ = on(
            base, "/api/v1/statuses", token, "POST",
            {"status": "something to group", "visibility": "public"},
        )
        if status != 200:
            findings.append(f"grouping: {name} would not accept a status ({status})")
            return 0

        # The same verb from several accounts, which is what a group is made of.
        # Two kinds: favourites group by the status they are about, follows have
        # no status and group by type alone.
        for _, fan_token in fans:
            on(base, f"/api/v1/statuses/{posted['id']}/favourite", fan_token, "POST")
        for _, fan_token in fans:
            on(base, f"/api/v1/accounts/{me['id']}/follow", fan_token, "POST")

        # Mastodon writes these notifications from `LocalNotificationWorker`,
        # after the favourite or follow has answered; eunha writes them in the
        # request. Reading the groups straight away once caught Mastodon with
        # fan3's follow still queued, a group of two against eunha's three.
        # Wait for one notification per act — a count of what was done, not of
        # how it should group — and compare whatever is there if it never comes.
        acts = 2 * len(fans)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            status, rows, _ = on(base, "/api/v1/notifications?limit=40", token)
            if status == 200 and isinstance(rows, list) and len(rows) >= acts:
                break
            time.sleep(0.5)

        status, body, _ = on(base, "/api/v2/notifications", token)
        if status != 200:
            findings.append(
                f"grouping: {name} answered {status} for /api/v2/notifications"
            )
            return 0
        label = {acct: f"fan{i + 1}" for i, (acct, _) in enumerate(fans)}
        summaries[name] = [
            {
                "type": g.get("type"),
                "notifications_count": g.get("notifications_count"),
                "sample_accounts": [
                    label.get(a, "unknown") for a in (g.get("sample_account_ids") or [])
                ],
            }
            for g in ((body or {}).get("notification_groups") or [])
        ]

    left, right = summaries["eunha"], summaries["mastodon"]
    if len(left) != len(right):
        findings.append(
            f"grouping: eunha returned {len(left)} group(s), Mastodon {len(right)}"
        )
    for i, (e_group, m_group) in enumerate(zip(left, right)):
        for field in ("type", "notifications_count", "sample_accounts"):
            if e_group[field] != m_group[field]:
                findings.append(
                    f"grouping: group {i} `{field}` is {e_group[field]!r} on eunha, "
                    f"{m_group[field]!r} on Mastodon"
                )
    return 1


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--eunha", required=True)
    parser.add_argument("--mastodon", required=True)
    parser.add_argument("--eunha-token", required=True)
    parser.add_argument("--mastodon-token", required=True)
    parser.add_argument("--only", help="compare just paths containing this")
    parser.add_argument("--eunha-other-id", help="a second account on eunha, to follow and block")
    parser.add_argument("--eunha-fans", help="`id:token` pairs on eunha that act together, for grouping")
    parser.add_argument("--mastodon-fans", help="the same, on Mastodon")
    parser.add_argument("--mastodon-other-id", help="the same, on Mastodon")
    parser.add_argument("--eunha-fixture", help="JSON naming eunha's seeded ids and tokens, for the flows")
    parser.add_argument("--mastodon-fixture", help="the same, for Mastodon")
    parser.add_argument("--flow", help="run just the flows whose name contains this")
    parser.add_argument("--verbose", action="store_true",
                        help="print each flow step's status codes, eunha's then Mastodon's")
    parser.add_argument(
        "--mastodon-host",
        help="Host header for Mastodon, when it answers on a name this machine "
        "cannot resolve. Rails refuses a request whose Host is not its "
        "LOCAL_DOMAIN, with a 403 on every endpoint including the ones that "
        "need no authentication — which reads as eunha being wrong about all "
        "of them at once.",
    )
    args = parser.parse_args()

    findings, compared, skipped = [], 0, []
    for method, path, needs_auth in ENDPOINTS:
        if args.flow or (args.only and args.only not in path):
            continue
        e_status, e_body, _ = request(
            args.eunha, path, args.eunha_token if needs_auth else None, method
        )
        m_status, m_body, _ = request(
            args.mastodon,
            path,
            args.mastodon_token if needs_auth else None,
            method,
            extra_headers=mastodon_headers(args),
        )

        if e_status is None or m_status is None:
            skipped.append(f"{path}: transport error")
            continue
        # An endpoint Mastodon does not implement at this version is not drift.
        if m_status == 404 and e_status == 404:
            continue
        if e_status != m_status:
            findings.append(f"{path}: eunha {e_status}, Mastodon {m_status}")
            continue
        if m_status >= 400:
            skipped.append(f"{path}: both {m_status}")
            continue

        compared += 1
        compare(path, shape(e_body), shape(m_body), findings)

        # And the values that are not a matter of instance data.
        e_values, m_values = values_at(e_body), values_at(m_body)
        for field in sorted(set(e_values) & set(m_values)):
            if e_values[field] != m_values[field]:
                findings.append(
                    f"{path}: `{field}` is {e_values[field]!r} on eunha, "
                    f"{m_values[field]!r} on Mastodon"
                )

    if args.flow:
        compared += differential_flows.run(args, findings)
    elif not args.only:
        compared += compare_writes(args, findings)
        compared += compare_interactions(args, findings)
        # Before grouping, which clears the notifications some flows read.
        compared += differential_flows.run(args, findings)
        # Last, because it is the only comparison that depends on what the
        # account's notification list already holds.
        compared += compare_notification_grouping(args, findings)

    print(f"compared {compared} endpoint(s)")
    for s in skipped:
        print(f"  skipped {s}")
    if not findings:
        print("\nNo differences.")
        return 0
    print(f"\n{len(findings)} difference(s):")
    for f in findings:
        print(f"  {f}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
