#!/usr/bin/env python3
"""Federate eunha with a real Mastodon, in both directions, and check what lands.

The rest of this repo's federation tests run eunha against eunha. Both sides
then share eunha's reading of ActivityPub, so a misreading is invisible: two
servers agreeing about something they are both wrong about looks exactly like
correctness. This drives the same activities between eunha and Mastodon 4.7.2,
where nothing is shared but the specification.

Each case makes something happen through one server's own API — so the activity
is signed, addressed and delivered by that server's real code — and then asks
the other server what it received. Delivery is asynchronous on both sides, so
every check polls.

Run it through scripts/federation_test.sh, which builds the pair.

Known state, as of writing: all forty-five checks pass. A follow, a status, a
favourite, a boost and a delete cross in both directions and are understood on
the other side; so do a report forwarded to the other server's moderators, and
an account's move, which takes its followers on the other server with it.

One note on eunha's delivery, visible in the proxy log: each POST is a 401 then
a 202. That is the double-knock working as intended — draft-cavage first, then
RFC 9421 when the peer answers 401 — and is what Mastodon 4.7 itself does. It
reads alarmingly in an access log and is not a failure.

Seven things about the environment took a while to find, and all of them make
a correct implementation look broken:

* **Port 443 or nothing.** Mastodon webfingers an account by the *host* of its
  actor URI and drops the port, so eunha on `:3002` is looked up on `:443` and
  every delivery fails verification. eunha sits behind a proxy on 443 for this
  reason, not by preference.
* **A private address is refused by both.** Mastodon has `ALLOWED_PRIVATE_
  ADDRESSES` for it; eunha's `PublicOnlyResolver` has no equivalent, so remote
  actors are seeded rather than fetched.
* **The image runs as a non-root user**, so the CA goes in through
  `SSL_CERT_FILE` — appended to the image's own bundle, not replacing it.
* **No Sidekiq means nothing is delivered at all**, silently.
* **`ALLOWED_PRIVATE_ADDRESSES` belongs on the sidekiq service, not just web.**
  Deliveries run in sidekiq; without it every one fails before a request is
  made, and Mastodon's circuit breaker then trips the inbox to red and stops
  trying. The symptom is no request, no error, and empty queues — which reads as
  "Mastodon silently refuses to talk to eunha" and is nothing of the kind. If
  deliveries stop arriving, look for `stoplight:` keys in Mastodon's Redis
  before suspecting eunha.
* **Mastodon cannot search statuses without Elasticsearch.** Asking `/api/v2/
  search` whether a status arrived answers no whatever the truth, so the check
  reads the account's timeline instead. This one cost the most: it looked for a
  long time like eunha was not delivering, when eunha was delivering and
  Mastodon was storing it perfectly well.
* **Mastodon serves an actor from a three-minute cache keyed without a
  timestamp** (`render_with_cache`), so a change to an account is not in what
  it serves until the cache expires. An alias added after eunha first fetched
  the account was missing from the fetch that checks a `Move`, and eunha
  refused the move — correctly, on what it was given. The script makes the
  alias before anything fetches the account.
"""
import argparse
import json
import sys
import time
import urllib.error
import urllib.request


class Server:
    """One side of the pair, driven through its client API."""

    def __init__(self, name, base, token, extra_headers=None):
        self.name = name
        self.base = base.rstrip("/")
        self.token = token
        self.extra_headers = extra_headers or {}

    def call(self, method, path, body=None, token=None):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(f"{self.base}{path}", method=method, data=data)
        req.add_header("Authorization", f"Bearer {token or self.token}")
        req.add_header("Accept", "application/json")
        # Mastodon's production environment sets `config.force_ssl` and would
        # answer 301 to a plain request. Both servers are driven over plain
        # published ports here — the certificates they federate with stay inside
        # the container network — so this is what a proxy in front would send.
        req.add_header("X-Forwarded-Proto", "https")
        if data is not None:
            req.add_header("Content-Type", "application/json")
        for k, v in self.extra_headers.items():
            req.add_header(k, v)
        try:
            with urllib.request.urlopen(req, timeout=30) as response:
                raw = response.read()
                return response.status, (json.loads(raw) if raw else None)
        except urllib.error.HTTPError as e:
            raw = e.read()
            try:
                return e.code, json.loads(raw)
            except json.JSONDecodeError:
                return e.code, {"raw": raw[:200].decode("utf-8", "replace")}
        except Exception as e:
            return None, {"transport_error": str(e)}


