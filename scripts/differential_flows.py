"""Flows for the differential harness: sequences of requests, each compared.

`differential_test.py` compares single requests — a read, a write, a verb.
Much of what a client depends on is not one request but what a request does to
the next: a domain block that should cost a follower, a moderation action that
should land a warning in the target's notifications, a report that resolves when
its account is actioned. Each flow here drives the same steps through both
servers and compares every step's answer, so a difference is reported at the
step that produced it rather than as a puzzle three requests later.

Each server comes with a fixture (`--eunha-fixture`, `--mastodon-fixture`):
the ids of the accounts and rules it was seeded with, and a token for each
local account. Paths and bodies name them as `{troll}` or `{rule1}`; values a
step creates are saved under a name the same way, so `{report}` is whatever
id each server gave the report it made.

Ids cannot match between two servers, so where an id *is* the answer — which
statuses a report names, which rules — the step says which fixture names to
translate it back into before comparing (`labels`).

Mastodon defers some of what a step causes to Sidekiq, where eunha usually
does it in the request: a domain block's effect on the accounts it covers, the
warning a moderation action sends. A read that observes such an effect waits
for it (`wait`) on both sides, bounded, and compares whatever is there at the
end — so a server that never does it still fails, just not by racing a worker.
"""
import time

import differential_test as dt


def dig(value, path):
    """`a.b.c` into nested dicts, `a[].b` across a list; `None` when missing."""
    if not path:
        return value
    key, _, rest = path.partition(".")
    if key.endswith("[]"):
        # `[]` alone is the response itself, when it is a list.
        if key == "[]":
            items = value
        else:
            items = value.get(key[:-2]) if isinstance(value, dict) else None
        if not isinstance(items, list):
            return None
        return [dig(item, rest) for item in items]
    # A list read as one thing is its first entry, as the shape comparison
    # reads it: `followed_by` of a relationships response is the first's.
    if isinstance(value, list):
        value = value[0] if value else None
    if not isinstance(value, dict):
        return None
    return dig(value.get(key), rest)


class Side:
    """One server, with what its fixture says about it."""

    def __init__(self, name, base, fixture, headers=None):
        self.name = name
        self.base = base
        self.tokens = dict(fixture["tokens"])
        self.vars = dict(fixture["ids"])
        for i, rule in enumerate(fixture.get("rules", [])):
            self.vars[f"rule{i + 1}"] = rule
        self.headers = headers

    def fmt(self, value):
        if isinstance(value, str):
            return value.format(**self.vars)
        if isinstance(value, list):
            return [self.fmt(v) for v in value]
        if isinstance(value, dict):
            return {k: self.fmt(v) for k, v in value.items()}
        return value

    def call(self, method, path, body=None, as_="differ"):
        token = self.tokens.get(as_) if as_ else None
        if isinstance(body, dict) and any(isinstance(v, bytes) for v in body.values()):
            return self.upload(method, path, body, token)
        return dt.request(
            self.base, self.fmt(path), token, method, self.fmt(body),
            extra_headers=self.headers,
        )

    def upload(self, method, path, fields, token):
        """A `multipart/form-data` request: a `bytes` field is a file."""
        import json
        import urllib.error
        import urllib.request
        import uuid

        boundary = uuid.uuid4().hex
        parts = []
        for name, value in fields.items():
            if isinstance(value, bytes):
                head = (f'Content-Disposition: form-data; name="{name}"; filename="{name}.png"\r\n'
                        "Content-Type: image/png\r\n\r\n")
                parts.append(f"--{boundary}\r\n{head}".encode() + value + b"\r\n")
            else:
                head = f'Content-Disposition: form-data; name="{name}"\r\n\r\n'
                parts.append(f"--{boundary}\r\n{head}{self.fmt(value)}\r\n".encode())
        data = b"".join(parts) + f"--{boundary}--\r\n".encode()
        req = urllib.request.Request(f"{self.base}{self.fmt(path)}", method=method, data=data)
        req.add_header("Content-Type", f"multipart/form-data; boundary={boundary}")
        req.add_header("Accept", "application/json")
        req.add_header("X-Forwarded-Proto", "https")
        if token:
            req.add_header("Authorization", f"Bearer {token}")
        for key, value in (self.headers or {}).items():
            req.add_header(key, value)
        try:
            with urllib.request.urlopen(req, timeout=60) as response:
                raw, status = response.read(), response.status
        except urllib.error.HTTPError as e:
            raw, status = e.read(), e.code
        try:
            return status, json.loads(raw), {}
        except json.JSONDecodeError:
            return status, {"__not_json__": raw[:200].decode("utf-8", "replace")}, {}

    def label(self, value, names):
        """An id back into the fixture name it was saved under, if it was."""
        reverse = {str(self.vars[n]): n for n in names if n in self.vars}
        if isinstance(value, list):
            return sorted(reverse.get(str(v), "unknown") for v in value)
        if value is None:
            return None
        return reverse.get(str(value), "unknown")


