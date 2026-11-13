#!/usr/bin/env python3
"""Exercise the production Compose stack with an isolated CA and disposable volumes."""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import tarfile
import time
import uuid


ROOT = Path(__file__).resolve().parents[1]
LABEL = 'artifactory.acceptance.run'


def execute(arguments, **kwargs):
    return subprocess.run(arguments, check=True, timeout=kwargs.pop('timeout', 60), **kwargs)


def capture(arguments, **kwargs):
    return execute(arguments, capture_output=True, text=True, **kwargs).stdout.strip()


def free_ports(count):
    listeners = []
    try:
        for _ in range(count):
            listener = socket.socket()
            listeners.append(listener)
            listener.bind(('127.0.0.1', 0))
        return [listener.getsockname()[1] for listener in listeners]
    finally:
        for listener in listeners:
            listener.close()


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image-prefix', default='', help='optional pull-through registry, such as docker.m.daocloud.io')
    parser.add_argument('--control-plane-image', '--control-image', dest='control_plane_image', help='reuse a control plane image labelled with the exact current Git revision')
    parser.add_argument('--web-image', help='reuse an image labelled with the exact current Git revision')
    parser.add_argument('--agent-worker-image', help='reuse an Agent worker image labelled with the exact current Git revision')
    parser.add_argument('--probe-image', help='reuse a Linux test-tooling image with python3, Git, cc and OpenSSL')
    options = parser.parse_args()
    if os.getuid() == 0:
        parser.error('run as a non-root deployment account with Docker access')
    source_sha = capture(['git', 'rev-parse', 'HEAD'], cwd=ROOT)
    run_id = uuid.uuid4().hex[:12]
    project = 'artifactory-tls-' + run_id
    directory = ROOT / '.local/compose-tls' / run_id
    directory.mkdir(parents=True, mode=0o700)
    snapshot = directory / 'source'
    snapshot.mkdir()
    archive = execute(['git', 'archive', source_sha], cwd=ROOT, capture_output=True).stdout
    with tarfile.open(fileobj=io.BytesIO(archive)) as source:
        source.extractall(snapshot, filter='data')
    probe = directory / 'probe'
    probe.mkdir()
    shutil.copyfile(ROOT / 'scripts/compose_probe.py', probe / 'probe.py')
    shutil.copyfile(ROOT / 'scripts/legacy_fixture.py', probe / 'legacy_fixture.py')
    shutil.copyfile(Path(__file__), directory / 'executed-controller.py')
    evidence = {'kind': 'isolated-compose-tls', 'run_id': run_id, 'project': project, 'source_sha': source_sha, 'controller_sha256': hashlib.sha256((directory / 'executed-controller.py').read_bytes()).hexdigest(), 'probe_sha256': hashlib.sha256((probe / 'probe.py').read_bytes()).hexdigest(), 'image_prefix': options.image_prefix, 'checks': []}
    print('COMPOSE RUN', run_id, str(directory), flush=True)

    def check(name, condition):
        if not condition:
            raise AssertionError(name)
        evidence['checks'].append(name)
        print('PASS', name, flush=True)

    def registry(image):
        if not options.image_prefix:
            return image
        return options.image_prefix.rstrip('/') + '/' + ('' if '/' in image else 'library/') + image

    def build(name, context, dockerfile, image=None, platform=None):
        digest = hashlib.sha256(dockerfile.encode()).hexdigest()
        if image:
            details = json.loads(capture(['docker', 'image', 'inspect', image]))[0]
            labels = details['Config'].get('Labels') or {}
            check(name + ' reused image matches current source and Dockerfile', labels.get('org.opencontainers.image.revision') == source_sha and labels.get('artifactory.dockerfile.sha256') == digest)
            return image
        tag = 'artifactory-' + name + ':compose-' + run_id
        modified = re.sub(r'(?m)^FROM ([^ \n]+)', lambda match: 'FROM ' + registry(match.group(1)), dockerfile) if name != 'probe' else dockerfile
        print('BUILD', name, flush=True)
        arguments = ['docker', 'build', '--progress=plain', '--label', 'org.opencontainers.image.revision=' + source_sha, '--label', 'artifactory.dockerfile.sha256=' + digest]
        if platform:
            arguments += ['--platform', platform]
        with (directory / (name + '-build.log')).open('w') as log:
            execute(arguments + ['-f', '-', '-t', tag, str(context)], input=modified, text=True, stdout=log, stderr=subprocess.STDOUT, timeout=1200)
        print('BUILT', name, flush=True)
        return tag

    compose_file = directory / 'compose.json'
    compose = ['docker', 'compose', '--project-name', project, '--project-directory', str(snapshot), '-f', str(compose_file)]
    started = False
    worker = None
    worker_name = project + '-probe'
    resources = {}
    baseline = {}
    for name in ('artifactory-postgres-test', 'artifactory-s3-test'):
        result = subprocess.run(['docker', 'inspect', name], capture_output=True, text=True, timeout=15)
        if result.returncode == 0:
            state = json.loads(result.stdout)[0]
            baseline[name] = (state['Id'], state['State']['StartedAt'], state['RestartCount'])

    def assert_owned(resource, name):
        result = subprocess.run(['docker', resource, 'inspect', name], capture_output=True, text=True, timeout=15)
        if result.returncode:
            if 'No such' in result.stderr:
                return False
            raise RuntimeError('cannot inspect acceptance resource: ' + name)
        value = json.loads(result.stdout)[0]
        labels = value['Config']['Labels'] if resource == 'container' else value.get('Labels')
        if (labels or {}).get(LABEL) != run_id:
            raise RuntimeError('refusing to remove a resource without its run ownership label: ' + name)
        return True

    try:
        for kind in ('container', 'volume', 'network'):
            check('unique Compose ' + kind + ' namespace is unused', not capture(['docker', kind, 'ls', '-q'] + (['-a'] if kind == 'container' else []) + ['--filter', 'label=com.docker.compose.project=' + project]))
        control_image = build('control-plane', snapshot, (snapshot / 'Dockerfile').read_text(), options.control_plane_image)
        web_image = build('web', snapshot / 'web', (snapshot / 'web/Dockerfile').read_text(), options.web_image)
        agent_worker_image = build('agent-worker', snapshot, (snapshot / 'packages/agent-worker/Dockerfile').read_text(), options.agent_worker_image)
        architecture = capture(['docker', 'image', 'inspect', control_image, '--format', '{{.Architecture}}'])
        check('probe image uses a supported Emissary architecture', architecture in ('amd64', 'arm64'))
        probe_dockerfile = f'FROM {control_image}\nUSER 0\nRUN apt-get update && apt-get install -y --no-install-recommends python3 gcc libc6-dev postgresql-client && rm -rf /var/lib/apt/lists/*\nUSER 10001:10001\n'
        probe_image = options.probe_image or build('probe', snapshot, probe_dockerfile, platform='linux/' + architecture)
        architecture = capture(['docker', 'image', 'inspect', probe_image, '--format', '{{.Architecture}}'])
        check('test tooling supports the compiled Emissary architecture', architecture in ('amd64', 'arm64'))
        emissary_image = build('emissary', snapshot, (snapshot / 'crates/emissary/Dockerfile').read_text(), platform='linux/' + architecture)
        extract = capture(['docker', 'create', '--label', 'artifactory.acceptance=' + run_id, emissary_image])
        try:
            execute(['docker', 'cp', extract + ':/usr/local/bin/artifactory-emissary', str(probe / 'artifactory-emissary')])
        finally:
            execute(['docker', 'rm', extract])
        console = 'console-' + run_id + '.test'
        objects = 'objects-' + run_id + '.test'
        environment = {key: value for key, value in os.environ.items() if not key.startswith(('ARTIFACTORY_', 'COMPOSE_')) and key != 'DATABASE_URL'}
        execute(['sh', str(snapshot / 'scripts/init-deploy.sh')], cwd=snapshot, env=dict(environment, ARTIFACTORY_DOMAIN=console, ARTIFACTORY_S3_DOMAIN=objects, ARTIFACTORY_GRPC_HOST='control-plane'), capture_output=True, text=True)
        settings = dict(line.split('=', 1) for line in (snapshot / '.deploy/env').read_text().splitlines())
        shutil.copyfile(snapshot / '.deploy/pki/ca.pem', probe / 'grpc-ca.pem')
        with (directory / 'certificate-generation.log').open('w') as log:
            execute(['openssl', 'req', '-newkey', 'rsa:2048', '-nodes', '-keyout', str(probe / 'git.key'), '-out', str(probe / 'git.csr'), '-subj', '/CN=probe'], stdout=log, stderr=log)
            (probe / 'git.ext').write_text('subjectAltName=DNS:probe\nextendedKeyUsage=serverAuth\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\n')
            execute(['openssl', 'x509', '-req', '-in', str(probe / 'git.csr'), '-CA', str(snapshot / '.deploy/pki/ca.pem'), '-CAkey', str(snapshot / '.deploy/pki/ca.key'), '-CAcreateserial', '-out', str(probe / 'git.pem'), '-days', '1', '-sha256', '-extfile', str(probe / 'git.ext')], stdout=log, stderr=log)
        caddy = snapshot / 'deploy/Caddyfile'
        source_caddy = caddy.read_text()
        for name in ('ARTIFACTORY_DOMAIN', 'ARTIFACTORY_S3_DOMAIN'):
            token = '{$' + name + '} {'
            if source_caddy.count(token) != 1:
                raise RuntimeError('production Caddy host blocks changed; review the acceptance override')
            source_caddy = source_caddy.replace(token, token + '\n    tls internal')
        caddy.write_text('{\n    admin off\n    auto_https disable_redirects\n    skip_install_trust\n}\n\n' + source_caddy)
        manifest = json.loads(capture(['docker', 'compose', '--project-name', project, '--project-directory', str(snapshot), '--env-file', str(snapshot / '.deploy/env'), '-f', str(snapshot / 'compose.yaml'), 'config', '--format', 'json'], env=environment))
        check('production Compose includes independent telemetry', set(manifest['services']) == {'postgres', 's3', 'control-plane', 'web', 'gateway','agent-worker','telemetry'})
        manifest['services']['control-plane']['image'] = control_image
        manifest['services']['telemetry']['image'] = control_image
        manifest['services']['web']['image'] = web_image
        manifest['services']['agent-worker']['image'] = agent_worker_image
        for name in ('control-plane', 'web', 'agent-worker','telemetry'):
            manifest['services'][name].pop('build', None)
        for name in ('postgres', 's3', 'gateway'):
            service = manifest['services'][name]
            service['image'] = registry(service['image'])
        http_port, https_port, grpc_port, telemetry_port = free_ports(4)
        manifest['services']['gateway']['ports'] = [{'target': 80, 'published': str(http_port), 'host_ip': '127.0.0.1', 'protocol': 'tcp'}, {'target': 443, 'published': str(https_port), 'host_ip': '127.0.0.1', 'protocol': 'tcp'}]
        manifest['services']['control-plane']['ports'] = [{'target': 8443, 'published': str(grpc_port), 'host_ip': '127.0.0.1', 'protocol': 'tcp'}]
        manifest['services']['telemetry']['ports'] = [{'target':8444,'published':str(telemetry_port),'host_ip':'127.0.0.1','protocol':'tcp'}]
        manifest['services']['gateway']['networks'] = {'default': {'aliases': [console, objects]}}
        manifest['services']['control-plane']['environment'].update({'SSL_CERT_FILE': '/run/acceptance/https-ca.pem', 'GIT_SSL_CAINFO': '/run/artifactory-pki/ca.pem'})
        manifest['services']['control-plane']['volumes'].append({'type': 'bind', 'source': str(probe / 'https-ca.pem'), 'target': '/run/acceptance/https-ca.pem', 'read_only': True, 'bind': {'create_host_path': False}})
        for kind in ('services', 'volumes', 'networks'):
            for value in manifest.get(kind, {}).values():
                value.setdefault('labels', {})[LABEL] = run_id
        resources = {kind: [value['name'] for value in manifest.get(kind, {}).values()] for kind in ('volumes', 'networks')}
        check('volumes and networks belong exclusively to this project', all(value['name'].startswith(project + '_') and not value.get('external') for kind in ('volumes', 'networks') for value in manifest.get(kind, {}).values()))
        check('all host mounts belong to the private acceptance directory', all(Path(volume['source']).resolve().is_relative_to(directory.resolve()) for service in manifest['services'].values() for volume in service.get('volumes', []) if volume['type'] == 'bind'))
        check('production insecure-development bypass stays disabled', manifest['services']['control-plane']['environment'].get('ARTIFACTORY_INSECURE_DEV') != 'true')
        evidence.update({'images': {}, 'image_platforms': {}, 'ports': {'https': https_port, 'http': http_port, 'grpc': grpc_port}})
        for name, image in [('control-plane', control_image), ('web', web_image), ('agent-worker', agent_worker_image), ('probe', probe_image), *[(name, manifest['services'][name]['image']) for name in ('postgres', 's3', 'gateway')]]:
            # Pull infrastructure images through the chosen registry without changing global Docker settings.
            inspected = subprocess.run(['docker', 'image', 'inspect', image], capture_output=True, text=True, timeout=15)
            if inspected.returncode:
                with (directory / (name + '-pull.log')).open('w') as log:
                    execute(['docker', 'pull', image], stdout=log, stderr=subprocess.STDOUT, timeout=300)
            details = json.loads(capture(['docker', 'image', 'inspect', image]))[0]
            platform = details['Os'] + '/' + details['Architecture'] + (('/' + details['Variant']) if details.get('Variant') else '')
            evidence['images'][name] = details['Id']
            evidence['image_platforms'][name] = platform
            if name in manifest['services']:
                manifest['services'][name]['platform'] = platform
        manifest['services']['telemetry']['platform']=manifest['services']['control-plane']['platform']
        compose_file.write_text(json.dumps(manifest, indent=2) + '\n')
        started = True
        with (directory / 'startup.log').open('w') as log:
            execute(compose + ['up', '-d', '--no-deps', '--no-build', '--pull', 'never', 'postgres', 's3', 'web', 'gateway'], stdout=log, stderr=log, timeout=180)
            deadline = time.monotonic() + 60
            while True:
                result = subprocess.run(compose + ['exec', '-T', 'gateway', 'cat', '/data/caddy/pki/authorities/local/root.crt'], capture_output=True, timeout=15)
                if result.returncode == 0 and b'BEGIN CERTIFICATE' in result.stdout:
                    (probe / 'https-ca.pem').write_bytes(result.stdout)
                    break
                if time.monotonic() > deadline:
                    raise TimeoutError('Caddy did not create the isolated HTTPS CA')
                time.sleep(1)
            execute(compose + ['up', '-d', '--no-build', '--pull', 'never', 'control-plane', 'agent-worker','telemetry'], stdout=log, stderr=log, timeout=180)
        details = json.loads(capture(['docker', 'inspect', capture(compose + ['ps', '-q', 'control-plane'])]))[0]
        check('control runs non-root with a read-only root filesystem and no capabilities', details['Config']['User'].split(':')[0] != '0' and details['HostConfig']['ReadonlyRootfs'] and 'ALL' in (details['HostConfig']['CapDrop'] or []) and 'no-new-privileges:true' in (details['HostConfig']['SecurityOpt'] or []))
        for service in ('postgres', 's3', 'web', 'gateway', 'control-plane', 'agent-worker','telemetry'):
            details = json.loads(capture(['docker', 'inspect', capture(compose + ['ps', '-aq', service])]))[0]
            bindings = details['HostConfig'].get('PortBindings') or {}
            check(service + ' exposes no non-loopback host ports', all(binding['HostIp'] == '127.0.0.1' for values in bindings.values() for binding in values))
        worker_environment = dict(environment, ARTIFACTORY_BOOTSTRAP_PASSWORD=settings['ARTIFACTORY_BOOTSTRAP_PASSWORD'], ARTIFACTORY_COMPOSE_CONSOLE_URL='https://' + console, ARTIFACTORY_COMPOSE_S3_URL='https://' + objects, SSL_CERT_FILE='/acceptance/https-ca.pem', DATABASE_URL=manifest['services']['control-plane']['environment']['DATABASE_URL'])
        arguments = ['docker', 'run', '--init', '--platform', evidence['image_platforms']['probe'], '--name', worker_name, '--label', LABEL + '=' + run_id, '--network', manifest['networks']['default']['name'], '--network-alias', 'probe', '--user', f'{os.getuid()}:{os.getgid()}', '--read-only', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges:true', '--tmpfs', '/tmp:mode=1777', '--mount', 'type=bind,src=' + str(probe) + ',dst=/acceptance', '--workdir', '/acceptance']
        for name in ('ARTIFACTORY_BOOTSTRAP_PASSWORD', 'ARTIFACTORY_COMPOSE_CONSOLE_URL', 'ARTIFACTORY_COMPOSE_S3_URL', 'SSL_CERT_FILE', 'DATABASE_URL'):
            arguments += ['--env', name]
        with (directory / 'probe.log').open('w') as log:
            worker = subprocess.Popen(arguments + ['--entrypoint', 'python3', probe_image, '/acceptance/probe.py'], env=worker_environment, stdout=log, stderr=log)
            restarted = False
            storage_paused = storage_resumed = False
            deadline = time.monotonic() + 360
            while worker.poll() is None:
                if (probe / 'ready-for-restart').is_file() and not restarted:
                    execute(compose + ['restart', 'control-plane'], stdout=log, stderr=log, timeout=60)
                    (probe / 'control-plane-restarted').touch()
                    restarted = True
                if (probe / 'pause-log-storage').is_file() and not storage_paused:
                    storage = capture(compose + ['ps', '-aq', 's3'])
                    assert_owned('container', storage)
                    execute(compose + ['pause', 's3'], stdout=log, stderr=log, timeout=30)
                    check('log fault pauses only this run-owned S3 container', json.loads(capture(['docker', 'inspect', storage]))[0]['State']['Paused'])
                    (probe / 'log-storage-paused').touch()
                    storage_paused = True
                if (probe / 'resume-log-storage').is_file() and storage_paused and not storage_resumed:
                    storage = capture(compose + ['ps', '-aq', 's3'])
                    assert_owned('container', storage)
                    execute(compose + ['unpause', 's3'], stdout=log, stderr=log, timeout=30)
                    check('run-owned S3 resumes after the real log-buffer fault', not json.loads(capture(['docker', 'inspect', storage]))[0]['State']['Paused'])
                    (probe / 'log-storage-resumed').touch()
                    storage_resumed = True
                if time.monotonic() > deadline:
                    raise TimeoutError('Compose traffic probe exceeded six minutes')
                time.sleep(1)
        evidence['probe'] = json.loads((probe / 'probe-evidence.json').read_text()) if (probe / 'probe-evidence.json').is_file() else {}
        check('full HTTPS / mTLS / S3 / SSE / restart / log-buffer traffic probe passed', worker.returncode == 0 and 'failure' not in evidence['probe'] and restarted and storage_resumed)
        fault = evidence['probe']['log_buffer_fault']
        attempt = str(uuid.UUID(fault['attempt_id']))
        query = f"SELECT coalesce(json_agg(json_build_object('sequence',sequence,'size',size,'sha256',sha256) ORDER BY sequence),'[]'::json) FROM log_chunks WHERE attempt_id='{attempt}'"
        chunks = json.loads(capture(compose + ['exec', '-T', 'postgres', 'psql', '-U', 'artifactory', '-d', 'artifactory', '-tA', '-c', query]))
        check('PostgreSQL confirms consecutive delivered log sequences after overflow', [chunk['sequence'] for chunk in chunks] == list(range(1, len(chunks) + 1)))
        expected = fault['retained_chunks']
        check('PostgreSQL log sizes and hashes match the retained unacknowledged chunks', chunks[expected[0]['sequence'] - 1:] == expected)
    except BaseException as error:
        evidence['failure'] = str(error)
        raise
    finally:
        cleanup_errors = []
        if started:
            try:
                storage = capture(compose + ['ps', '-aq', 's3'])
                if storage and assert_owned('container', storage) and json.loads(capture(['docker', 'inspect', storage]))[0]['State']['Paused']:
                    execute(compose + ['unpause', 's3'], capture_output=True, timeout=30)
            except Exception as error:
                cleanup_errors.append('resume owned storage before teardown: ' + str(error))
            try:
                with (directory / 'services.log').open('w') as log:
                    execute(compose + ['logs', '--no-color'], stdout=log, stderr=log, timeout=30)
            except Exception as error:
                cleanup_errors.append('service diagnostics: ' + str(error))
            try:
                if assert_owned('container', worker_name):
                    execute(['docker', 'rm', '-f', worker_name], capture_output=True)
                if worker is not None:
                    worker.wait(timeout=15)
                containers = capture(compose + ['ps', '-aq']).splitlines()
                for name in containers:
                    assert_owned('container', name)
                for kind, names in resources.items():
                    for name in names:
                        assert_owned('volume' if kind == 'volumes' else 'network', name)
                with (directory / 'cleanup.log').open('w') as log:
                    execute(compose + ['down', '--volumes', '--timeout', '10'], stdout=log, stderr=log, timeout=90)
                for kind in ('container', 'volume', 'network'):
                    check('owned Compose ' + kind + ' resources were removed', not capture(['docker', kind, 'ls', '-q'] + (['-a'] if kind == 'container' else []) + ['--filter', 'label=' + LABEL + '=' + run_id]))
            except Exception as error:
                cleanup_errors.append(str(error))
        for name, previous in baseline.items():
            try:
                value = json.loads(capture(['docker', 'inspect', name]))[0]
                check('existing ' + name + ' was preserved', previous == (value['Id'], value['State']['StartedAt'], value['RestartCount']))
            except Exception as error:
                cleanup_errors.append(str(error))
        evidence['cleanup_errors'] = cleanup_errors
        if cleanup_errors and 'failure' not in evidence:
            evidence['failure'] = 'acceptance resource cleanup failed'
        (directory / 'evidence.json').write_text(json.dumps(evidence, indent=2) + '\n')
        print('COMPOSE EVIDENCE', str(directory / 'evidence.json'), flush=True)
    if 'failure' in evidence:
        raise RuntimeError(evidence['failure'])
    print('COMPOSE TLS ACCEPTANCE PASSED', json.dumps({'run_id': run_id, 'checks': len(evidence['checks']), 'probe_checks': len(evidence['probe']['checks'])}), flush=True)


if __name__ == '__main__':
    main()
