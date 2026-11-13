#!/usr/bin/env python3
import argparse
import base64
import hashlib
import http.cookiejar
import http.server
import json
import os
import platform
import signal
import shutil
import socket
import ssl
import subprocess
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BASE = os.environ.get('ARTIFACTORY_ACCEPTANCE_HTTP_URL', 'http://127.0.0.1:8080')
GRPC_ENDPOINT = os.environ.get('ARTIFACTORY_ACCEPTANCE_GRPC_ENDPOINT', 'localhost:8443')
GIT_PORT = int(os.environ.get('ARTIFACTORY_ACCEPTANCE_GIT_PORT', '19443'))
GIT_ORIGIN = f'https://localhost:{GIT_PORT}'
PKI = Path(os.environ.get('ARTIFACTORY_PKI_DIR', str(ROOT / '.local/pki')))
FIXTURE_DATABASE_URL = None
RUN_ID = uuid.uuid4().hex[:12]
DIRECTORY = ROOT / '.local' / 'acceptance' / RUN_ID
DIRECTORY.mkdir(parents=True, mode=0o700)
EVIDENCE = {'run_id': RUN_ID, 'host': platform.platform(), 'checks': []}


def check(name, condition):
    if not condition:
        raise AssertionError(name)
    EVIDENCE['checks'].append(name)
    print('PASS', name, flush=True)


def command(arguments, **kwargs):
    return subprocess.check_output(arguments, stderr=subprocess.STDOUT, **kwargs).decode().strip()


def wait_for(function, timeout=90):
    deadline = time.monotonic() + timeout
    last_error = None
    while time.monotonic() < deadline:
        try:
            result = function()
            if result:
                return result
        except (OSError, urllib.error.URLError) as error:
            last_error = error
        time.sleep(0.3)
    raise TimeoutError(f'acceptance condition timed out: {last_error}')


COOKIE_JAR = http.cookiejar.CookieJar()
HTTP = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(COOKIE_JAR))


def request(path, body=None, method=None, authenticated=True, token=None):
    headers = {'Content-Type': 'application/json', 'x-artifactory-csrf': '1', 'Origin': os.environ['ARTIFACTORY_PUBLIC_URL']}
    if token:
        headers['Authorization'] = 'Bearer ' + token
    payload = None if body is None else json.dumps(body).encode()
    outgoing = urllib.request.Request(BASE + '/api/v1' + path, data=payload, method=method, headers=headers)
    opener = HTTP if authenticated else urllib.request.build_opener()
    try:
        with opener.open(outgoing, timeout=60) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        return error.code, json.load(error)


def api(path, body=None, method=None):
    if body is not None and method in (None,'POST') and path.startswith('/projects/') and path.endswith('/builds'):
        from legacy_fixture import historical_build
        return historical_build(path.split('/')[2],body,FIXTURE_DATABASE_URL)
    status, value = request(path, body, method)
    if status != 200:
        raise RuntimeError(f'{path}: {status}: {value}')
    return value


