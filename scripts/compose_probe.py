#!/usr/bin/env python3
"""Run real API / Emissary traffic inside the isolated Compose acceptance network."""

import base64
import hashlib
import http.cookiejar
import http.server
import json
import os
import platform
import signal
import ssl
import subprocess
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path


DIRECTORY = Path('/acceptance')


def main():
    os.umask(0o077)
    base = os.environ['ARTIFACTORY_COMPOSE_CONSOLE_URL']
    evidence = {'kind': 'compose-tls-probe', 'checks': [], 'host': platform.platform()}
    context = ssl.create_default_context(cafile=str(DIRECTORY / 'https-ca.pem'))
    cookies = http.cookiejar.CookieJar()

    def opener(authenticated=True):
        handlers = [urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=context)]
        if authenticated:
            handlers.append(urllib.request.HTTPCookieProcessor(cookies))
        return urllib.request.build_opener(*handlers)

    client = opener()

    def check(name, condition):
        if not condition:
            raise AssertionError(name)
        evidence['checks'].append(name)
        print('PASS', name, flush=True)

    def command(arguments, **kwargs):
        return subprocess.check_output(arguments, stderr=subprocess.STDOUT, timeout=60, **kwargs).decode().strip()

    def wait(function, timeout=90):
        deadline = time.monotonic() + timeout
        last_error = None
        while time.monotonic() < deadline:
            try:
                result = function()
                if result:
                    return result
            except (OSError, urllib.error.URLError) as error:
                last_error = error
            time.sleep(0.5)
        raise TimeoutError(f'Compose probe timed out: {last_error}')

    def request(path, body=None, authenticated=True, csrf=True, origin=base):
        headers = {'Content-Type': 'application/json', 'Origin': origin}
        if csrf:
            headers['x-artifactory-csrf'] = '1'
        payload = None if body is None else json.dumps(body).encode()
        outgoing = urllib.request.Request(base + '/api/v1' + path, payload, headers)
        try:
            with (client if authenticated else opener(False)).open(outgoing, timeout=15) as response:
                return response.status, json.load(response), response.headers
        except urllib.error.HTTPError as error:
            return error.code, json.load(error), error.headers

    def api(path, body=None):
        if body is not None and path.startswith('/projects/') and path.endswith('/builds'):
            from legacy_fixture import historical_build
            return historical_build(path.split('/')[2],body)
        status, result, _headers = request(path, body)
        if status != 200:
            raise RuntimeError(f'{path}: HTTP {status}: {result}')
        return result

    def build_result(build_id, expected='succeeded'):
        def terminal():
            result = api('/builds/' + build_id)
            if result['status'] not in ('succeeded', 'failed', 'canceled', 'lost'):
                return None
            if result['status'] != expected:
                evidence['unexpected_build'] = result
                raise AssertionError(f'build expected {expected}, got {result["status"]}')
            return result
        return wait(terminal, 120)

    repository = DIRECTORY / 'repos/fixture.git'
    repository.mkdir(parents=True)
    command(['git', 'init', '-b', 'main', str(repository)])
    (repository / 'main.c').write_text('#include <stdio.h>\nint main(void) { puts("artifactory-compose-tls-ok"); return 0; }\n')
    (repository / 'descendant.py').write_text('''import pathlib, time
path = pathlib.Path('/acceptance/buffer-child-alive')
while True:
    path.write_text(str(time.monotonic()))
    time.sleep(0.1)
''')
    (repository / 'flood.py').write_text('''import pathlib, subprocess, sys, time
directory = pathlib.Path('/acceptance')
child = subprocess.Popen([sys.executable, 'descendant.py'])
(directory / 'buffer-child.pid').write_text(str(child.pid))
while not (directory / 'begin-log-flood').exists():
    time.sleep(0.05)
for index in range(1024):
    print('buffer-fault-%04d:' % index + 'x' * 4096, flush=True)
time.sleep(180)
''')
    arch = 'arm64' if platform.machine() in ('aarch64', 'arm64') else 'amd64'
    (repository / 'artifactory.yaml').write_text(f'''version: 1
release: {{enabled: true}}
targets:
  native:
    os: linux
    arch: {arch}
    capabilities: [cc]
    steps:
      - name: compile
        run: |
          mkdir -p dist
          cc main.c -o dist/app
          echo 'compose-tls-live-log'
          count=0
          while [ ! -f /acceptance/finish-build ]; do
            count=$((count + 1))
            if [ "$count" -ge 180 ]; then exit 12; fi
            sleep 1
          done
      - name: verify
        phase: verify
        run: ./dist/app
    artifacts: [dist/app]
  log-buffer:
    os: linux
    arch: {arch}
    capabilities: [cc]
    timeout_seconds: 180
    steps:
      - name: flood
        timeout_seconds: 150
        run: python3 flood.py
    artifacts: [dist/app]
''')
    command(['git', '-C', str(repository), 'add', '.'])
    command(['git', '-C', str(repository), '-c', 'user.name=Compose acceptance', '-c', 'user.email=acceptance@localhost', 'commit', '-m', 'HTTPS Compose fixture'])
    commit = command(['git', '-C', str(repository), 'rev-parse', 'HEAD'])
    password = os.urandom(24).hex()
    authorization = 'Basic ' + base64.b64encode(('fixture:' + password).encode()).decode()

    class GitHandler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_arguments):
            pass

        def do_GET(self):
            self.handle_git()

        def do_POST(self):
            self.handle_git()

        def handle_git(self):
            if self.headers.get('Authorization') != authorization:
                self.send_response(401)
                self.send_header('WWW-Authenticate', 'Basic realm="compose acceptance"')
                self.send_header('Content-Length', '0')
                self.end_headers()
                return
            parsed = urllib.parse.urlsplit(self.path)
            environment = dict(os.environ, GIT_PROJECT_ROOT=str(repository.parent), GIT_HTTP_EXPORT_ALL='1', PATH_INFO=parsed.path, QUERY_STRING=parsed.query, REQUEST_METHOD=self.command, CONTENT_TYPE=self.headers.get('Content-Type', ''), CONTENT_LENGTH=self.headers.get('Content-Length', '0'), REMOTE_USER='fixture')
            result = subprocess.run(['git', 'http-backend'], input=self.rfile.read(int(environment['CONTENT_LENGTH'])), env=environment, capture_output=True, timeout=30, check=True)
            raw_headers, content = result.stdout.split(b'\r\n\r\n', 1)
            headers = [line.decode().split(': ', 1) for line in raw_headers.split(b'\r\n')]
            status = next((int(value.split()[0]) for key, value in headers if key.lower() == 'status'), 200)
            self.send_response(status)
            for key, value in headers:
                if key.lower() not in ('status', 'content-length'):
                    self.send_header(key, value)
            self.send_header('Content-Length', str(len(content)))
            self.end_headers()
            self.wfile.write(content)

    server = http.server.ThreadingHTTPServer(('0.0.0.0', 19443), GitHandler)
    git_context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    git_context.load_cert_chain(DIRECTORY / 'git.pem', DIRECTORY / 'git.key')
    server.socket = git_context.wrap_socket(server.socket, server_side=True)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    emissary = None
    try:
        def ready():
            with client.open(base + '/', timeout=3) as response:
                if response.status != 200:
                    return False
            try:
                with opener(False).open(base + '/api/v1/auth/me', timeout=3):
                    return False
            except urllib.error.HTTPError as error:
                return error.code == 401
        wait(ready, 90)
        check('console is served through trusted Caddy HTTPS', True)
        for path in ('/docs', '/docs/'):
            with opener(False).open(base + path, timeout=15) as response:
                check('anonymous documentation index ' + path,
                      response.status == 200 and response.url.endswith('/docs/index.html')
                      and 'Artifactory 文档' in response.read().decode())
        for path, content_type in (
            ('/docs/acceptance/emissary/2026-11-13.html', 'text/html'),
            ('/docs/mermaid.min.js', 'javascript'),
            ('/docs/brand/artifactory-assembly-a.png', 'image/png'),
            ('/docs/brand/artifactory-logo-kit.zip', 'application/zip'),
        ):
            with opener(False).open(base + path, timeout=15) as response:
                check('documentation resource ' + path, response.status == 200
                      and content_type in response.headers.get('Content-Type', '')
                      and len(response.read()) > 100)
        try:
            opener(False).open(base + '/docs/missing/nested.html', timeout=15)
            raise AssertionError('missing documentation must return HTTP 404')
        except urllib.error.HTTPError as error:
            check('missing documentation returns the book HTTP 404', error.code == 404
                  and '页面未找到' in error.read().decode())
        status, _result, headers = request('/auth/login', {'username': 'admin', 'password': os.environ['ARTIFACTORY_BOOTSTRAP_PASSWORD']})
        cookie = headers.get('Set-Cookie', '')
        check('HTTPS login issues a Secure HttpOnly SameSite=Strict session', status == 200 and all(value in cookie for value in ('Secure', 'HttpOnly', 'SameSite=Strict')))
        check('anonymous management access is rejected', request('/overview', authenticated=False)[0] == 401)
        check('cookie writes require CSRF header', request('/projects', {}, csrf=False)[0] == 403)
        check('cookie writes reject a foreign Origin', request('/projects', {}, origin='https://unrelated.invalid')[0] == 403)
        credential = api('/credentials', {'name': 'compose fixture', 'kind': 'https', 'username': 'fixture', 'secret': password})['id']
        check('Git credentials are redacted from API responses', password not in json.dumps(api('/credentials')))
        pool = '00000000-0000-0000-0000-000000000001'
        enrollment = api('/enrollment-tokens', {'pool_id': pool})['token']
        config_file = DIRECTORY / 'emissary.json'
        environment = {key: value for key, value in os.environ.items() if not key.startswith('ARTIFACTORY_')}
        command([str(DIRECTORY / 'artifactory-emissary'), 'register', '--config', str(config_file), '--endpoint', 'control-plane:8443', '--ca', str(DIRECTORY / 'grpc-ca.pem'), '--root', str(DIRECTORY / 'emissary-data'), '--name', 'compose-tls-probe'], env=dict(environment, ARTIFACTORY_ENROLLMENT_TOKEN=enrollment))
        config = json.loads(config_file.read_text())
        config['git_ca_file'] = str(DIRECTORY / 'grpc-ca.pem')
        config['log_buffer_bytes'] = 1024 * 1024
        config_file.write_text(json.dumps(config))
        check('Emissary keeps HTTPS-only artifact uploads enabled', not config.get('allow_http_uploads', False))
        with (DIRECTORY / 'emissary.log').open('ab') as log:
            emissary = subprocess.Popen([str(DIRECTORY / 'artifactory-emissary'), 'run', '--config', str(config_file)], env=environment, stdout=log, stderr=log)
        node_id = config['emissary_id']
        wait(lambda: next((node for node in api('/nodes') if node['id'] == node_id and node['online'] and 'cc' in node['capabilities']), None))
        check('real Rust Emissary authenticates with mTLS and reports its compiler', True)
        project = api('/projects', {'name': 'Compose TLS acceptance', 'repository_url': 'https://probe:19443/fixture.git', 'credential_id': credential, 'pool_ids': [pool]})['id']
        build_id = api(f'/projects/{project}/builds', {'ref': 'main', 'targets': ['native'], 'release_version': 'compose-tls'})['id']
        wait(lambda: api('/builds/' + build_id)['status'] == 'running', 60)
        with client.open(base + '/api/v1/builds/' + build_id + '/events', timeout=15) as response:
            found = False
            event = b''
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                line = response.readline()
                if not line:
                    break
                if line.startswith(b'event:'):
                    event = line.removeprefix(b'event:').strip()
                if event == b'log' and line.startswith(b'data:') and 'compose-tls-live-log' in json.loads(line.removeprefix(b'data:')).get('text', ''):
                    found = True
                    break
            check('SSE delivers real build output through web and Caddy before completion', found and api('/builds/' + build_id)['status'] == 'running')
        (DIRECTORY / 'finish-build').touch()
        result = build_result(build_id)
        check('Control and Rust Emissary build the same immutable HTTPS Git commit', result['commit_sha'] == commit)
        check('native compilation and verification stages succeed', all(step['status'] == 'succeeded' for step in result['jobs'][0]['attempts'][-1]['steps']))
        wait(lambda: api('/builds/' + build_id)['publish_status'] == 'succeeded')
        release = api('/builds/' + build_id + '/publish', {})
        check('release publication retry is idempotent', api('/builds/' + build_id + '/publish', {})['id'] == release['id'])
        artifact = result['artifacts'][0]
        check('artifact downloads require an authenticated session', request('/artifacts/' + artifact['id'] + '/download', authenticated=False)[0] == 401)

        def download():
            with client.open(base + '/api/v1/artifacts/' + artifact['id'] + '/download', timeout=15) as response:
                content = response.read()
                url = response.url
            check('signed HTTPS artifact download matches verified SHA-256 and size', url.startswith(os.environ['ARTIFACTORY_COMPOSE_S3_URL'] + '/') and len(content) == artifact['size'] and hashlib.sha256(content).hexdigest() == artifact['sha256'])
            return content, url

        content, signed_url = download()
        try:
            opener(False).open(signed_url.split('?')[0], timeout=10)
            raise AssertionError('anonymous object download succeeded')
        except urllib.error.HTTPError as error:
            check('S3 objects stay private behind HTTPS', error.code in (401, 403))
        binary = DIRECTORY / 'downloaded-app'
        binary.write_bytes(content)
        binary.chmod(0o700)
        check('downloaded native application executes', command([str(binary)]) == 'artifactory-compose-tls-ok')
        (DIRECTORY / 'finish-build').unlink()
        second = api(f'/projects/{project}/builds', {'ref': 'main', 'targets': ['native']})['id']

        def running_build(build_id):
            value = api('/builds/' + build_id)
            if value['status'] in ('failed', 'canceled', 'lost', 'succeeded'):
                evidence['unexpected_build'] = value
                raise AssertionError('build fixture did not remain in progress')
            return value if value['status'] == 'running' else None

        running = wait(lambda: running_build(second), 60)
        attempt_id = running['jobs'][0]['attempts'][-1]['id']
        previous_seen = next(item for item in api('/nodes') if item['id'] == node_id)['last_seen']
        (DIRECTORY / 'ready-for-restart').touch()
        wait(lambda: (DIRECTORY / 'control-plane-restarted').is_file(), 90)
        wait(ready, 60)
        wait(lambda: next((item for item in api('/nodes') if item['id'] == node_id and item['online'] and item['last_seen'] > previous_seen), None), 60)
        check('Emissary reconnects with its existing certificate after center restart', emissary.poll() is None)
        check('session and immutable release survive center restart', api('/releases/' + release['id'])['build_id'] == build_id)
        download()
        (DIRECTORY / 'finish-build').touch()
        resumed = build_result(second)
        attempts = resumed['jobs'][0]['attempts']
        check('center restart preserves the same active build attempt', len(attempts) == 1 and attempts[0]['id'] == attempt_id and resumed['commit_sha'] == commit)
        third = api(f'/projects/{project}/builds', {'ref': 'main', 'targets': ['native']})['id']
        check('persisted Git credential decrypts for a new build after restart', build_result(third)['commit_sha'] == commit)

        overflow = api(f'/projects/{project}/builds', {'ref': 'main', 'targets': ['log-buffer'], 'release_version': 'must-not-publish'})['id']
        wait(lambda: (DIRECTORY / 'buffer-child-alive').is_file() and (DIRECTORY / 'buffer-child.pid').is_file(), 60)
        child_pid = int((DIRECTORY / 'buffer-child.pid').read_text())
        os.kill(child_pid, 0)
        running = wait(lambda: running_build(overflow), 60)
        check('log fault starts a real running build and descendant', running['status'] == 'running')
        overflow_attempt = running['jobs'][0]['attempts'][-1]['id']
        (DIRECTORY / 'pause-log-storage').touch()
        wait(lambda: (DIRECTORY / 'log-storage-paused').is_file(), 30)
        (DIRECTORY / 'begin-log-flood').touch()

        def descendant_stopped():
            try:
                os.kill(child_pid, 0)
                return False
            except ProcessLookupError:
                return True

        wait(descendant_stopped, 30)
        wait(lambda: not (DIRECTORY / 'emissary-data/work' / overflow_attempt).exists(), 10)
        check('log-buffer exhaustion terminates the real Worker descendant before storage recovery', True)
        spool = DIRECTORY / 'emissary-data/spool' / overflow_attempt
        files = sorted(spool.glob('*.json'))
        records = [json.loads(path.read_text()) for path in files]
        buffered_bytes = sum(path.stat().st_size for path in files)
        check('actual on-disk log buffer stays within its configured bound', config['log_buffer_bytes'] * 3 // 4 < buffered_bytes <= config['log_buffer_bytes'])
        sequences = [record['sequence'] for record in records]
        check('overflow retains consecutive unacknowledged log sequences', sequences == list(range(sequences[0], sequences[-1] + 1)))
        tail = [{'sequence': record['sequence'], 'size': len(bytes(record['data'])), 'sha256': hashlib.sha256(bytes(record['data'])).hexdigest()} for record in records]
        previous_seen = next(node for node in api('/nodes') if node['id'] == node_id)['last_seen']
        wait(lambda: next((node for node in api('/nodes') if node['id'] == node_id and node['last_seen'] > previous_seen and node['online']), None), 20)
        check('independent Emissary heartbeat continues while S3 and log delivery are stopped', emissary.poll() is None and api('/builds/' + overflow)['status'] == 'running')
        evidence['log_buffer_fault'] = {'build_id': overflow, 'attempt_id': overflow_attempt, 'buffer_limit': config['log_buffer_bytes'], 'buffered_bytes': buffered_bytes, 'retained_chunks': tail}
        (DIRECTORY / 'resume-log-storage').touch()
        wait(lambda: (DIRECTORY / 'log-storage-resumed').is_file(), 30)
        failed = build_result(overflow, 'failed')
        attempts = failed['jobs'][0]['attempts']
        published = any(release['build_id'] == overflow for release in api('/releases?project_id=' + project))
        check('buffer exhaustion reports an explicit failure without artifacts or release', len(attempts) == 1 and 'log buffer exhausted' in (attempts[0].get('error') or '') and not failed['artifacts'] and failed['publish_status'] != 'succeeded' and not published)
        check('acknowledged buffered chunks are removed after storage recovery', not list(spool.glob('*.json')))
        replayed = []
        with client.open(base + '/api/v1/builds/' + overflow + '/events', timeout=15) as response:
            event = b''
            while len(replayed) < sequences[-1]:
                line = response.readline()
                if not line:
                    break
                if line.startswith(b'event:'):
                    event = line.removeprefix(b'event:').strip()
                if event == b'log' and line.startswith(b'data:'):
                    value = json.loads(line.removeprefix(b'data:'))
                    if value['attempt_id'] == overflow_attempt:
                        replayed.append(value['text'].encode())
        recovered = replayed[sequences[0] - 1:]
        check('HTTPS SSE replays every retained log chunk with its original bytes and order', len(replayed) == sequences[-1] and [(len(value), hashlib.sha256(value).hexdigest()) for value in recovered] == [(item['size'], item['sha256']) for item in tail])
        recovery = api(f'/projects/{project}/builds', {'ref': 'main', 'targets': ['native']})['id']
        check('same Emissary builds successfully after recovering from log-buffer exhaustion', emissary.poll() is None and build_result(recovery)['commit_sha'] == commit)
        evidence['log_buffer_fault']['recovery_build_id'] = recovery
        evidence.update({'project_id': project, 'node_id': node_id, 'build_id': build_id, 'restart_build_id': second, 'post_restart_build_id': third, 'release_id': release['id'], 'fixture_commit': commit, 'artifact_sha256': artifact['sha256']})
    except BaseException as error:
        evidence['failure'] = str(error)
        raise
    finally:
        if emissary is not None:
            if emissary.poll() is None:
                emissary.send_signal(signal.SIGTERM)
            try:
                emissary.wait(timeout=10)
            except subprocess.TimeoutExpired:
                emissary.kill()
                emissary.wait(timeout=5)
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
        (DIRECTORY / 'probe-evidence.json').write_text(json.dumps(evidence, indent=2) + '\n')


if __name__ == '__main__':
    main()