class Pair:
    def __init__(self, eunha, mastodon, findings, verbose=False):
        self.sides = (eunha, mastodon)
        self.findings = findings
        self.compared = 0
        self.verbose = verbose

    def each(self, method, path, body=None, as_="differ"):
        """Do something on both sides without comparing it: setup, cleanup."""
        return {s.name: s.call(method, path, body, as_)[:2] for s in self.sides}

    def step(self, label, method, path, body=None, as_="differ", check="values",
             save=None, wait=None, timeout=30, fields=(), labels=None,
             count=False, errors=False):
        """Send one request to both servers and compare the answers.

        `check`: "status" compares the status code alone, "shape" adds which
        fields exist and of what kind, "values" adds every field that two
        servers acting on the same input should agree on.
        `save`: {name: dotted path} of the response to keep for later steps,
        or a bare name for its `id`.
        `wait`: a predicate over (status, body) to poll a read until, per side.
        `fields`: dotted paths compared by value even where `check` would not.
        `labels`: {dotted path: fixture names} compared as names, not ids.
        `count`: compare the length of a list response.
        `errors`: compare the shape of an error response, not just its code.
        """
        results = {}
        for side in self.sides:
            try:
                side.fmt([path, body])
            except KeyError as missing:
                # An earlier step that should have made this did not, and
                # already said so; this one cannot be asked at all.
                self.findings.append(
                    f"{label}: not compared, {side.name} has no {missing} from an earlier step"
                )
                return None
        for side in self.sides:
            status, body_, _ = side.call(method, path, body, as_)
            if wait is not None:
                deadline = time.monotonic() + timeout
                while not wait(status, body_) and time.monotonic() < deadline:
                    # Once a second: both servers rate-limit a token's API
                    # calls, and a tighter poll ran into eunha's.
                    time.sleep(1)
                    status, body_, _ = side.call(method, path, body, as_)
                # And then until it stops changing. The first sign of a worker's
                # effect is not the whole of it: `BlockDomainService` suspends
                # the domain's accounts and only afterwards clears the silence
                # an update replaced, so a read between the two caught Mastodon
                # both silenced and suspended.
                while time.monotonic() < deadline:
                    time.sleep(1)
                    again, again_body, _ = side.call(method, path, body, as_)
                    if (again, again_body) == (status, body_):
                        break
                    status, body_ = again, again_body
            if save and status is not None and status < 300:
                # A list's first entry, as a client takes the newest.
                saved_from = body_[0] if isinstance(body_, list) and body_ else body_
                for name, at in ({save: "id"} if isinstance(save, str) else save).items():
                    found = dig(saved_from, at)
                    if isinstance(found, list):
                        found = found[0] if found else None
                    if found is not None:
                        side.vars[name] = found
            results[side.name] = (status, body_)

        self.compared += 1
        (e_status, e_body), (m_status, m_body) = results["eunha"], results["mastodon"]
        if self.verbose:
            # Which steps answered what: a flow that compares two 404s all the
            # way down agrees perfectly and tests nothing.
            print(f"  {e_status} {m_status}  {label}")
        if e_status is None or m_status is None:
            self.findings.append(f"{label}: transport error {e_body!r} / {m_body!r}")
            return results
        if e_status != m_status:
            self.findings.append(f"{label}: eunha {e_status}, Mastodon {m_status}")
            return results
        if m_status >= 400 and not errors:
            return results
        if m_status >= 400:
            # An error's message is worded by each server; what a client acts
            # on is its code and, where there is one, the entity alongside it.
            e_body = {k: v for k, v in (e_body or {}).items() if k != "error"}
            m_body = {k: v for k, v in (m_body or {}).items() if k != "error"}
        if check != "status":
            dt.compare(label, dt.shape(e_body), dt.shape(m_body), self.findings)
        if check == "values":
            dt.compare_values(label, e_body, m_body, self.findings)
        for at in fields:
            left, right = dig(e_body, at), dig(m_body, at)
            if left != right:
                self.findings.append(
                    f"{label}: `{at}` is {left!r} on eunha, {right!r} on Mastodon"
                )
        for at, names in (labels or {}).items():
            e_side, m_side = self.sides
            left, right = e_side.label(dig(e_body, at), names), m_side.label(dig(m_body, at), names)
            if left != right:
                self.findings.append(
                    f"{label}: `{at}` names {left!r} on eunha, {right!r} on Mastodon"
                )
        if count and isinstance(e_body, list) and isinstance(m_body, list):
            if len(e_body) != len(m_body):
                self.findings.append(
                    f"{label}: eunha returned {len(e_body)} item(s), Mastodon {len(m_body)}"
                )
        return results


def nonempty(status, body):
    return status == 200 and bool(body)


def empty(status, body):
    return status == 200 and body == []


def has(path, expected):
    def check(status, body):
        return status == 200 and dig(body, path) == expected
    return check


# ── Domain blocks a member makes ──────────────────────────────────────────────
#
# Mastodon's `AfterBlockDomainFromAccountService`, run by a worker after the
# block itself is saved: notifications from the domain are cleared, follows in
# both directions are severed, and the member is told so with a
# `severed_relationships` notification. The fixture gives `differ` a follower on
# `blocked.example` and the follow notification it produced.
def user_domain_blocks(p):
    domain = {"domain": "blocked.example"}
    p.step("domain blocks: relationship before", "GET",
           "/api/v1/accounts/relationships?id[]={faraway}",
           fields=("followed_by", "domain_blocking"))
    p.step("domain blocks: the follower's notification", "GET",
           "/api/v1/notifications?account_id={faraway}", check="shape", count=True)
    p.step("domain blocks: block a domain", "POST", "/api/v1/domain_blocks", domain)
    p.step("domain blocks: list", "GET", "/api/v1/domain_blocks")
    p.step("domain blocks: relationship after", "GET",
           "/api/v1/accounts/relationships?id[]={faraway}",
           wait=has("followed_by", False),
           fields=("followed_by", "following", "domain_blocking"))
    # After the first block's worker has run, or both runs find the follow to
    # sever and Mastodon reports it twice.
    p.step("domain blocks: blocking it twice", "POST", "/api/v1/domain_blocks", domain)
    p.step("domain blocks: the follower's notification is gone", "GET",
           "/api/v1/notifications?account_id={faraway}", wait=empty, count=True)
    p.step("domain blocks: severed relationships notification", "GET",
           "/api/v1/notifications?types[]=severed_relationships", wait=nonempty,
           count=True)
    p.step("domain blocks: following an account there", "POST",
           "/api/v1/accounts/{faraway}/follow")
    p.step("domain blocks: unblock", "DELETE", "/api/v1/domain_blocks", domain)
    p.step("domain blocks: list after unblock", "GET", "/api/v1/domain_blocks")
    p.step("domain blocks: relationship after unblock", "GET",
           "/api/v1/accounts/relationships?id[]={faraway}",
           fields=("followed_by", "domain_blocking"))
    p.step("domain blocks: no domain", "POST", "/api/v1/domain_blocks", {"domain": ""})


