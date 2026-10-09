#!/bin/bash
# Federate eunha with a real Mastodon 4.7.2, both directions, and check what lands.
#
# eunha's other federation tests run eunha against eunha, where both sides share
# eunha's reading of ActivityPub — so a misreading is invisible. This builds a
# pair that shares nothing but the specification.
#
# Both servers run inside one container network and reach each other by name.
# eunha used to run on the host behind a host-side Caddy, which cannot work:
# `mastodon.test` is a network alias, so a host process cannot resolve it, and
# eunha could not fetch an actor's key to verify a single inbound activity. In
# the network they resolve each other, and the harness needs no `/etc/hosts`
# entry, no trusted certificate on the host, and no eunha built for this machine.
#
# Needs a container runtime, openssl and python3. Everything else is set up here
# and torn down at the end.
#
#   scripts/federation_test.sh
#   scripts/federation_test.sh --keep      # leave both running to poke at
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="${FEDERATION_WORK_DIR:-/tmp/eunha-federation}"
KEEP=""
[ "${1:-}" = "--keep" ] && KEEP=1

# `eunha.test` and `mastodon.test` on 443, resolved inside the compose network by
# aliases on the two proxies. The port is not a detail: Mastodon webfingers an
# account by the *host* of its actor URI and drops any port, so a peer on
# `eunha.test:3002` is looked up as `eunha.test` on 443 and every delivery fails
# as what looks like a signature error. Real instances are on 443; so are these.
EUNHA_DOMAIN="eunha.test"
MASTODON_DOMAIN="mastodon.test"
# Plain HTTP on the host, for this script to drive. Nothing federates over these.
EUNHA_HOST_PORT=3001
MASTODON_HOST_PORT=3005

for tool in docker openssl python3; do
  command -v "$tool" >/dev/null || { echo "!! $tool is not on PATH" >&2; exit 1; }
done

cleanup() {
  [ -n "$KEEP" ] && {
    echo "==> Left running (--keep): eunha :$EUNHA_HOST_PORT, Mastodon :$MASTODON_HOST_PORT"
    return
  }
  echo "==> Tearing down"
  docker compose -f "$WORK/docker-compose.yml" down -v >/dev/null 2>&1 || true
}
trap cleanup EXIT

rm -rf "$WORK"
mkdir -p "$WORK"
cp "$ROOT/scripts/mastodon-federation-compose.yml" "$WORK/docker-compose.yml"

compose() { docker compose -f "$WORK/docker-compose.yml" "$@"; }

echo "==> Certificates"
# A throwaway CA and one leaf covering both names, with openssl rather than
# mkcert. Nothing on the host validates these — the script drives both servers
# over plain published ports — so there is no reason to install a trust root into
# anyone's keychain, and CI needs no tool the laptop does not already have.
openssl req -x509 -newkey rsa:2048 -nodes -days 2 \
  -subj "/CN=eunha federation test CA" \
  -keyout "$WORK/ca-key.pem" -out "$WORK/ca.pem" 2>/dev/null
cat > "$WORK/leaf.cnf" <<EOF
[req]
distinguished_name = dn
[dn]
[ext]
subjectAltName = DNS:$EUNHA_DOMAIN,DNS:$MASTODON_DOMAIN
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
EOF
openssl req -newkey rsa:2048 -nodes -subj "/CN=$EUNHA_DOMAIN" \
  -keyout "$WORK/eunha-key.pem" -out "$WORK/leaf.csr" -config "$WORK/leaf.cnf" 2>/dev/null
openssl x509 -req -in "$WORK/leaf.csr" -days 2 \
  -CA "$WORK/ca.pem" -CAkey "$WORK/ca-key.pem" -CAcreateserial \
  -extfile "$WORK/leaf.cnf" -extensions ext -out "$WORK/eunha.pem" 2>/dev/null

# Both servers read `SSL_CERT_FILE`: Ruby because the image runs unprivileged and
# cannot write its trust store, eunha because reqwest is rustls with native roots
# and that is where they come from. The bundle is the image's own roots with this
# CA appended — appended, not replacing, or each would trust this CA and no
# public one, and anything reaching the wider internet would fail obscurely.
docker run --rm ghcr.io/mastodon/mastodon:v4.7.2 \
  cat /etc/ssl/certs/ca-certificates.crt > "$WORK/ca-bundle.crt"
cat "$WORK/ca.pem" >> "$WORK/ca-bundle.crt"

cat > "$WORK/Caddyfile.eunha" <<'EOF'
{
	admin off
	auto_https off
}
:443 {
	tls /certs/eunha.pem /certs/eunha-key.pem
	log {
		output stdout
		format console
	}
	reverse_proxy eunha:3000 {
		header_up X-Forwarded-Proto https
	}
}
EOF
cat > "$WORK/Caddyfile" <<'EOF'
{
	admin off
	auto_https off
}
:443 {
	tls /certs/eunha.pem /certs/eunha-key.pem
	reverse_proxy web:3000 {
		header_up X-Forwarded-Proto https
	}
}
EOF

