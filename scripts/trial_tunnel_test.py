#!/usr/bin/env python3
"""Test public trial discovery through Serveo and Pinggy, without bypass headers."""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

from trial_federation_test import command, stop, stream, wait_line


def fetch(url, accept='application/json'):
    request = urllib.request.Request(url, headers={'Accept': accept})
    deadline = time.monotonic() + 25
    while True:
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                return json.load(response)
        except (urllib.error.URLError, json.JSONDecodeError, TimeoutError) as error:
            if time.monotonic() >= deadline:
                raise RuntimeError(f'No public JSON at {urllib.parse.urlparse(url).path}') from error
            time.sleep(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', default='target/debug/eunha')
    parser.add_argument('--image', default='ghcr.io/eunha-space/eunha:main')
    parser.add_argument('--provider', choices=['serveo.net', 'pinggy.io'], action='append')
    args = parser.parse_args()
    for provider in args.provider or ['serveo.net', 'pinggy.io']:
        root = Path(tempfile.mkdtemp(prefix='eunha-tunnel-test-'))
        os.chmod(root, 0o700)
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            port = listener.getsockname()[1]
        process = subprocess.Popen([
            str(Path(args.binary).absolute()), 'try', '--email', 'trial@gmail.com',
            '--directory', str(root), '--port', str(port), '--image', args.image,
            '--tunnel-service', provider,
        ], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, bufsize=1)
        try:
            print(f'Checking {provider}…', flush=True)
            wait_line(process, stream(process), lambda line: 'Public trial running.' in line)
            public = (root / 'origin').read_text()
            domain = urllib.parse.urlparse(public).hostname
            assert fetch(public + '/api/v2/instance')['domain'] == domain
            resource = urllib.parse.urlencode({'resource': f'acct:owner@{domain}'})
            finger = fetch(public + '/.well-known/webfinger?' + resource, 'application/jrd+json')
            actor_url = next(link['href'] for link in finger['links'] if link['rel'] == 'self')
            assert actor_url.startswith(public + '/')
            # Mastodon 5.0 defaults to signed fetch for user actors. The
            # public instance actor exposes the key peers need to sign those fetches.
            instance_actor = fetch(public + '/actor', 'application/activity+json')
            assert instance_actor['id'] == public + '/actor'
            assert instance_actor['publicKey']['owner'] == instance_actor['id']
            print(f'PASS {provider}: public HTTPS, WebFinger and ActivityPub actor', flush=True)
        finally:
            stop(process)
            if (root / 'compose.json').exists():
                running = command(['docker', 'compose', '-f', str(root / 'compose.json'),
                                   'ps', '--status', 'running', '--services']).strip()
                if running:
                    command(['docker', 'compose', '-f', str(root / 'compose.json'), 'down'])
                    raise RuntimeError('Ctrl-C left trial containers running')
                assert (root / 'config.toml').exists(), 'stop must retain configuration'
            print(f'Test configuration and data retained: {root}', flush=True)
        assert process.returncode == 0, 'trial did not stop cleanly'
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