# ── Domain blocks the moderators make ─────────────────────────────────────────
#
# `DomainBlockWorker` → `BlockDomainService`: silencing or suspending every
# account on the domain, retroactively undone on an update by whatever no longer
# applies, and by `UnblockDomainService` on removal. `differ` follows
# `distant@silenced.example`, so suspending the domain severs that follow and
# says so.
def admin_domain_blocks(p):
    admin = "mod"
    p.step("admin domain blocks: list", "GET", "/api/v1/admin/domain_blocks",
           as_=admin, check="shape")
    p.step("admin domain blocks: silence a domain", "POST", "/api/v1/admin/domain_blocks",
           {"domain": "silenced.example", "severity": "silence", "reject_media": True,
            "reject_reports": True, "private_comment": "private", "public_comment": "public",
            "obfuscate": True},
           as_=admin, save="domain_block")
    p.step("admin domain blocks: the same domain again", "POST",
           "/api/v1/admin/domain_blocks",
           {"domain": "silenced.example", "severity": "silence"}, as_=admin, errors=True)
    p.step("admin domain blocks: a subdomain, no stricter", "POST",
           "/api/v1/admin/domain_blocks",
           {"domain": "sub.silenced.example", "severity": "noop"}, as_=admin, errors=True)
    p.step("admin domain blocks: show", "GET", "/api/v1/admin/domain_blocks/{domain_block}",
           as_=admin)
    p.step("admin domain blocks: silenced account", "GET", "/api/v1/admin/accounts/{distant}",
           as_=admin, wait=has("silenced", True), check="shape",
           fields=("silenced", "suspended", "sensitized", "disabled"))
    p.step("admin domain blocks: silenced account, as a member sees it", "GET",
           "/api/v1/accounts/{distant}", check="shape", fields=("limited", "suspended"))
    p.step("admin domain blocks: suspend instead", "PUT",
           "/api/v1/admin/domain_blocks/{domain_block}", {"severity": "suspend"}, as_=admin)
    p.step("admin domain blocks: suspended account", "GET", "/api/v1/admin/accounts/{distant}",
           as_=admin, wait=has("suspended", True), check="shape",
           fields=("silenced", "suspended"))
    p.step("admin domain blocks: suspended account, as a member sees it", "GET",
           "/api/v1/accounts/{distant}", check="values")
    p.step("admin domain blocks: the severed follow", "GET",
           "/api/v1/accounts/relationships?id[]={distant}", fields=("following",))
    p.step("admin domain blocks: severed relationships notification", "GET",
           "/api/v1/notifications?types[]=severed_relationships",
           wait=lambda s, b: s == 200 and any(
               dig(n, "event.type") == "domain_block" for n in (b or [])),
           count=True)
    p.step("admin domain blocks: down to noop", "PUT",
           "/api/v1/admin/domain_blocks/{domain_block}",
           {"severity": "noop", "reject_media": False, "obfuscate": False}, as_=admin)
    p.step("admin domain blocks: account after noop", "GET", "/api/v1/admin/accounts/{distant}",
           as_=admin, wait=has("suspended", False), check="shape",
           fields=("silenced", "suspended"))
    p.step("admin domain blocks: list with one", "GET", "/api/v1/admin/domain_blocks",
           as_=admin)
    p.step("admin domain blocks: remove", "DELETE",
           "/api/v1/admin/domain_blocks/{domain_block}", as_=admin)
    p.step("admin domain blocks: removed", "GET",
           "/api/v1/admin/domain_blocks/{domain_block}", as_=admin)
    p.step("admin domain blocks: no domain", "POST", "/api/v1/admin/domain_blocks",
           {"severity": "silence"}, as_=admin)
    p.step("admin domain blocks: without the scope", "GET", "/api/v1/admin/domain_blocks")


