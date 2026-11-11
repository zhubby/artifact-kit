#!/usr/bin/env python3
"""Wait for actual PostgreSQL and authenticated S3 readiness, preserving startup errors."""

import argparse
import json
import os
import subprocess
import time
import urllib.error
import urllib.request


def diagnostics(container, started_at=None):
    arguments = ['docker', 'logs']
    if started_at:
        arguments += ['--since', started_at]
    result = subprocess.run(arguments + [container], capture_output=True, text=True, timeout=10)
    output = result.stdout + result.stderr
    for name, value in os.environ.items():
        if len(value) >= 8 and any(marker in name for marker in ('SECRET', 'PASSWORD', 'TOKEN', 'MASTER_KEY', 'DATABASE_URL')):
            output = output.replace(value, '[REDACTED]')
    if len(output) > 16000:
        output = output[:8000] + '\n[diagnostic output truncated]\n' + output[-8000:]
    return output


def wait_ready(container, probe, timeout):
    deadline = time.monotonic() + timeout
    state = {}
    while time.monotonic() < deadline:
        result = subprocess.run(['docker', 'inspect', '--format', '{{json .State}}', container], capture_output=True, text=True, timeout=10)
        if result.returncode != 0:
            raise RuntimeError(f'{container}: container does not exist or cannot be inspected')
        state = json.loads(result.stdout)
        if not state['Running']:
            raise RuntimeError(f'{container}: exited with status {state["ExitCode"]}\n{diagnostics(container, state.get("StartedAt"))}')
        try:
            if probe():
                print(f'READY {container}', flush=True)
                return
        except OSError:
            pass
        time.sleep(0.3)
    raise TimeoutError(f'{container}: service did not become ready\n{diagnostics(container, state.get("StartedAt"))}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--postgres', default='artifactory-postgres-test')
    parser.add_argument('--s3', default='artifactory-s3-test')
    parser.add_argument('--s3-url', default=os.environ.get('ARTIFACTORY_S3_ENDPOINT', 'http://127.0.0.1:19000'))
    parser.add_argument('--timeout', type=float, default=60)
    arguments = parser.parse_args()
    if arguments.timeout <= 0:
        parser.error('--timeout must be positive')

    def postgres_ready():
        # The entrypoint's temporary initialization server accepts Unix sockets
        # before it restarts. Only the final server opens its TCP listener.
        return subprocess.run(['docker', 'exec', arguments.postgres, 'pg_isready', '-h', '127.0.0.1', '-U', 'artifactory', '-d', 'artifactory'], capture_output=True, timeout=5).returncode == 0

    def s3_ready():
        try:
            with urllib.request.urlopen(arguments.s3_url, timeout=2):
                raise RuntimeError('local S3 accepted an anonymous request; check its authentication configuration')
        except urllib.error.HTTPError as error:
            with error:
                return error.code in (401, 403)

    wait_ready(arguments.postgres, postgres_ready, arguments.timeout)
    wait_ready(arguments.s3, s3_ready, arguments.timeout)


if __name__ == '__main__':
    main()