VAPID_KEY=$(openssl ecparam -genkey -name prime256v1 -noout 2>/dev/null)
VAPID_PRIV=$(printf '%s' "$VAPID_KEY" | openssl pkcs8 -topk8 -nocrypt 2>/dev/null)
# `tr -d '=\n'`: GNU base64 wraps at 76 columns and the key would arrive split
# over two lines, breaking the TOML. macOS base64 does not wrap, which is why
# this only ever failed on Linux.
VAPID_PUB=$(printf '%s' "$VAPID_KEY" | openssl ec -pubout -outform DER 2>/dev/null \
  | tail -c 65 | base64 | tr '+/' '-_' | tr -d '=\n')

cat > "$WORK/eunha-config.toml" <<EOF
database_url = "postgres://eunha:eunha@eunha-db/eunha"

# The pair lives on a container network, whose addresses are private. Federation
# refuses those by default — rightly, since otherwise a peer could name an
# address and have this server probe its own network — so the ranges are named
# here, exactly as an instance behind split-horizon DNS or a mesh network would.
allowed_private_networks = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "127.0.0.0/8", "::1/128", "fc00::/7"]
redis_url = "redis://redis:6379/13"
bind_address = "0.0.0.0:3000"

[instance]
domain = "$EUNHA_DOMAIN"
title = "eunha"
description = ""
short_description = ""
contact_email = "alice@localhost"
registrations_open = false
approval_required = false
vapid_private_key = """
$VAPID_PRIV
"""
vapid_public_key = "$VAPID_PUB"
privacy_policy = ""
terms_of_service = ""

[media_storage]
bucket = "f"
region = "auto"
endpoint = "http://localhost:9999"
access_key_id = "f"
secret_access_key = "f"
base_url = "http://localhost:9999"

EOF

echo "==> Building eunha"
# Built here rather than by compose: the compose file is copied into the scratch
# directory, so a `build:` stanza there would take that directory as its context.
docker build -q -t eunha:federation "$ROOT" >/dev/null

echo "==> Mastodon"
compose up -d db redis web sidekiq proxy

# The `Host` matters as much as the address. Rails refuses a request whose Host
# is not its `LOCAL_DOMAIN` — 403 on every endpoint, including the ones needing
# no authentication — and `mastodon.test` does not resolve on this machine, so
# the only way in is the published port with the name supplied by hand. Without
# it this loop spins forever against a Mastodon that is up and answering.
until curl -sf -m 3 -o /dev/null \
        -H "Host: $MASTODON_DOMAIN" -H "X-Forwarded-Proto: https" \
        "http://localhost:$MASTODON_HOST_PORT/api/v1/instance"; do sleep 5; done

# A worker, or nothing Mastodon defers ever happens — and delivery is deferred,
# so every activity bound for eunha would sit in a queue and the run would report
# eunha as unreachable. Sidekiq registers itself in Redis, so ask Redis rather
# than trusting that a container was started.
echo "==> Waiting for a worker"
workers() {
  local n
  n=$(compose exec -T redis redis-cli scard processes 2>/dev/null | tr -d '\r')
  case "$n" in
    "" | *[!0-9]*) echo 0 ;;
    *) echo "$n" ;;
  esac
}
for _ in $(seq 60); do
  [ "$(workers)" -gt 0 ] && break
  sleep 2
done
if [ "$(workers)" -lt 1 ]; then
  echo "!! no Sidekiq worker registered; nothing Mastodon defers will happen" >&2
  compose logs --tail=20 sidekiq >&2
  exit 1
fi

