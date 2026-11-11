#!/usr/bin/env python3
"""Exercise a real `eunha try` and Mastodon through public HTTPS tunnels.

Requires Docker Compose, OpenSSH and a built eunha binary. Credentials stay in
memory or the private scratch directory; they are never printed. No existing
instance or account is used. All containers and tunnels stop on exit; volumes
and private configuration are retained for diagnosis.
"""
import argparse
import json
import os
from pathlib import Path
import queue
import re
import secrets
import signal
import subprocess
import tempfile
import threading
import time
import urllib.request

from federation_test import Report, Server, run_direction, until


def command(args, input=None):
    result = subprocess.run(args, input=input, capture_output=True, text=True, timeout=300)
    if result.returncode:
        raise RuntimeError(f"command failed: {' '.join(args[:3])} (exit {result.returncode})")
    return result.stdout


def stream(process):
    lines = queue.Queue()

    def drain():
        for line in iter(process.stdout.readline, ''):
            lines.put(line)
        lines.put(None)

    threading.Thread(target=drain, daemon=True).start()
    return lines


def wait_line(process, lines, predicate, seconds=180):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            line = lines.get(timeout=1)
        except queue.Empty:
            if process.poll() is not None:
                break
            continue
        if line is None:
            break
        match = predicate(line)
        if match:
            return match
    raise RuntimeError('process did not announce readiness before its deadline')