# A follow that crosses to Mastodon is the slowest wait here: the Follow goes
# out, Sidekiq picks it up, and the Accept has to come back before either side
# will say so. The default budget was tuned against the local waits, which are
# one queue hop, and on a cold CI runner it is the round trip that runs out of
# room — `mastodon→eunha: receiver follows the sender` failed on fa9b0e2 with no
# server code changed in the range, and passed on the next commit. Waiting
# longer is free on a passing run, since `until` returns the moment its
# predicate holds; the budget is only spent on a run that would otherwise fail.
FOLLOW_ROUND_TRIP = 60


def until(predicate, seconds=25, interval=1.0):
    """Poll, because delivery is a queue on both sides, not a function call."""
    deadline = time.time() + seconds
    last = None
    while time.time() < deadline:
        last = predicate()
        if last:
            return last
        time.sleep(interval)
    return last


class Report:
    def __init__(self):
        self.results = []

    def check(self, direction, name, ok, detail=""):
        self.results.append((direction, name, bool(ok), detail))
        mark = "ok  " if ok else "FAIL"
        line = f"  [{mark}] {direction}: {name}"
        print(f"{line}    {detail}" if detail and not ok else line)

    def failed(self):
        return [r for r in self.results if not r[2]]


def find_account(server, acct):
    """Look up an account by its full handle, resolving it if unseen."""
    status, body = server.call("GET", f"/api/v1/accounts/lookup?acct={acct}")
    if status == 200 and body:
        return body
    status, body = server.call(
        "GET", f"/api/v2/search?q={acct}&type=accounts&resolve=true"
    )
    if status == 200 and body and body.get("accounts"):
        return body["accounts"][0]
    return None