# ── Reports, moderation, warnings ─────────────────────────────────────────────
#
# A member reports a post; a moderator works the report and acts on the
# account; the account's owner sees the warning. Appeals are not here: Mastodon
# takes them through the web interface (`/disputes/strikes/:id/appeal`) and has
# no API for them, so there is nothing of Mastodon's to compare against.
def reports_and_moderation(p):
    admin = "mod"
    p.step("reports: the troll posts", "POST", "/api/v1/statuses",
           {"status": "something reportable"}, as_="troll", check="status",
           save="troll_status")
    p.step("reports: file a report", "POST", "/api/v1/reports",
           {"account_id": "{troll}", "status_ids": ["{troll_status}"],
            "comment": "this is spam", "category": "violation",
            "rule_ids": ["{rule1}"], "forward": False},
           save="report",
           labels={"status_ids": ["troll_status"], "rule_ids": ["rule1", "rule2"]})
    p.step("reports: about nobody", "POST", "/api/v1/reports",
           {"account_id": "0", "comment": "x"})
    # Not "an unknown category": Mastodon answers that with a 500, the enum's
    # `ArgumentError` escaping the controller, and eunha with a 422 — the
    # `report-unknown-category-rejected` divergence.
    p.step("reports: a rule that does not exist", "POST", "/api/v1/reports",
           {"account_id": "{troll}", "category": "violation", "rule_ids": ["0"]})
    p.step("admin reports: list", "GET", "/api/v1/admin/reports", as_=admin,
           check="shape")
    p.step("admin reports: show", "GET", "/api/v1/admin/reports/{report}", as_=admin,
           labels={"rules[].id": ["rule1", "rule2"]})
    p.step("admin reports: assign to self", "POST",
           "/api/v1/admin/reports/{report}/assign_to_self", as_=admin,
           labels={"assigned_account.id": ["mod"]})
    p.step("admin reports: unassign", "POST", "/api/v1/admin/reports/{report}/unassign",
           as_=admin)
    # Refused while the report still cites a rule, which only a violation may.
    p.step("admin reports: recategorise, keeping the rule", "PUT",
           "/api/v1/admin/reports/{report}", {"category": "spam"}, as_=admin)
    p.step("admin reports: recategorise", "PUT", "/api/v1/admin/reports/{report}",
           {"category": "spam", "rule_ids": []}, as_=admin)
    p.step("admin reports: back to a rule", "PUT", "/api/v1/admin/reports/{report}",
           {"category": "violation", "rule_ids": ["{rule2}"]}, as_=admin,
           labels={"rules[].id": ["rule1", "rule2"]})
    p.step("admin reports: resolve", "POST", "/api/v1/admin/reports/{report}/resolve",
           as_=admin, labels={"action_taken_by_account.id": ["mod"]})
    p.step("admin reports: reopen", "POST", "/api/v1/admin/reports/{report}/reopen",
           as_=admin)
    p.step("admin reports: resolved filter", "GET", "/api/v1/admin/reports?resolved=true",
           as_=admin, check="shape")

    p.step("admin accounts: the troll", "GET", "/api/v1/admin/accounts/{troll}", as_=admin)
    p.step("admin accounts: list local", "GET", "/api/v2/admin/accounts?origin=local",
           as_=admin, check="shape")
    p.step("admin accounts: warn, with the report", "POST",
           "/api/v1/admin/accounts/{troll}/action",
           {"type": "none", "report_id": "{report}", "text": "please stop",
            "send_email_notification": True}, as_=admin)
    p.step("admin accounts: the warned report", "GET", "/api/v1/admin/reports/{report}",
           as_=admin, fields=("action_taken",))
    p.step("moderation warning: notification", "GET",
           "/api/v1/notifications?types[]=moderation_warning", as_="troll",
           wait=nonempty, count=True,
           labels={"moderation_warning.status_ids": ["troll_status"]})
    p.step("moderation warning: grouped", "GET",
           "/api/v2/notifications?types[]=moderation_warning", as_="troll")
    p.step("admin accounts: mark sensitive, quietly", "POST",
           "/api/v1/admin/accounts/{troll}/action",
           {"type": "sensitive", "text": "sensitive", "send_email_notification": False},
           as_=admin)
    p.step("admin accounts: silence", "POST", "/api/v1/admin/accounts/{troll}/action",
           {"type": "silence", "send_email_notification": True}, as_=admin)
    p.step("admin accounts: after sensitive and silence", "GET",
           "/api/v1/admin/accounts/{troll}", as_=admin)
    p.step("moderation warning: only the ones that asked to notify", "GET",
           "/api/v1/notifications?types[]=moderation_warning", as_="troll",
           wait=lambda s, b: s == 200 and len(b or []) >= 2, timeout=10, count=True)
    p.step("admin accounts: silenced, as a member sees it", "GET",
           "/api/v1/accounts/{troll}", check="shape", fields=("limited",))
    p.step("admin accounts: disable", "POST", "/api/v1/admin/accounts/{troll}/action",
           {"type": "disable"}, as_=admin)
    p.step("admin accounts: a disabled login", "GET", "/api/v1/accounts/verify_credentials",
           as_="troll", errors=True)
    p.step("admin accounts: enable", "POST", "/api/v1/admin/accounts/{troll}/enable",
           as_=admin)
    p.step("admin accounts: unsilence", "POST", "/api/v1/admin/accounts/{troll}/unsilence",
           as_=admin)
    p.step("admin accounts: unsensitive", "POST",
           "/api/v1/admin/accounts/{troll}/unsensitive", as_=admin)
    p.step("admin accounts: an unknown action", "POST",
           "/api/v1/admin/accounts/{troll}/action", {"type": "nonsense"}, as_=admin)
    p.step("admin accounts: suspend", "POST", "/api/v1/admin/accounts/{troll}/action",
           {"type": "suspend", "text": "gone"}, as_=admin)
    p.step("admin accounts: suspended", "GET", "/api/v1/admin/accounts/{troll}", as_=admin)
    p.step("admin accounts: suspended, as a member sees it", "GET",
           "/api/v1/accounts/{troll}")
    p.step("admin accounts: a suspended login", "GET", "/api/v1/accounts/verify_credentials",
           as_="troll", errors=True)
    p.step("admin accounts: unsuspend", "POST", "/api/v1/admin/accounts/{troll}/unsuspend",
           as_=admin)
    p.step("admin accounts: without the scope", "GET", "/api/v1/admin/accounts/{troll}")


# ── Moves ─────────────────────────────────────────────────────────────────────
#
# Moving is a settings form in Mastodon and a REST call in eunha (the
# `account-moves-rest-api` divergence), so the fixture moves `mover` to
# `moved_to` directly. What a client sees of it is compared: the account's
# `moved`, and that it can no longer be followed.
def moves(p):
    p.step("moves: a moved account", "GET", "/api/v1/accounts/{mover}",
           labels={"moved.id": ["moved_to"]})
    p.step("moves: following it", "POST", "/api/v1/accounts/{mover}/follow")
    p.step("moves: its relationship", "GET", "/api/v1/accounts/relationships?id[]={mover}")
    p.step("moves: looked up", "GET", "/api/v1/accounts/lookup?acct=mover",
           labels={"moved.id": ["moved_to"]})


# ── What a client does every day ──────────────────────────────────────────────
#
# The reads in `differential_test.py` see these endpoints with whatever the
# account already holds, often nothing; these make something, change it, read
# it back and take it away, comparing every answer on the way.