def stop(process):
    if process and process.poll() is None:
        process.send_signal(signal.SIGINT)
        try:
            process.wait(timeout=40)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', default='target/debug/eunha')
    parser.add_argument('--image', default='ghcr.io/eunha-space/eunha:main')
    parser.add_argument('--trial-port', type=int, default=31784)
    parser.add_argument('--mastodon-port', type=int, default=31786)
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix='eunha-public-federation-'))
    os.chmod(root, 0o700)
    trial_dir = root / 'trial'
    trial = None
    tunnel = None
    compose_file = root / 'compose.json'

    def compose(*parts, input=None):
        return command(['docker', 'compose', '-f', str(compose_file), *parts], input)

    try:
        print('Starting a real eunha trial through Pinggy…', flush=True)
        trial = subprocess.Popen([
            str(Path(args.binary).absolute()), 'try', '--email', 'trial@gmail.com',
            '--directory', str(trial_dir), '--port', str(args.trial_port),
            '--image', args.image, '--tunnel-service', 'pinggy.io',
        ], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, bufsize=1)
        lines = stream(trial)
        wait_line(trial, lines, lambda line: 'Public trial running.' in line)
        trial_url = (trial_dir / 'origin').read_text()
        with urllib.request.urlopen(trial_url + '/api/v2/instance', timeout=20) as response:
            assert response.status == 200 and json.load(response)['domain'] == trial_url.removeprefix('https://')
        print('PASS: trial API is reachable over public HTTPS without bypass headers', flush=True)

        # Reuse only the disposable identity generated for this test trial.
        tunnel = subprocess.Popen([
            'ssh', '-F', '/dev/null', '-p', '443', '-T', '-l', 'eunha',
            '-o', 'StrictHostKeyChecking=yes',
            '-o', f'UserKnownHostsFile="{trial_dir / "tunnel-known-hosts"}"',
            '-o', 'GlobalKnownHostsFile=/dev/null', '-o', 'BatchMode=no',
            '-o', 'PasswordAuthentication=no', '-o', 'NumberOfPasswordPrompts=1',
            '-o', 'IdentityAgent=none', '-o', 'IdentitiesOnly=yes',
            '-i', str(trial_dir / 'tunnel-identity'),
            '-o', 'ExitOnForwardFailure=yes', '-o', 'ServerAliveInterval=30',
            '-R', f'80:127.0.0.1:{args.mastodon_port}', 'serveo.net',
        ], env=dict(os.environ, SSH_ASKPASS_REQUIRE='force', SSH_ASKPASS='/usr/bin/false'),
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, bufsize=1)
        lines = stream(tunnel)
        pattern = re.compile(r'https://[a-z0-9-]+\.(?:serveo\.net|serveousercontent\.com)(?=[/\s]|$)')
        mastodon_url = wait_line(tunnel, lines, lambda line: (m.group(0) if (m := pattern.search(line)) else None), 20)
        domain = mastodon_url.removeprefix('https://')
        password = secrets.token_hex(24)
        env = {
            'RAILS_ENV': 'production', 'SECRET_KEY_BASE': secrets.token_hex(64),
            'OTP_SECRET': secrets.token_hex(64),
            'ACTIVE_RECORD_ENCRYPTION_PRIMARY_KEY': secrets.token_hex(16),
            'ACTIVE_RECORD_ENCRYPTION_DETERMINISTIC_KEY': secrets.token_hex(16),
            'ACTIVE_RECORD_ENCRYPTION_KEY_DERIVATION_SALT': secrets.token_hex(16),
            'DB_HOST': 'db', 'DB_USER': 'mastodon', 'DB_PASS': password,
            'DB_NAME': 'mastodon', 'REDIS_HOST': 'redis', 'LOCAL_DOMAIN': domain,
            'RAILS_SERVE_STATIC_FILES': 'true', 'ES_ENABLED': 'false',
            'SMTP_DELIVERY_METHOD': 'test', 'RAILS_LOG_LEVEL': 'warn',
        }
        image = 'ghcr.io/mastodon/mastodon:v5.0.0-beta.1'
        spec = {'name': 'eunha-public-masto-' + secrets.token_hex(6), 'services': {
            'db': {'image': 'postgres:16-alpine', 'environment': {
                'POSTGRES_USER': 'mastodon', 'POSTGRES_PASSWORD': password, 'POSTGRES_DB': 'mastodon'},
                'volumes': ['postgres:/var/lib/postgresql/data'],
                'healthcheck': {'test': ['CMD-SHELL', 'pg_isready -U mastodon'], 'interval': '2s', 'timeout': '5s', 'retries': 60}},
            'redis': {'image': 'redis:7-alpine', 'volumes': ['redis:/data']},
            'web': {'image': image, 'environment': env,
                'command': ['bash', '-c', 'bin/rails db:prepare && bin/rails server -b 0.0.0.0 -p 3000'],
                'depends_on': {'db': {'condition': 'service_healthy'}, 'redis': {'condition': 'service_started'}}},
            'sidekiq': {'image': image, 'environment': env, 'command': ['bundle', 'exec', 'sidekiq'],
                'restart': 'on-failure', 'depends_on': ['web']},
            'proxy': {'image': 'caddy:2-alpine', 'ports': [f'127.0.0.1:{args.mastodon_port}:80'],
                'volumes': ['./Caddyfile:/etc/caddy/Caddyfile:ro'], 'depends_on': ['web']},
        }, 'volumes': {'postgres': {}, 'redis': {}}}
        compose_file.write_text(json.dumps(spec))
        (root / 'Caddyfile').write_text(':80 {\n reverse_proxy web:3000 {\n header_up Host ' + domain + '\n header_up X-Forwarded-Proto https\n }\n}\n')
        print('Starting an isolated Mastodon with a second public HTTPS tunnel…', flush=True)
        compose('up', '-d')
        mastodon = Server('Mastodon', f'http://127.0.0.1:{args.mastodon_port}', '', {'Host': domain})
        if not until(lambda: mastodon.call('GET', '/api/v1/instance')[0] == 200, seconds=240, interval=2):
            raise RuntimeError('Mastodon did not become ready')
        workers = compose('exec', '-T', 'redis', 'redis-cli', 'scard', 'processes').strip()
        if not workers.isdigit() or int(workers) < 1:
            raise RuntimeError('Mastodon has no delivery worker')
        ruby = '''
a = Account.create!(username: 'masto', domain: nil)
u = User.create!(email: 'masto@gmail.com', password: SecureRandom.hex(24), account: a,
  agreement: true, approved: true, confirmed_at: Time.now.utc)
u.update!(approved: true, confirmed_at: Time.now.utc, role: UserRole.find_by!(name: 'Owner'))
app = Doorkeeper::Application.create!(name: 'public-federation-test',
  redirect_uri: 'urn:ietf:wg:oauth:2.0:oob', scopes: 'read write follow')
t = Doorkeeper::AccessToken.create!(application: app, resource_owner_id: u.id,
  scopes: 'read write follow', expires_in: nil)
puts({token: t.token}.to_json)
'''
        mastodon.token = json.loads(compose('exec', '-T', 'web', 'bin/rails', 'runner', '-', input=ruby).splitlines()[-1])['token']
        # Mint a test client token for the owner created by `eunha try`.
        token = secrets.token_urlsafe(32)
        sql = f"""
WITH app AS (
 INSERT INTO oauth_applications (name, uid, secret, redirect_uri, scopes, created_at, updated_at)
 VALUES ('public-federation-test', '{secrets.token_hex(16)}', '{secrets.token_hex(16)}',
 'urn:ietf:wg:oauth:2.0:oob', 'read write follow', now(), now()) RETURNING id
)
INSERT INTO oauth_access_tokens (token, resource_owner_id, application_id, scopes, created_at)
SELECT '{token}', users.id, app.id, 'read write follow', now() FROM users
JOIN accounts ON accounts.id = users.account_id CROSS JOIN app WHERE accounts.username = 'owner';
"""
        command(['docker', 'compose', '-f', str(trial_dir / 'compose.json'), 'exec', '-T', 'postgres',
                 'psql', '-q', '-v', 'ON_ERROR_STOP=1', '-U', 'eunha', '-d', 'eunha'], sql)
        eunha_domain = trial_url.removeprefix('https://')
        eunha = Server('eunha trial', f'http://127.0.0.1:{args.trial_port}', token, {'Host': eunha_domain})
        # Confirm the peer is publicly reachable before interpreting an AP
        # resolution failure as a server bug.
        with urllib.request.urlopen(mastodon_url + '/api/v2/instance', timeout=20) as response:
            assert response.status == 200 and json.load(response)['domain'] == domain
        report = Report()
        run_direction(eunha, mastodon, f'owner@{eunha_domain}', f'masto@{domain}', report)
        run_direction(mastodon, eunha, f'masto@{domain}', f'owner@{eunha_domain}', report)
        print(f'Public federation: {len(report.results) - len(report.failed())}/{len(report.results)} checks passed', flush=True)
        return 1 if report.failed() else 0
    finally:
        if compose_file.exists():
            try:
                compose('down')
            except RuntimeError:
                print('Could not stop Mastodon; inspect the private test directory.', flush=True)
        stop(tunnel)
        stop(trial)
        trial_compose = trial_dir / 'compose.json'
        if trial_compose.exists():
            command(['docker', 'compose', '-f', str(trial_compose), 'down'])
        print(f'Test configuration and data retained: {root}', flush=True)


if __name__ == '__main__':
    raise SystemExit(main())