def create_fixture():
    repo = DIRECTORY / 'repos' / 'fixture.git'
    repo.mkdir(parents=True)
    command(['git', 'init', '-b', 'main', str(repo)])
    (repo / 'main.c').write_text('#include <stdio.h>\nint main(void) { puts("artifactory-native-ok"); return 0; }\n')
    host_os = {'Darwin': 'darwin', 'Linux': 'linux'}[platform.system()]
    arch = 'arm64' if platform.machine() in ('arm64', 'aarch64') else 'amd64'
    signing = host_os == 'darwin'
    signing_config = '\n    signing_profiles: [adhoc-test]' if signing else ''
    sign_steps = '''
      - name: sign
        phase: sign
        timeout_seconds: 180
        run: codesign --force --sign - dist/app
      - name: verify
        phase: verify
        timeout_seconds: 180
        run: |
          codesign --verify --strict --verbose=2 dist/app
          ./dist/app
          if [ "$ARTIFACTORY_PARAM_MODE" = missing ]; then rm dist/app; fi
          if [ "$ARTIFACTORY_PARAM_MODE" = escape ]; then ln -s /etc/passwd dist/escape; fi
''' if signing else '''
      - name: verify
        phase: verify
        timeout_seconds: 180
        run: |
          ./dist/app
          if [ "$ARTIFACTORY_PARAM_MODE" = missing ]; then rm dist/app; fi
          if [ "$ARTIFACTORY_PARAM_MODE" = escape ]; then ln -s /etc/passwd dist/escape; fi
'''
    (repo / 'artifactory.yaml').write_text(f'''version: 1
parameters: {{MODE: normal}}
release: {{enabled: true}}
targets:
  native:
    os: {host_os}
    arch: {arch}
    capabilities: [cc]{signing_config}
    timeout_seconds: 240
    steps:
      - name: compile
        timeout_seconds: 180
        run: |
          mkdir -p dist
          cc main.c -o dist/app
          echo 'native compilation complete'
          if [ "$ARTIFACTORY_PARAM_MODE" = fail ]; then exit 17; fi
          if [ "$ARTIFACTORY_PARAM_MODE" = resume ]; then sleep 18; fi
          if [ "$ARTIFACTORY_PARAM_MODE" = slow ]; then sleep 180 & child=$!; echo "$child" > child.pid; wait "$child"; fi
{sign_steps}
    artifacts: [dist/*]
  intentional-failure:
    os: {host_os}
    arch: {arch}
    timeout_seconds: 60
    steps:
      - name: deliberate failure
        timeout_seconds: 30
        run: exit 7
    artifacts: [dist/*]
''')
    command(['git', '-C', str(repo), '-c', 'user.name=Acceptance', '-c', 'user.email=acceptance@localhost', 'add', '.'])
    command(['git', '-C', str(repo), '-c', 'user.name=Acceptance', '-c', 'user.email=acceptance@localhost', 'commit', '-m', 'Native acceptance fixture'])
    commit = command(['git', '-C', str(repo), 'rev-parse', 'HEAD'])
    password = uuid.uuid4().hex
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
                self.send_header('WWW-Authenticate', 'Basic realm="acceptance"')
                self.send_header('Content-Length', '0')
                self.end_headers()
                return
            parsed = urllib.parse.urlsplit(self.path)
            environment = dict(os.environ, GIT_PROJECT_ROOT=str(repo.parent), GIT_HTTP_EXPORT_ALL='1', PATH_INFO=parsed.path, QUERY_STRING=parsed.query, REQUEST_METHOD=self.command, CONTENT_TYPE=self.headers.get('Content-Type', ''), CONTENT_LENGTH=self.headers.get('Content-Length', '0'), REMOTE_USER='fixture')
            result = subprocess.run(['git', 'http-backend'], input=self.rfile.read(int(environment['CONTENT_LENGTH'])), env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
            header_bytes, content = result.stdout.split(b'\r\n\r\n', 1)
            headers = [line.decode().split(': ', 1) for line in header_bytes.split(b'\r\n')]
            status = next((int(value.split()[0]) for key, value in headers if key.lower() == 'status'), 200)
            self.send_response(status)
            for key, value in headers:
                if key.lower() not in ('status', 'content-length'):
                    self.send_header(key, value)
            self.send_header('Content-Length', str(len(content)))
            self.end_headers()
            self.wfile.write(content)

    server = http.server.ThreadingHTTPServer(('127.0.0.1', GIT_PORT), GitHandler)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(PKI / 'server.pem', PKI / 'server.key')
    server.socket = context.wrap_socket(server.socket, server_side=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, password, commit, signing


def build(project, mode='normal', version=None):
    return api(f'/projects/{project}/builds', {'ref': 'main', 'parameters': {'MODE': mode}, 'targets': ['native'], 'release_version': version})['id']


def terminal(build_id, status):
    def complete():
        value = api('/builds/' + build_id)
        if value['status'] in ('succeeded', 'failed', 'canceled', 'lost'):
            if value['status'] != status:
                failure = {
                    'build_id': build_id,
                    'expected': status,
                    'actual': value['status'],
                    'error': value.get('error'),
                    'attempts': [
                        {'target': job.get('target'), 'status': attempt.get('status'), 'error': attempt.get('error')}
                        for job in value.get('jobs', [])
                        for attempt in job.get('attempts', [])
                    ],
                }
                EVIDENCE['unexpected_build'] = failure
                (DIRECTORY / 'evidence.json').write_text(json.dumps(EVIDENCE, indent=2) + '\n')
                raise AssertionError('unexpected build result: ' + json.dumps(failure))
            check(f'build {status}', value['status'] == status)
            return value
        return None
    return wait_for(complete, 120)


def stop_process(process):
    if process is not None:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
        process.wait(timeout=5)


def start_control(timeout=30):
    endpoint = urllib.parse.urlsplit(BASE)
    try:
        with socket.create_connection((endpoint.hostname, endpoint.port), timeout=1):
            raise RuntimeError(f'Port {endpoint.port} is already occupied; stop the manually started control plane first.')
    except ConnectionRefusedError:
        pass
    log_path = DIRECTORY / 'control.log'
    with log_path.open('ab') as log:
        process = subprocess.Popen([str(ROOT / 'target/debug/artifactory-control')], cwd=ROOT, env=dict(os.environ, GIT_SSL_CAINFO=str(PKI / 'ca.pem')), stdout=log, stderr=log)

    def ready():
        status = process.poll()
        if status is not None:
            details = log_path.read_text(errors='replace')[-8192:]
            raise RuntimeError(f'control plane exited with status {status}; see {log_path}\n{details}')
        with urllib.request.urlopen(BASE + '/healthz', timeout=2) as response:
            return response.status == 200

    try:
        wait_for(ready, timeout)
        return process
    except BaseException:
        stop_process(process)
        raise


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--keep-alive', action='store_true')
    options = parser.parse_args()
    server = control = emissary = emissary_log = None
    try:
        server, git_password, commit, signing = create_fixture()
        control = start_control()
        api('/auth/login', {'username': os.environ['ARTIFACTORY_BOOTSTRAP_USERNAME'], 'password': os.environ['ARTIFACTORY_BOOTSTRAP_PASSWORD']})
        credential = api('/credentials', {'name': 'acceptance-' + RUN_ID, 'kind': 'https', 'username': 'fixture', 'secret': git_password})['id']
        check('credential response redacts secret', git_password not in json.dumps(api('/credentials')))
        pool = '00000000-0000-0000-0000-000000000001'
        enrollment = api('/enrollment-tokens', {'pool_id': pool})['token']
        config_file = DIRECTORY / 'emissary.json'
        emissary_binary = DIRECTORY / 'bin with spaces' / 'artifactory emissary'
        emissary_binary.parent.mkdir()
        shutil.copy2(ROOT / 'target/debug/artifactory-emissary', emissary_binary)
        arguments = [str(emissary_binary), 'register', '--config', str(config_file), '--endpoint', GRPC_ENDPOINT, '--ca', str(PKI / 'ca.pem'), '--root', str(DIRECTORY / 'emissary-data'), '--name', 'acceptance-' + RUN_ID, '--allow-http-uploads']
        emissary_environment = {key: value for key, value in os.environ.items() if not key.startswith('ARTIFACTORY_') and key != 'DATABASE_URL'}
        environment = dict(emissary_environment, ARTIFACTORY_ENROLLMENT_TOKEN=enrollment)
        command(arguments, env=environment)
        reused = subprocess.run(arguments + ['--config', str(DIRECTORY / 'reused.json'), '--root', str(DIRECTORY / 'reused-data')], env=environment, capture_output=True)
        check('enrollment token single use', reused.returncode != 0 and not (DIRECTORY / 'reused.json').exists())
        config = json.loads(config_file.read_text())
        config['git_ca_file'] = str(PKI / 'ca.pem')
        config['signing_profiles'] = {'adhoc-test': {}} if signing else {}
        config_file.write_text(json.dumps(config))
        emissary_log = open(DIRECTORY / 'emissary.log', 'ab')
        emissary = subprocess.Popen([str(emissary_binary), 'run', '--config', str(config_file)], env=emissary_environment, stdout=emissary_log, stderr=emissary_log)
        node_id = config['emissary_id']

        def node_ready():
            return next((node for node in api('/nodes') if node['id'] == node_id and 'cc' in node['capabilities']), None)

        node = wait_for(node_ready, 90)
        settings = {key: node[key] for key in ('draining', 'revoked', 'slots', 'labels', 'signing_profiles', 'pool_id')}
        settings['signing_profiles'] = ['adhoc-test'] if signing else []
        api('/nodes/' + node_id, settings, 'PATCH')
        project_settings = {'name': 'Native acceptance ' + RUN_ID, 'repository_url': GIT_ORIGIN + '/fixture.git', 'credential_id': credential, 'pool_ids': [pool], 'signing_profiles': settings['signing_profiles']}
        project = api('/projects', project_settings)['id']
        success_id = build(project, version='acceptance-' + RUN_ID)
        success = terminal(success_id, 'succeeded')
        check('pinned Git commit and configuration', success['commit_sha'] == commit and 'native compilation complete' in success['config_yaml'])
        check('native stages verified', all(step['status'] == 'succeeded' for step in success['jobs'][0]['attempts'][-1]['steps']))
        wait_for(lambda: api('/builds/' + success_id)['publish_status'] == 'succeeded')
        release_id = api('/builds/' + success_id + '/publish', {})['id']
        check('publication retry idempotent', api('/builds/' + success_id + '/publish', {})['id'] == release_id)
        artifact = success['artifacts'][0]
        download = urllib.request.Request(BASE + '/api/v1/artifacts/' + artifact['id'] + '/download')
        with HTTP.open(download) as response:
            content = response.read()
            signed_url = response.url
        check('download hash and size match verified object', len(content) == artifact['size'] and hashlib.sha256(content).hexdigest() == artifact['sha256'])
        binary = DIRECTORY / 'downloaded-app'
        binary.write_bytes(content)
        binary.chmod(0o700)
        check('downloaded native program executes', command([str(binary)]) == 'artifactory-native-ok')
        if signing:
            command(['codesign', '--verify', '--strict', str(binary)])
            check('downloaded binary ad-hoc signature verified', True)
        status, _value = request('/artifacts/' + artifact['id'] + '/download', authenticated=False)
        check('download requires authentication', status == 401)
        try:
            urllib.request.urlopen(signed_url.split('?')[0])
            raise AssertionError('object store was anonymously readable')
        except urllib.error.HTTPError as error:
            check('object store is private', error.code in (401, 403))
        with HTTP.open(BASE + '/api/v1/builds/' + success_id + '/events', timeout=10) as response:
            found_log = False
            for _line in range(60):
                if response.readline().startswith(b'event: log'):
                    found_log = True
                    break
            check('SSE replays real build logs', found_log)
        for mode in ('missing', 'escape', 'fail'):
            failed = terminal(build(project, mode), 'failed')
            check(mode + ' cannot produce successful artifacts', not failed['artifacts'])
        failed_id = failed['id']
        job_id = failed['jobs'][0]['id']
        api('/jobs/' + job_id + '/retry', {})
        retried = terminal(failed_id, 'failed')
        check('explicit retry creates a distinct attempt', len(retried['jobs'][0]['attempts']) == 2)
        settings['draining'] = True
        api('/nodes/' + node_id, settings, 'PATCH')
        queued_id = build(project)
        wait_for(lambda: api('/builds/' + queued_id)['status'] == 'queued')
        api('/projects/' + project, dict(project_settings, repository_url=GIT_ORIGIN + '/not-the-source.git'), 'PATCH')
        settings['draining'] = False
        api('/nodes/' + node_id, settings, 'PATCH')
        terminal(queued_id, 'succeeded')
        check('queued assignment uses immutable source snapshot', True)
        api('/projects/' + project, project_settings, 'PATCH')
        partial_id = api('/projects/' + project + '/builds', {'ref': 'main', 'targets': ['native', 'intentional-failure'], 'release_version': 'partial-' + RUN_ID})['id']
        partial = terminal(partial_id, 'failed')
        check('partial target failure prevents automatic release', sorted(job['status'] for job in partial['jobs']) == ['failed', 'succeeded'] and partial['publish_status'] != 'succeeded')
        resumed_id = build(project, 'resume')
        running = wait_for(lambda: (value if value['status'] == 'running' else None) if (value := api('/builds/' + resumed_id)) else None)
        attempt_id = running['jobs'][0]['attempts'][-1]['id']
        control.kill()
        control.wait(timeout=10)
        time.sleep(2)
        control = start_control()
        resumed = terminal(resumed_id, 'succeeded')
        check('center crash resumes same running attempt', len(resumed['jobs'][0]['attempts']) == 1 and resumed['jobs'][0]['attempts'][0]['id'] == attempt_id)
        partition_id = build(project, 'slow')
        marker = wait_for(lambda: next(iter((DIRECTORY / 'emissary-data/work').glob('*/source/child.pid')), None))
        partition_child = int(marker.read_text())
        control.kill()
        control.wait(timeout=10)

        def partition_child_exited():
            try:
                os.kill(partition_child, 0)
                return False
            except ProcessLookupError:
                return True

        wait_for(partition_child_exited, 105)
        check('network partition enforces local monotonic lease and kills child tree', True)
        control = start_control()
        partition = terminal(partition_id, 'lost')
        check('expired running attempt is never automatically rerun', len(partition['jobs'][0]['attempts']) == 1)
        for failure in ('cancel', 'emissary-crash'):
            slow_id = build(project, 'slow')
            marker = wait_for(lambda: next(iter((DIRECTORY / 'emissary-data/work').glob('*/source/child.pid')), None))
            child_pid = int(marker.read_text())
            if failure == 'cancel':
                api('/builds/' + slow_id + '/cancel', {})
                terminal(slow_id, 'canceled')
            else:
                emissary.kill()
                emissary.wait(timeout=10)

            def child_exited():
                try:
                    os.kill(child_pid, 0)
                    return False
                except ProcessLookupError:
                    return True

            wait_for(child_exited, 10)
            check(failure + ' terminates child process tree', True)
            if failure == 'emissary-crash':
                terminal(slow_id, 'lost')
        emissary = subprocess.Popen([str(emissary_binary), 'run', '--config', str(config_file)], env=emissary_environment, stdout=emissary_log, stderr=emissary_log)
        wait_for(node_ready, 90)
        collision = terminal(build(project, version='acceptance-' + RUN_ID), 'succeeded')
        wait_for(lambda: api('/builds/' + collision['id'])['publish_status'] == 'failed')
        check('same-name release conflicts without overwriting', api('/releases/' + release_id)['build_id'] == success_id)
        EVIDENCE.update({'project_id': project, 'node_id': node_id, 'build_id': success_id, 'partial_build_id': partial_id, 'failed_build_id': failed_id, 'collision_build_id': collision['id'], 'release_id': release_id, 'emissary_config': str(config_file), 'commit': commit, 'signing': 'macOS ad-hoc only' if signing else 'not tested', 'diagnostics': node['diagnostics']})
        (ROOT / '.local/acceptance/latest.json').write_text(json.dumps(EVIDENCE, indent=2))
        (DIRECTORY / 'evidence.json').write_text(json.dumps(EVIDENCE, indent=2))
        print('ACCEPTANCE READY', json.dumps({'checks': len(EVIDENCE['checks']), 'run_id': RUN_ID}), flush=True)
        if options.keep_alive:
            while True:
                time.sleep(1)
    finally:
        for process in (emissary, control):
            stop_process(process)
        if emissary_log is not None:
            emissary_log.close()
        if server is not None:
            server.shutdown()
            server.server_close()


if __name__ == '__main__':
    def stop_services(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGINT, stop_services)
    signal.signal(signal.SIGTERM, stop_services)
    try:
        main()
    except KeyboardInterrupt:
        print('Acceptance services stopped.', flush=True)