def filters(p):
    p.step("filters: create with a keyword", "POST", "/api/v2/filters",
           {"title": "parity", "context": ["home", "public"], "filter_action": "warn",
            "keywords_attributes": [{"keyword": "parityword", "whole_word": True}]},
           save={"filter": "id", "keyword": "keywords[].id"})
    p.step("filters: show", "GET", "/api/v2/filters/{filter}")
    p.step("filters: add a keyword", "POST", "/api/v2/filters/{filter}/keywords",
           {"keyword": "another", "whole_word": False}, save="keyword2")
    p.step("filters: keywords", "GET", "/api/v2/filters/{filter}/keywords", count=True)
    p.step("filters: a keyword", "GET", "/api/v2/filters/keywords/{keyword2}")
    p.step("filters: change a keyword", "PUT", "/api/v2/filters/keywords/{keyword2}",
           {"keyword": "changed", "whole_word": True})
    p.step("filters: remove a keyword", "DELETE", "/api/v2/filters/keywords/{keyword2}")
    p.step("filters: someone posts the word", "POST", "/api/v1/statuses",
           {"status": "a parityword here"}, as_="other", check="status", save="filtered")
    p.step("filters: the status, filtered", "GET", "/api/v1/statuses/{filtered}",
           labels={"filtered[].filter.id": ["filter"]})
    p.step("filters: filter the status itself", "POST", "/api/v2/filters/{filter}/statuses",
           {"status_id": "{filtered}"}, save="filter_status",
           labels={"status_id": ["filtered"]})
    p.step("filters: statuses", "GET", "/api/v2/filters/{filter}/statuses", count=True)
    p.step("filters: a filtered status", "GET", "/api/v2/filters/statuses/{filter_status}")
    p.step("filters: unfilter the status", "DELETE",
           "/api/v2/filters/statuses/{filter_status}")
    p.step("filters: change the filter", "PUT", "/api/v2/filters/{filter}",
           {"title": "renamed", "filter_action": "hide", "context": ["notifications"]})
    p.step("filters: as v1 sees them", "GET", "/api/v1/filters", count=True)
    p.step("filters: a v1 filter", "GET", "/api/v1/filters/{keyword}")
    p.step("filters: create a v1 filter", "POST", "/api/v1/filters",
           {"phrase": "oldstyle", "context": ["home"], "whole_word": True,
            "irreversible": False}, save="v1filter")
    p.step("filters: change a v1 filter", "PUT", "/api/v1/filters/{v1filter}",
           {"phrase": "oldstyle2", "context": ["home", "thread"]})
    p.step("filters: remove a v1 filter", "DELETE", "/api/v1/filters/{v1filter}")
    p.step("filters: remove", "DELETE", "/api/v2/filters/{filter}")
    p.step("filters: removed", "GET", "/api/v2/filters/{filter}")
    p.step("filters: no context", "POST", "/api/v2/filters", {"title": "x"})


def lists(p):
    p.step("lists: follow someone to list", "POST", "/api/v1/accounts/{other}/follow",
           check="status")
    p.step("lists: create", "POST", "/api/v1/lists",
           {"title": "parity list", "replies_policy": "followed", "exclusive": True},
           save="list")
    p.step("lists: update", "PUT", "/api/v1/lists/{list}",
           {"title": "renamed list", "replies_policy": "none", "exclusive": False})
    p.step("lists: show", "GET", "/api/v1/lists/{list}")
    p.step("lists: add an account", "POST", "/api/v1/lists/{list}/accounts",
           {"account_ids": ["{other}"]})
    p.step("lists: add it again", "POST", "/api/v1/lists/{list}/accounts",
           {"account_ids": ["{other}"]})
    p.step("lists: add one not followed", "POST", "/api/v1/lists/{list}/accounts",
           {"account_ids": ["{troll}"]})
    p.step("lists: accounts", "GET", "/api/v1/lists/{list}/accounts", count=True,
           labels={"[].id": ["other"]})
    p.step("lists: lists holding an account", "GET", "/api/v1/accounts/{other}/lists",
           count=True)
    p.step("lists: the member posts", "POST", "/api/v1/statuses",
           {"status": "for the list"}, as_="other", check="status", save="listed")
    p.step("lists: timeline", "GET", "/api/v1/timelines/list/{list}", wait=nonempty,
           check="shape")
    p.step("lists: remove the account", "DELETE", "/api/v1/lists/{list}/accounts",
           {"account_ids": ["{other}"]})
    p.step("lists: accounts after", "GET", "/api/v1/lists/{list}/accounts", count=True)
    p.step("lists: remove", "DELETE", "/api/v1/lists/{list}")
    p.step("lists: removed", "GET", "/api/v1/lists/{list}")
    p.step("lists: unfollow", "POST", "/api/v1/accounts/{other}/unfollow", check="status")


def tags(p):
    p.step("tags: follow", "POST", "/api/v1/tags/paritytag/follow")
    p.step("tags: show", "GET", "/api/v1/tags/paritytag")
    p.step("tags: followed", "GET", "/api/v1/followed_tags", count=True)
    p.step("tags: unfollow", "POST", "/api/v1/tags/paritytag/unfollow")
    p.step("tags: feature", "POST", "/api/v1/tags/paritytag/feature")
    p.step("tags: featured", "GET", "/api/v1/featured_tags", count=True)
    p.step("tags: unfeature", "POST", "/api/v1/tags/paritytag/unfeature")
    p.step("featured tags: create", "POST", "/api/v1/featured_tags",
           {"name": "paritytwo"}, save="featured")
    p.step("featured tags: create again", "POST", "/api/v1/featured_tags",
           {"name": "paritytwo"})
    p.step("featured tags: an invalid name", "POST", "/api/v1/featured_tags",
           {"name": "not a tag"})
    p.step("featured tags: suggestions", "GET", "/api/v1/featured_tags/suggestions",
           check="shape")
    p.step("featured tags: on the profile", "GET", "/api/v1/accounts/{differ}/featured_tags",
           count=True)
    p.step("featured tags: remove", "DELETE", "/api/v1/featured_tags/{featured}")
    p.step("tags: timeline", "GET", "/api/v1/timelines/tag/paritytag", check="shape")