MASTODON_TOKEN=$(compose exec -T web bin/rails runner '
  a = Account.find_or_create_by!(username: "masto") { |x| x.domain = nil }
  u = User.find_by(email: "masto@localhost") || User.create!(
    email: "masto@localhost", password: SecureRandom.hex(16), account: a,
    agreement: true, approved: true, confirmed_at: Time.now.utc)
  u.update!(approved: true, confirmed_at: Time.now.utc)
  app = Doorkeeper::Application.find_or_create_by!(name: "fed") do |x|
    x.redirect_uri = "urn:ietf:wg:oauth:2.0:oob"; x.scopes = "read write follow"
  end
  t = Doorkeeper::AccessToken.find_or_create_by!(
    application: app, resource_owner_id: u.id, revoked_at: nil
  ) { |x| x.scopes = "read write follow"; x.expires_in = nil }
  puts t.token' | tr -d '\r' | tail -1)
[ -n "$MASTODON_TOKEN" ] || { echo "!! Mastodon minted no token" >&2; exit 1; }

echo "==> eunha"
compose up -d eunha-db
# Migrations before the server, not by it: eunha refuses to serve a database
# behind its binary, so starting the container first would only crash-loop.
compose run --rm --no-deps eunha ./eunha migrate

# eunha refuses to deliver an unsigned activity, and an account seeded straight
# into SQL has no key — so give alice one. eunha would generate this itself for
# an account created through its own API.
ALICE_KEY=$(openssl genrsa 2048 2>/dev/null)
ALICE_PUB=$(printf '%s' "$ALICE_KEY" | openssl rsa -pubout 2>/dev/null)
# Bob exists so that a shared-inbox delivery has more than one local recipient.
# Mastodon posts a status to eunha's `/inbox` whether one account follows or
# two, so that path was never unexercised — but with a single follower, fanning
# a delivery out to everyone it concerns and handing it to that one account are
# the same act, and only one of them is being tested.
BOB_KEY=$(openssl genrsa 2048 2>/dev/null)
BOB_PUB=$(printf '%s' "$BOB_KEY" | openssl rsa -pubout 2>/dev/null)
compose exec -T eunha-db psql -q -U eunha -d eunha -v ON_ERROR_STOP=1 >/dev/null <<SQL
INSERT INTO accounts (id, username, domain, display_name, note, created_at, updated_at)
VALUES (1, 'alice', NULL, 'Alice', '', now(), now()),
       (2, 'bob',   NULL, 'Bob',   '', now(), now()) ON CONFLICT DO NOTHING;
INSERT INTO users (id, email, account_id, created_at, updated_at, confirmed_at, approved, encrypted_password)
VALUES (1, 'alice@localhost', 1, now(), now(), now(), true, 'x'),
       (2, 'bob@localhost',   2, now(), now(), now(), true, 'x') ON CONFLICT DO NOTHING;
INSERT INTO oauth_applications (id, name, uid, secret, redirect_uri, scopes, created_at, updated_at)
VALUES (1, 'fed', 'u', 's', 'urn:ietf:wg:oauth:2.0:oob', 'read write follow push', now(), now())
ON CONFLICT DO NOTHING;
INSERT INTO oauth_access_tokens (id, token, resource_owner_id, application_id, scopes, created_at)
VALUES (1, 'eunha-federation-token', 1, 1, 'read write follow push', now()),
       (2, 'eunha-federation-bob-token', 2, 1, 'read write follow push', now()) ON CONFLICT DO NOTHING;
UPDATE accounts SET private_key = \$P\$$ALICE_KEY\$P\$, public_key = \$U\$$ALICE_PUB\$U\$
WHERE id = 1;
UPDATE accounts SET private_key = \$P\$$BOB_KEY\$P\$, public_key = \$U\$$BOB_PUB\$U\$
WHERE id = 2;
SQL

# No peer seeding. eunha resolves Mastodon's actor over the wire, webfinger and
# all, because both sides are on this network under names each can resolve.
compose up -d eunha eunha-proxy
until curl -sf -m 3 -o /dev/null "http://localhost:$EUNHA_HOST_PORT/api/v1/instance"; do sleep 2; done

# Mastodon's circuit breaker remembers failures, and an inbox it has tripped to
# red is one it stops attempting entirely — no request, no error, empty queues.
# A run that begins with a red breaker from a previous run reports that eunha is
# unreachable, which is not true and takes a long time to disbelieve.
compose exec -T redis sh -c \
  'redis-cli --scan --pattern "*stoplight*" | xargs -r redis-cli del' >/dev/null 2>&1 || true

echo "==> Federating"
STATUS=0
# `--opt=value`, not `--opt value`: Doorkeeper mints tokens with
# `SecureRandom.urlsafe_base64`, whose alphabet includes `-`, so a token
# beginning with one is read by argparse as an option name rather than a value.
# Everything after the `=` is the value however it starts.
python3 "$ROOT/scripts/federation_test.py" \
  --eunha="http://localhost:$EUNHA_HOST_PORT" \
  --eunha-token="eunha-federation-token" \
  --eunha-acct="alice@$EUNHA_DOMAIN" \
  --eunha-second-token="eunha-federation-bob-token" \
  --mastodon="http://localhost:$MASTODON_HOST_PORT" \
  --mastodon-host="$MASTODON_DOMAIN" \
  --mastodon-token="$MASTODON_TOKEN" \
  --mastodon-acct="masto@$MASTODON_DOMAIN" || STATUS=$?

# The checks above prove both accounts received the status; this proves it came
# through the shared inbox rather than two personal ones. It does not prove the
# fan-out had two recipients — Mastodon uses the shared inbox for a single
# follower too — so it is a floor, not a ceiling: if eunha ever stops
# advertising `sharedInbox`, or Mastodon stops honouring it, this says so.
echo
echo "==> Which inbox Mastodon used"
# `grep -c`, not `grep -q`: under `pipefail` a `grep -q` exits on the first match
# and SIGPIPEs the process feeding it, so the pipeline reports failure precisely
# when the thing it looks for is found. This check called itself a failure while
# four deliveries sat in the log.
#
# eunha logs each activity it accepts with the inbox it came through
# (`inbox=shared` or `inbox=personal`); ojak serves the inbox, so the request
# path is no longer what is logged.
SHARED_HITS=$(compose logs eunha 2>/dev/null | grep -c "inbox=shared" || true)
if [ "${SHARED_HITS:-0}" -gt 0 ]; then
  echo "  [ok  ] $SHARED_HITS deliveries reached eunha's shared inbox"
else
  echo "  [FAIL] nothing reached eunha's shared inbox: Mastodon addressed the" >&2
  echo "         personal ones, so that path was not exercised" >&2
  STATUS=1
fi

exit $STATUS