def run_direction(sender, receiver, sender_acct, receiver_acct, report):
    """Drive sender → receiver: follow, post, favourite, boost, delete."""
    direction = f"{sender.name}→{receiver.name}"

    # The receiving account as the sender sees it. Everything else needs this.
    remote = until(lambda: find_account(sender, receiver_acct), seconds=30)
    if not remote:
        report.check(direction, "resolve the remote account", False,
                     f"{sender.name} could not find {receiver_acct}")
        return
    report.check(direction, "resolve the remote account", True)

    # Start from no relationship, so a run says what this run did rather than
    # what a previous one left behind. Both sides, since either may hold one.
    sender.call("POST", f"/api/v1/accounts/{remote['id']}/unfollow")
    back0 = find_account(receiver, sender_acct)
    if back0:
        receiver.call("POST", f"/api/v1/accounts/{back0['id']}/unfollow")
    time.sleep(3)

    # ── Follow ──────────────────────────────────────────────────────────────
    status, _ = sender.call("POST", f"/api/v1/accounts/{remote['id']}/follow")
    ok = status == 200
    report.check(direction, "follow is accepted locally", ok, f"status {status}")

    def followed():
        st, body = receiver.call("GET", "/api/v1/accounts/verify_credentials")
        if st != 200:
            return False
        st, followers = receiver.call(
            "GET", f"/api/v1/accounts/{body['id']}/followers"
        )
        return st == 200 and any(
            a.get("acct") == sender_acct for a in (followers or [])
        )

    report.check(direction, "follow arrives", bool(until(followed)))

    # A status is delivered to the author's followers, so the receiver has to
    # follow the sender for anything to arrive. That is a Follow in the other
    # direction — tested on its own in the other pass; here it is setup.
    back = until(lambda: find_account(receiver, sender_acct), seconds=30)
    if not back:
        report.check(direction, "receiver can resolve the sender", False,
                     f"{receiver.name} could not find {sender_acct}")
        return
    st, _ = receiver.call("POST", f"/api/v1/accounts/{back['id']}/follow")
    if st != 200:
        report.check(direction, "receiver follows the sender", False,
                     f"{receiver.name} refused the follow: status {st}")
        return

    def follows_back():
        """Both sides, because they commit at different moments.

        The sender listing the receiver as a follower is what makes it
        *deliver*. The receiver having committed the follow is what makes it
        *keep* what arrives — Mastodon drops an activity from an account no
        local account follows yet, and it only counts the follow once it has
        processed the sender's Accept. Waiting on the sender alone leaves a gap
        of a few milliseconds in which a status is delivered, accepted with a
        2xx, and silently discarded: one run missed by 2.9ms.
        """
        st, body = sender.call("GET", "/api/v1/accounts/verify_credentials")
        if st != 200:
            return False
        st, followers = sender.call("GET", f"/api/v1/accounts/{body['id']}/followers")
        if st != 200 or not any(
            a.get("acct") == receiver_acct for a in (followers or [])
        ):
            return False
        st, rels = receiver.call(
            "GET", f"/api/v1/accounts/relationships?id[]={back['id']}"
        )
        return st == 200 and bool(rels) and rels[0].get("following") is True

    if not until(follows_back, seconds=FOLLOW_ROUND_TRIP):
        report.check(direction, "receiver follows the sender", False,
                     "without this a status has nowhere to be delivered")
        return
    report.check(direction, "receiver follows the sender", True)

    # ── Create ──────────────────────────────────────────────────────────────
    marker = f"federated-{int(time.time() * 1000)}"
    status, posted = sender.call(
        "POST", "/api/v1/statuses", {"status": f"hello {marker}", "visibility": "public"}
    )
    if status != 200:
        report.check(direction, "post a status", False, f"status {status}")
        return
    report.check(direction, "post a status", True)

    def received_status():
        # Not search: Mastodon's status search needs Elasticsearch, which this
        # stack does not run, so a delivered status is simply not findable that
        # way. The account's own timeline is what actually shows what arrived.
        st, body = receiver.call(
            "GET", f"/api/v1/accounts/{back['id']}/statuses?limit=40"
        )
        if st != 200 or not body:
            return None
        for s in body:
            if marker in (s.get("content") or ""):
                return s
        return None

    arrived = until(received_status, seconds=30)
    report.check(direction, "status arrives", bool(arrived),
                 "not found on the other side")
    if not arrived:
        return

    # ── Like ────────────────────────────────────────────────────────────────
    status, _ = receiver.call("POST", f"/api/v1/statuses/{arrived['id']}/favourite")
    report.check(direction, "favourite is accepted", status == 200, f"status {status}")

    def favourited():
        st, body = sender.call("GET", f"/api/v1/statuses/{posted['id']}")
        return st == 200 and (body or {}).get("favourites_count", 0) >= 1

    report.check(direction, "favourite comes back", bool(until(favourited)))

    # ── Announce ────────────────────────────────────────────────────────────
    status, _ = receiver.call("POST", f"/api/v1/statuses/{arrived['id']}/reblog")
    report.check(direction, "boost is accepted", status == 200, f"status {status}")

    def boosted():
        st, body = sender.call("GET", f"/api/v1/statuses/{posted['id']}")
        return st == 200 and (body or {}).get("reblogs_count", 0) >= 1

    report.check(direction, "boost comes back", bool(until(boosted)))

    # ── Delete ──────────────────────────────────────────────────────────────
    status, _ = sender.call("DELETE", f"/api/v1/statuses/{posted['id']}")
    report.check(direction, "delete is accepted locally", status == 200,
                 f"status {status}")

    def gone():
        st, body = receiver.call("GET", f"/api/v1/statuses/{arrived['id']}")
        return st in (404, 410)

    report.check(direction, "delete arrives", bool(until(gone)),
                 "the status is still there")