def statuses(p):
    p.step("statuses: post one to edit", "POST", "/api/v1/statuses",
           {"status": "before the edit"}, save="edited")
    p.step("statuses: edit", "PUT", "/api/v1/statuses/{edited}",
           {"status": "after the edit", "spoiler_text": "now with a warning",
            "sensitive": True, "language": "en"})
    p.step("statuses: history", "GET", "/api/v1/statuses/{edited}/history", count=True)
    p.step("statuses: source", "GET", "/api/v1/statuses/{edited}/source")
    p.step("statuses: edit someone else's", "PUT", "/api/v1/statuses/{edited}",
           {"status": "mine now"}, as_="other")
    p.step("statuses: reply", "POST", "/api/v1/statuses",
           {"status": "a reply", "in_reply_to_id": "{edited}"}, save="reply",
           labels={"in_reply_to_id": ["edited"]})
    p.step("statuses: context", "GET", "/api/v1/statuses/{edited}/context",
           labels={"descendants[].id": ["reply"]})
    # In no particular order: `permitted_statuses_from_ids` is not `stable`
    # here, so Mastodon answers in whatever order the database does, and the
    # first entry — what the shape and values are read from — is either.
    p.step("statuses: several at once", "GET",
           "/api/v1/statuses?id[]={edited}&id[]={reply}", check="status",
           labels={"[].id": ["edited", "reply"]})
    p.step("statuses: favourited by", "POST", "/api/v1/statuses/{edited}/favourite",
           as_="fan1", check="status")
    p.step("statuses: who favourited", "GET", "/api/v1/statuses/{edited}/favourited_by",
           wait=nonempty, labels={"[].id": ["fan1"]}, count=True)
    p.step("statuses: who boosted", "GET", "/api/v1/statuses/{edited}/reblogged_by",
           count=True)
    p.step("statuses: delete the reply", "DELETE", "/api/v1/statuses/{reply}")
    p.step("statuses: deleted", "GET", "/api/v1/statuses/{reply}")
    p.step("statuses: a reply to nothing", "POST", "/api/v1/statuses",
           {"status": "orphan", "in_reply_to_id": "1"})


def scheduled_statuses(p):
    from datetime import datetime, timedelta, timezone

    def at(delta):
        return (datetime.now(timezone.utc) + delta).strftime("%Y-%m-%dT%H:%M:%S.000Z")

    p.step("scheduled: schedule a status", "POST", "/api/v1/statuses",
           {"status": "later", "scheduled_at": at(timedelta(days=1)),
            "visibility": "unlisted"}, save="scheduled")
    p.step("scheduled: list", "GET", "/api/v1/scheduled_statuses", count=True)
    p.step("scheduled: show", "GET", "/api/v1/scheduled_statuses/{scheduled}")
    p.step("scheduled: reschedule", "PUT", "/api/v1/scheduled_statuses/{scheduled}",
           {"scheduled_at": at(timedelta(days=2))})
    p.step("scheduled: too soon", "PUT", "/api/v1/scheduled_statuses/{scheduled}",
           {"scheduled_at": at(timedelta(minutes=1))})
    p.step("scheduled: cancel", "DELETE", "/api/v1/scheduled_statuses/{scheduled}")
    p.step("scheduled: cancelled", "GET", "/api/v1/scheduled_statuses/{scheduled}")


def polls(p):
    p.step("polls: post one", "POST", "/api/v1/statuses",
           {"status": "pick", "poll": {"options": ["a", "b", "c"], "expires_in": 3600}},
           save={"poll_status": "id", "poll": "poll.id"})
    p.step("polls: vote", "POST", "/api/v1/polls/{poll}/votes", {"choices": [1]},
           as_="other")
    p.step("polls: vote again", "POST", "/api/v1/polls/{poll}/votes", {"choices": [0]},
           as_="other")
    p.step("polls: vote on one's own", "POST", "/api/v1/polls/{poll}/votes",
           {"choices": [0]})
    p.step("polls: a choice that is not there", "POST", "/api/v1/polls/{poll}/votes",
           {"choices": [7]}, as_="fan1")
    p.step("polls: show", "GET", "/api/v1/polls/{poll}")
    p.step("polls: post a multiple choice one", "POST", "/api/v1/statuses",
           {"status": "pick several", "poll": {"options": ["a", "b", "c"],
                                                "expires_in": 3600, "multiple": True}},
           save={"multi": "poll.id"})
    p.step("polls: vote for several", "POST", "/api/v1/polls/{multi}/votes",
           {"choices": [0, 2]}, as_="other")


def conversations(p):
    # A private mention from someone the recipient does not follow is
    # filtered by the default notification policy, and a filtered mention
    # makes no conversation. Following first keeps this about conversations.
    p.step("conversations: the recipient follows", "POST", "/api/v1/accounts/{differ}/follow",
           as_="other", check="status")
    p.step("conversations: a direct message", "POST", "/api/v1/statuses",
           {"status": "@other just between us", "visibility": "direct"}, save="dm")
    p.step("conversations: the recipient's", "GET", "/api/v1/conversations", as_="other",
           wait=lambda s, b: s == 200 and bool(b) and b[0].get("unread") is True,
           save="conversation", labels={"[].last_status.id": ["dm"]})
    p.step("conversations: mark read", "POST", "/api/v1/conversations/{conversation}/read",
           as_="other")
    p.step("conversations: mark unread", "POST",
           "/api/v1/conversations/{conversation}/unread", as_="other")
    p.step("conversations: remove", "DELETE", "/api/v1/conversations/{conversation}",
           as_="other")
    p.step("conversations: the recipient unfollows", "POST",
           "/api/v1/accounts/{differ}/unfollow", as_="other", check="status")


def markers(p):
    p.step("markers: post something to mark", "POST", "/api/v1/statuses",
           {"status": "read up to here"}, check="status", save="marked")
    p.step("markers: set", "POST", "/api/v1/markers",
           {"home": {"last_read_id": "{marked}"}},
           labels={"home.last_read_id": ["marked"]})
    p.step("markers: read", "GET", "/api/v1/markers?timeline[]=home&timeline[]=notifications",
           labels={"home.last_read_id": ["marked"]})


def accounts(p):
    p.step("accounts: show", "GET", "/api/v1/accounts/{other}")
    p.step("accounts: several at once", "GET",
           "/api/v1/accounts?id[]={other}&id[]={troll}", check="status",
           labels={"[].id": ["other", "troll"]})
    p.step("accounts: look up", "GET", "/api/v1/accounts/lookup?acct=other")
    p.step("accounts: look up nobody", "GET", "/api/v1/accounts/lookup?acct=nobody")
    p.step("accounts: search", "GET", "/api/v1/accounts/search?q=other&resolve=false",
           check="shape")
    p.step("accounts: statuses without replies", "GET",
           "/api/v1/accounts/{differ}/statuses?exclude_replies=true&limit=1", check="shape")
    p.step("accounts: pinned statuses", "GET", "/api/v1/accounts/{differ}/statuses?pinned=true",
           count=True)
    p.step("accounts: a note", "POST", "/api/v1/accounts/{other}/note",
           {"comment": "met at the parity meetup"})
    p.step("accounts: follow to endorse", "POST", "/api/v1/accounts/{other}/follow",
           check="status")
    p.step("accounts: endorse", "POST", "/api/v1/accounts/{other}/endorse")
    p.step("accounts: endorsements", "GET", "/api/v1/endorsements", count=True)
    p.step("accounts: endorsed on the profile", "GET",
           "/api/v1/accounts/{differ}/endorsements", count=True)
    p.step("accounts: unendorse", "POST", "/api/v1/accounts/{other}/unendorse")
    p.step("accounts: endorse someone not followed", "POST",
           "/api/v1/accounts/{troll}/endorse")
    p.step("accounts: familiar followers", "GET",
           "/api/v1/accounts/familiar_followers?id[]={other}", check="shape")
    p.step("accounts: followers", "GET", "/api/v1/accounts/{other}/followers", check="shape")
    p.step("accounts: following", "GET", "/api/v1/accounts/{differ}/following", check="shape")
    p.step("accounts: unfollow", "POST", "/api/v1/accounts/{other}/unfollow", check="status")
    p.step("accounts: they follow", "POST", "/api/v1/accounts/{differ}/follow", as_="other",
           check="status")
    p.step("accounts: remove from followers", "POST",
           "/api/v1/accounts/{other}/remove_from_followers")
    p.step("accounts: directory", "GET", "/api/v1/directory?local=true&limit=2", check="shape")
    p.step("accounts: update credentials", "PATCH", "/api/v1/accounts/update_credentials",
           {"note": "comparing servers", "locked": False, "bot": False,
            "fields_attributes": {"0": {"name": "site", "value": "example.com"}}})
    p.step("accounts: profile", "GET", "/api/v1/profile")


def notification_settings(p):
    p.step("notification policy: v2", "GET", "/api/v2/notifications/policy")
    p.step("notification policy: filter strangers", "PATCH", "/api/v2/notifications/policy",
           {"for_not_following": "filter", "for_new_accounts": "drop",
            "for_limited_accounts": "filter"})
    p.step("notification policy: as v1 sees it", "GET", "/api/v1/notifications/policy")
    p.step("notification policy: v1 update", "PATCH", "/api/v1/notifications/policy",
           {"filter_not_followers": True})
    p.step("notification policy: back to accepting", "PATCH", "/api/v2/notifications/policy",
           {"for_not_following": "accept", "for_not_followers": "accept",
            "for_new_accounts": "accept", "for_private_mentions": "accept",
            "for_limited_accounts": "accept"})
    p.step("notification requests: list", "GET", "/api/v1/notifications/requests",
           check="shape")
    p.step("notification requests: merged", "GET", "/api/v1/notifications/requests/merged")
    p.step("notifications: unread count", "GET", "/api/v1/notifications/unread_count",
           check="shape")
    p.step("notifications: grouped unread count", "GET",
           "/api/v2/notifications/unread_count", check="shape")


# A real P-256 point — the curve's generator — and sixteen bytes of auth
# secret, because Mastodon checks the key is a point on the curve.
P256DH = ("BGsX0fLhLEJH-Lzm5WOkQPJ3A32BLeszoPShOUXYmMKWT-NC4v4af5uO5-tKfA-eFivOM1drMV7Oy7ZAaDe_UfU")
AUTH = "AAAAAAAAAAAAAAAAAAAAAA"


def push_subscription(p):
    p.step("push: subscribe", "POST", "/api/v1/push/subscription",
           {"subscription": {"endpoint": "https://push.example/send/parity",
                             "keys": {"p256dh": P256DH, "auth": AUTH}},
            "data": {"alerts": {"follow": True, "mention": True}, "policy": "followed"}})
    p.step("push: show", "GET", "/api/v1/push/subscription")
    p.step("push: change alerts", "PUT", "/api/v1/push/subscription",
           {"data": {"alerts": {"favourite": True}, "policy": "all"}})
    p.step("push: unsubscribe", "DELETE", "/api/v1/push/subscription")
    p.step("push: none left", "GET", "/api/v1/push/subscription")
    # Not "a key that is the point at infinity": Mastodon answers it with a
    # 500, the `PKeyError` escaping `WebPushKeyValidator`, and eunha with a
    # 422 — the `push-subscription-unusable-key-rejected` divergence.


def apps(p):
    p.step("apps: register", "POST", "/api/v1/apps",
           {"client_name": "parity", "redirect_uris": "urn:ietf:wg:oauth:2.0:oob",
            "scopes": "read write", "website": "https://parity.example"})
    p.step("apps: register with no name", "POST", "/api/v1/apps",
           {"redirect_uris": "urn:ietf:wg:oauth:2.0:oob"})
    p.step("apps: register with an unknown scope", "POST", "/api/v1/apps",
           {"client_name": "parity", "redirect_uris": "urn:ietf:wg:oauth:2.0:oob",
            "scopes": "read nonsense"})
    p.step("apps: verify credentials", "GET", "/api/v1/apps/verify_credentials")