# Mastodon distributes a status to a peer's *shared* inbox whenever that peer
# advertises one — it does not wait for a second follower — and uses the personal
# inbox for directed activities like Follow and Accept. So eunha's `/inbox` was
# already receiving deliveries; the earlier note here that it never had was
# simply wrong, and measuring it is what showed that.
#
# What a single follower cannot test is the part that makes a shared inbox worth
# having: one delivery, addressed to nobody in particular, fanned out by the
# *receiving* side to every local account it concerns. With one follower that is
# indistinguishable from delivering to that account. Bob makes it two.
def check_shared_inbox(eunha, mastodon, second, mastodon_acct, report):
    direction = "mastodon→eunha"
    remote = until(lambda: find_account(second, mastodon_acct), seconds=30)
    if not remote:
        report.check(direction, "shared inbox: second account resolves the sender",
                     False, f"eunha could not find {mastodon_acct}")
        return
    status, _ = second.call("POST", f"/api/v1/accounts/{remote['id']}/follow")
    if status != 200:
        report.check(direction, "shared inbox: a second local account follows",
                     False, f"status {status}")
        return

    def both_follow():
        st, body = mastodon.call("GET", "/api/v1/accounts/verify_credentials")
        if st != 200:
            return False
        st, followers = mastodon.call(
            "GET", f"/api/v1/accounts/{body['id']}/followers?limit=40"
        )
        if st != 200:
            return False
        local = [a for a in (followers or []) if "@" in (a.get("acct") or "")]
        return len(local) >= 2

    if not until(both_follow, seconds=FOLLOW_ROUND_TRIP):
        report.check(direction, "shared inbox: a second local account follows",
                     False, "Mastodon still sees one follower, so it would not "
                            "use the shared inbox")
        return
    report.check(direction, "shared inbox: a second local account follows", True)

    marker = f"shared-{int(time.time() * 1000)}"
    status, _ = mastodon.call(
        "POST", "/api/v1/statuses",
        {"status": f"hello {marker}", "visibility": "public"},
    )
    if status != 200:
        report.check(direction, "shared inbox: post a status", False, f"status {status}")
        return

    def seen_by(server):
        def check():
            st, body = server.call("GET", "/api/v1/timelines/home?limit=40")
            if st != 200 or not body:
                return False
            return any(marker in (s.get("content") or "") for s in body)
        return check

    for who, server in (("first", eunha), ("second", second)):
        report.check(
            direction, f"shared inbox: {who} follower receives it",
            bool(until(seen_by(server), seconds=45)),
            "not on that account's home timeline",
        )


# A report about an account on the other server, with `forward`, goes there as
# a `Flag` from the reporting server's instance actor, and becomes a report its
# moderators see. Each side's admin API is asked whether it arrived, by the
# comment it was sent with.
def check_report_forwarding(sender, receiver, target_acct, receiver_admin, report):
    direction = f"{sender.name}→{receiver.name}"
    target = until(lambda: find_account(sender, target_acct), seconds=30)
    if not target:
        report.check(direction, "forwarded report: find the account", False,
                     f"{sender.name} could not find {target_acct}")
        return
    marker = f"forwarded-{int(time.time() * 1000)}"
    status, body = sender.call("POST", "/api/v1/reports", {
        "account_id": target["id"], "comment": marker, "category": "spam",
        "forward": True,
    })
    report.check(direction, "forwarded report: filed", status == 200,
                 f"status {status}: {body}")
    if status != 200:
        return

    def arrived():
        st, reports = receiver.call("GET", "/api/v1/admin/reports?limit=200",
                                    token=receiver_admin)
        if st != 200:
            return None
        return next((r for r in reports or [] if r.get("comment") == marker), None)

    landed = until(arrived, seconds=45)
    report.check(direction, "forwarded report: arrives as a report", bool(landed),
                 "not among the receiver's reports")
    if landed:
        acct = (landed.get("target_account") or {}).get("account", {}).get("acct", "")
        report.check(direction, "forwarded report: about the right account",
                     acct.split("@")[0] == target_acct.split("@")[0],
                     f"it names {acct}")