def instance(p):
    for path in ("extended_description", "privacy_policy", "terms_of_service",
                 "languages", "activity", "translation_languages", "domain_blocks"):
        p.step(f"instance: {path}", "GET", f"/api/v1/instance/{path}", as_=None,
               check="shape")
    p.step("instance: donation campaigns", "GET",
           "/api/v1/donation_campaigns?locale=en&seed=1", check="status")
    p.step("admin: canonical email blocks", "GET", "/api/v1/admin/canonical_email_blocks",
           as_="mod", check="shape")
    p.step("admin: block an email", "POST", "/api/v1/admin/canonical_email_blocks",
           {"email": "parity@blocked.example"}, as_="mod", save="email_block")
    p.step("admin: test an email", "POST", "/api/v1/admin/canonical_email_blocks/test",
           {"email": "parity@blocked.example"}, as_="mod", count=True)
    p.step("admin: unblock the email", "DELETE",
           "/api/v1/admin/canonical_email_blocks/{email_block}", as_="mod")
    p.step("admin: block an email domain", "POST", "/api/v1/admin/email_domain_blocks",
           {"domain": "spam.example"}, as_="mod", save="email_domain_block")
    p.step("admin: email domain blocks", "GET", "/api/v1/admin/email_domain_blocks",
           as_="mod", check="shape")
    p.step("admin: unblock the email domain", "DELETE",
           "/api/v1/admin/email_domain_blocks/{email_domain_block}", as_="mod")
    p.step("admin: block an address", "POST", "/api/v1/admin/ip_blocks",
           {"ip": "192.0.2.0/24", "severity": "sign_up_requires_approval",
            "comment": "documentation range", "expires_in": 86400}, as_="mod",
           save="ip_block")
    p.step("admin: change the address block", "PUT", "/api/v1/admin/ip_blocks/{ip_block}",
           {"severity": "no_access"}, as_="mod")
    p.step("admin: address blocks", "GET", "/api/v1/admin/ip_blocks", as_="mod",
           check="shape")
    p.step("admin: unblock the address", "DELETE", "/api/v1/admin/ip_blocks/{ip_block}",
           as_="mod")
    p.step("admin: allow a domain", "POST", "/api/v1/admin/domain_allows",
           {"domain": "friendly.example"}, as_="mod", save="domain_allow")
    p.step("admin: allowed domains", "GET", "/api/v1/admin/domain_allows", as_="mod",
           check="shape")
    p.step("admin: disallow the domain", "DELETE",
           "/api/v1/admin/domain_allows/{domain_allow}", as_="mod")
    p.step("admin: hashtags", "GET", "/api/v1/admin/tags", as_="mod", check="shape")
    p.step("admin: trending tags", "GET", "/api/v1/admin/trends/tags", as_="mod",
           check="shape")
    p.step("admin: measures", "POST", "/api/v1/admin/measures",
           {"keys": ["active_users", "new_users"], "start_at": "2026-01-01",
            "end_at": "2026-01-02"}, as_="mod", check="shape")
    p.step("admin: dimensions", "POST", "/api/v1/admin/dimensions",
           {"keys": ["languages"], "start_at": "2026-01-01", "end_at": "2026-01-02"},
           as_="mod", check="shape")


def png(width=4, height=3):
    """A small red PNG, made here so the harness carries no binary."""
    import struct
    import zlib

    def chunk(kind, data):
        return (struct.pack(">I", len(data)) + kind + data
                + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF))

    rows = b"".join(b"\x00" + b"\xff\x00\x00" * width for _ in range(height))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))


def media(p):
    p.step("media: upload", "POST", "/api/v2/media",
           {"file": png(), "description": "a red rectangle"}, save="media")
    p.step("media: show", "GET", "/api/v1/media/{media}")
    p.step("media: describe and focus", "PUT", "/api/v1/media/{media}",
           {"description": "still red", "focus": "0.5,-0.5"})
    p.step("media: upload through v1", "POST", "/api/v1/media",
           {"file": png(2, 2)}, save="media_v1")
    p.step("media: post with it", "POST", "/api/v1/statuses",
           {"status": "with a picture", "media_ids": ["{media}"]}, save="with_media")
    p.step("media: delete one attached", "DELETE", "/api/v1/media/{media}")
    p.step("media: delete one not", "DELETE", "/api/v1/media/{media_v1}")
    p.step("media: deleted", "GET", "/api/v1/media/{media_v1}")
    p.step("media: no file", "POST", "/api/v2/media", {"description": "nothing"})


FLOWS = [
    user_domain_blocks, admin_domain_blocks, reports_and_moderation, moves,
    filters, lists, tags, statuses, scheduled_statuses, polls, conversations,
    markers, accounts, notification_settings, push_subscription, apps, instance,
    media,
]


def run(args, findings):
    import json

    if not (args.eunha_fixture and args.mastodon_fixture):
        return 0
    with open(args.eunha_fixture) as f:
        e_fixture = json.load(f)
    with open(args.mastodon_fixture) as f:
        m_fixture = json.load(f)
    pair = Pair(
        Side("eunha", args.eunha, e_fixture),
        Side("mastodon", args.mastodon, m_fixture, dt.mastodon_headers(args)),
        findings,
        verbose=args.verbose,
    )
    # Each flow acts as `differ` through a token of its own, in turn: one
    # token for them all runs into `throttle_per_token_api`, 300 requests in
    # five minutes, after which both servers answer 429 to everything and
    # agree about nothing worth knowing.
    for i, flow in enumerate(FLOWS):
        if args.flow and args.flow not in flow.__name__:
            continue
        for side, fixture in zip(pair.sides, (e_fixture, m_fixture)):
            spare = fixture.get("flow_tokens") or [fixture["tokens"]["differ"]]
            side.tokens["differ"] = spare[i % len(spare)]
        flow(pair)
    return pair.compared