# A move is a settings form in Mastodon and a REST call in eunha; either way the
# old account's followers on the *other* server should end up following the
# new one — Mastodon's `MoveWorker`, re-following through
# `UnfollowFollowWorker` — and the other server should show the old account as
# moved. `follower` follows `old_acct` from the far side before the move.
def check_move(origin, far, old_acct, new_acct, follower, follower_acct, move, report):
    direction = f"{origin.name}→{far.name}"
    old = until(lambda: find_account(far, old_acct), seconds=30)
    new = until(lambda: find_account(far, new_acct), seconds=30)
    if not (old and new):
        report.check(direction, "move: the far side knows both accounts", False,
                     f"{far.name} could not find {old_acct} and {new_acct}")
        return
    st, rels = follower.call("GET", f"/api/v1/accounts/relationships?id[]={old['id']}")
    if not (st == 200 and rels and rels[0].get("following")):
        st, _ = follower.call("POST", f"/api/v1/accounts/{old['id']}/follow")

        def following_old():
            st, rels = follower.call("GET", f"/api/v1/accounts/relationships?id[]={old['id']}")
            return st == 200 and bool(rels) and rels[0].get("following") is True

        if not until(following_old, seconds=FOLLOW_ROUND_TRIP):
            report.check(direction, "move: a follower on the far side", False,
                         f"{follower_acct} does not follow {old_acct}")
            return
    report.check(direction, "move: a follower on the far side", True)

    ok, detail = move()
    report.check(direction, "move: the account moves", ok, detail)
    if not ok:
        return

    def shown_moved():
        found = find_account(far, old_acct)
        return found and (found.get("moved") or {}).get("acct", "").split("@")[0] == \
            new_acct.split("@")[0]

    report.check(direction, "move: the far side shows it moved",
                 bool(until(shown_moved, seconds=45)), "no `moved` naming the new account")

    def refollowed():
        st, rels = follower.call("GET", f"/api/v1/accounts/relationships?id[]={new['id']}")
        return st == 200 and bool(rels) and rels[0].get("following") is True

    report.check(direction, "move: the follower follows the new account",
                 bool(until(refollowed, seconds=FOLLOW_ROUND_TRIP)),
                 f"{follower_acct} does not follow {new_acct}")

    def let_go():
        st, rels = follower.call("GET", f"/api/v1/accounts/relationships?id[]={old['id']}")
        return st == 200 and bool(rels) and rels[0].get("following") is False

    report.check(direction, "move: and no longer the old one",
                 bool(until(let_go, seconds=FOLLOW_ROUND_TRIP)),
                 f"{follower_acct} still follows {old_acct}")

    def on_the_origin():
        st, me = origin.call("GET", "/api/v1/accounts/lookup?acct=" + new_acct.split("@")[0])
        if st != 200:
            return False
        st, followers = origin.call("GET", f"/api/v1/accounts/{me['id']}/followers?limit=80")
        return st == 200 and any(a.get("acct") == follower_acct for a in followers or [])

    report.check(direction, "move: the new account has the follower",
                 bool(until(on_the_origin, seconds=FOLLOW_ROUND_TRIP)),
                 f"{new_acct} does not list {follower_acct} as a follower")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--eunha", required=True)
    parser.add_argument("--eunha-token", required=True)
    parser.add_argument("--eunha-acct", required=True,
                        help="the eunha account's full handle, e.g. alice@host:3002")
    parser.add_argument("--eunha-second-token",
                        help="a second eunha account's token. Mastodon only uses a "
                             "peer's shared inbox when two accounts there follow the "
                             "same actor, so without one that path is never taken.")
    parser.add_argument("--mastodon", required=True)
    parser.add_argument(
        "--mastodon-host",
        help="Host header for Mastodon, when it answers on a name this machine "
        "cannot resolve. Rails refuses a request whose Host is not its "
        "LOCAL_DOMAIN, with a 403 on every endpoint including the ones that "
        "need no authentication — which reads as Mastodon being broken rather "
        "than as being addressed by the wrong name.",
    )
    parser.add_argument("--mastodon-token", required=True)
    parser.add_argument("--mastodon-acct", required=True)
    parser.add_argument("--eunha-second-acct",
                        help="the second eunha account's handle, which a Mastodon "
                             "report is about and which follows across a move")
    parser.add_argument("--eunha-third-token",
                        help="a third eunha account, which the first moves to")
    parser.add_argument("--eunha-third-acct")
    parser.add_argument("--mastodon-second-acct",
                        help="a second Mastodon account, which the first moves to")
    parser.add_argument("--mastodon-runner",
                        help="a command that runs Ruby given as its last argument in "
                             "Mastodon (`bin/rails runner`): Mastodon moves an "
                             "account through a settings form, with no API to call")
    parser.add_argument("--only", choices=["to-eunha", "to-mastodon"])
    args = parser.parse_args()

    eunha = Server("eunha", args.eunha, args.eunha_token)
    mastodon = Server(
        "mastodon", args.mastodon, args.mastodon_token,
        extra_headers={"Host": args.mastodon_host} if args.mastodon_host else None,
    )

    report = Report()
    if args.only != "to-mastodon":
        print("Mastodon → eunha")
        run_direction(mastodon, eunha, args.mastodon_acct, args.eunha_acct, report)
    if args.only != "to-eunha":
        print("\neunha → Mastodon")
        run_direction(eunha, mastodon, args.eunha_acct, args.mastodon_acct, report)
    if args.eunha_second_token and args.only != "to-mastodon":
        print("\nShared inbox")
        second = Server("eunha", args.eunha, args.eunha_second_token)
        check_shared_inbox(eunha, mastodon, second, args.mastodon_acct, report)

    # Both tokens have the admin scopes and their accounts are owners, so each
    # side's moderators can be asked what arrived.
    if args.eunha_second_acct and args.only is None:
        print("\nForwarded reports")
        check_report_forwarding(eunha, mastodon, args.mastodon_acct,
                                args.mastodon_token, report)
        check_report_forwarding(mastodon, eunha, args.eunha_second_acct,
                                args.eunha_token, report)

    # Last, because a moved account is restricted afterwards.
    if args.eunha_third_token and args.only is None:
        print("\nMoves")
        third = Server("eunha", args.eunha, args.eunha_third_token)

        def move_on_eunha():
            st, body = third.call("POST", "/api/v1/profile/aliases",
                                  {"acct": args.eunha_acct})
            if st != 200:
                return False, f"alias: status {st}: {body}"
            st, body = eunha.call("POST", "/api/v1/accounts/move", {
                "acct": args.eunha_third_acct,
                "current_username": args.eunha_acct.split("@")[0],
            })
            return st == 200, f"move: status {st}: {body}"

        check_move(eunha, mastodon, args.eunha_acct, args.eunha_third_acct,
                   mastodon, args.mastodon_acct, move_on_eunha, report)

    if args.mastodon_runner and args.mastodon_second_acct and args.only is None:
        second = Server("eunha", args.eunha, args.eunha_second_token)
        old, new = args.mastodon_acct.split("@")[0], args.mastodon_second_acct.split("@")[0]

        def move_on_mastodon():
            import shlex
            import subprocess

            # `Settings::MigrationsController#create`, less the form: the
            # alias on the new account, the migration, and `MoveService`.
            ruby = f"""
              old = Account.find_local!({old!r})
              new = Account.find_local!({new!r})
              AccountAlias.find_or_create_by!(account: new, acct: old.local_username_and_domain)
              migration = old.migrations.create!(acct: new.local_username_and_domain)
              MoveService.new.call(migration)
              puts "moved"
            """
            done = subprocess.run(shlex.split(args.mastodon_runner) + [ruby],
                                  capture_output=True, text=True)
            return "moved" in done.stdout, (done.stdout + done.stderr)[-400:]

        check_move(mastodon, eunha, args.mastodon_acct, args.mastodon_second_acct,
                   second, args.eunha_second_acct, move_on_mastodon, report)

    failures = report.failed()
    print(f"\n{len(report.results) - len(failures)}/{len(report.results)} checks passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
