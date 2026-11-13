#!/usr/bin/env python3
"""Restore a quiescent local acceptance environment into fresh PostgreSQL and S3 volumes."""

import argparse
import hashlib
import json
import os
import re
import shutil
import socket
import subprocess
import urllib.parse
import urllib.request
from pathlib import Path

import acceptance


def execute(arguments, **kwargs):
    return subprocess.run(arguments, check=True, timeout=300, **kwargs)


def query(container, sql):
    result = execute(['docker', 'exec', container, 'psql', '-XAt', '-v', 'ON_ERROR_STOP=1', '-U', 'artifactory', '-d', 'artifactory', '-c', sql], capture_output=True, text=True)
    return json.loads(result.stdout)


def database_inventory(container):
    tables = query(container, "SELECT json_agg(tablename ORDER BY tablename) FROM pg_tables WHERE schemaname='public'")
    inventory = {}
    for table in tables:
        if not re.fullmatch(r'[a-zA-Z_][a-zA-Z_0-9]*', table):
            raise RuntimeError('unexpected table identifier')
        rows = query(container, f'SELECT COALESCE(json_agg(to_jsonb(t)),\'[]\'::json) FROM public."{table}" t')
        canonical = sorted(json.dumps(row, sort_keys=True, separators=(',', ':')) for row in rows)
        inventory[table] = {'rows': len(rows), 'sha256': hashlib.sha256('\n'.join(canonical).encode()).hexdigest()}
    sequences = query(container, "SELECT COALESCE(json_agg(sequencename ORDER BY sequencename),'[]'::json) FROM pg_sequences WHERE schemaname='public'")
    inventory['sequences'] = {}
    for sequence in sequences:
        if not re.fullmatch(r'[a-zA-Z_][a-zA-Z_0-9]*', sequence):
            raise RuntimeError('unexpected sequence identifier')
        inventory['sequences'][sequence] = query(container, f'SELECT row_to_json(t) FROM (SELECT last_value,is_called FROM public."{sequence}") t')
    return inventory


def object_environment(source, endpoint):
    return dict(source, RCLONE_CONFIG_ARTIFACTORY_RECOVERY_TYPE='s3', RCLONE_CONFIG_ARTIFACTORY_RECOVERY_PROVIDER='Other', RCLONE_CONFIG_ARTIFACTORY_RECOVERY_ACCESS_KEY_ID=source['ARTIFACTORY_S3_ACCESS_KEY'], RCLONE_CONFIG_ARTIFACTORY_RECOVERY_SECRET_ACCESS_KEY=source['ARTIFACTORY_S3_SECRET_KEY'], RCLONE_CONFIG_ARTIFACTORY_RECOVERY_ENDPOINT=endpoint, RCLONE_CONFIG_ARTIFACTORY_RECOVERY_REGION=source['ARTIFACTORY_S3_REGION'], RCLONE_CONFIG_ARTIFACTORY_RECOVERY_FORCE_PATH_STYLE='true')


def objects(arguments, environment, capture=False):
    return execute(['rclone', '--config', os.devnull] + arguments, env=environment, capture_output=capture, text=True)


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


def load_environment(path):
    # No inherited platform setting may silently substitute for a missing backup.
    clean = {key: value for key, value in os.environ.items() if not key.startswith(('ARTIFACTORY_', 'RCLONE_CONFIG_ARTIFACTORY_RECOVERY_')) and key != 'DATABASE_URL'}
    raw = subprocess.check_output(['bash', '-e', '-c', 'set -a; . "$1"; env -0', 'recovery-env', str(path)], env=clean, cwd=acceptance.ROOT)
    environment = dict(item.decode().split('=', 1) for item in raw.split(b'\0') if item and b'=' in item)
    for key in ('DATABASE_URL', 'ARTIFACTORY_MASTER_KEY', 'ARTIFACTORY_S3_ACCESS_KEY', 'ARTIFACTORY_S3_SECRET_KEY', 'ARTIFACTORY_S3_REGION', 'ARTIFACTORY_S3_BUCKET', 'ARTIFACTORY_S3_ENDPOINT', 'ARTIFACTORY_BOOTSTRAP_USERNAME', 'ARTIFACTORY_BOOTSTRAP_PASSWORD'):
        if not environment.get(key):
            raise RuntimeError('private environment is missing ' + key)
    return environment


def copy_private(source, destination):
    paths = [source] + (list(source.rglob('*')) if source.is_dir() and not source.is_symlink() else [])
    if any(path.is_symlink() or not (path.is_file() or path.is_dir()) for path in paths):
        raise RuntimeError('private backup source must contain only regular files and directories')
    if source.is_dir():
        shutil.copytree(source, destination)
        for path in (destination, *destination.rglob('*')):
            path.chmod(0o700 if path.is_dir() else 0o600)
    else:
        shutil.copyfile(source, destination)
        destination.chmod(0o600)


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--ssh-evidence', type=Path, default=acceptance.ROOT / '.local/acceptance/latest-ssh.json')
    parser.add_argument('--env-file', type=Path, default=acceptance.ROOT / '.local/env')
    parser.add_argument('--postgres-container', default='artifactory-timescaledb-test')
    parser.add_argument('--s3-container', default='artifactory-s3-test')
    parser.add_argument('--s3-config', type=Path, default=acceptance.ROOT / '.local/s3.json')
    parser.add_argument('--sudo-sshd', action='store_true', help='run only the isolated SSH fixture with passwordless sudo on a Linux CI runner')
    options = parser.parse_args()
    if os.environ.get('ARTIFACTORY_INSECURE_DEV') != 'true':
        parser.error('use only the explicit local development acceptance environment')
    if options.env_file.is_symlink():
        parser.error('the local environment must be a regular private file')
    source = load_environment(options.env_file)
    database = urllib.parse.urlsplit(source['DATABASE_URL'])
    custom = (options.postgres_container,options.s3_container) != ('artifactory-timescaledb-test','artifactory-s3-test')
    expected_pg,expected_s3 = 15432,19000
    if custom:
        containers=[json.loads(acceptance.command(['docker','inspect',name]))[0] for name in (options.postgres_container,options.s3_container)]
        owners=[(c['Config'].get('Labels') or {}).get('artifactory.acceptance.run') for c in containers]
        if not owners[0] or owners[0] != owners[1]:
            parser.error('custom recovery sources must share an explicit acceptance ownership label')
        ports=[containers[0]['NetworkSettings']['Ports']['5432/tcp'],containers[1]['NetworkSettings']['Ports']['8333/tcp']]
        if any(len(p)!=1 or p[0]['HostIp']!='127.0.0.1' for p in ports):
            parser.error('custom acceptance databases and object stores must bind only loopback')
        expected_pg,expected_s3=[int(p[0]['HostPort']) for p in ports]
    if (database.hostname, database.port, database.path, database.username) != ('127.0.0.1', expected_pg, '/artifactory', 'artifactory') or source['ARTIFACTORY_S3_ENDPOINT'] != f'http://127.0.0.1:{expected_s3}' or database.query or source.get('ARTIFACTORY_HTTP_BIND') != '127.0.0.1:8080' or source.get('ARTIFACTORY_GRPC_BIND') != '127.0.0.1:8443':
        parser.error('this drill is restricted to the repository local PostgreSQL / S3 test containers')
    for port in (8080, 8443, 19443):
        try:
            with socket.create_connection(('127.0.0.1', port), timeout=1):
                parser.error('stop the local acceptance harness before taking a consistent backup')
        except ConnectionRefusedError:
            pass
    ssh = json.loads(options.ssh_evidence.read_text())
    if ssh.get('kind') != 'ssh-git':
        parser.error('a successful SSH acceptance fixture is required')
    ssh_config = Path(ssh['ssh_config']).resolve()
    if not ssh_config.is_relative_to((acceptance.ROOT / '.local/acceptance').resolve()) or not ssh_config.is_file():
        parser.error('the SSH configuration must belong to a retained local acceptance fixture')
    for field in ('project_id', 'build_id', 'release_id'):
        if not re.fullmatch(r'[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}', ssh[field]):
            parser.error('invalid fixture identifier')
    ssh_settings = ssh_config.read_text()
    ssh_port = re.search(r'^Port (\d+)$', ssh_settings, re.MULTILINE)
    if not ssh_port or not re.search(r'^ListenAddress 127\.0\.0\.1$', ssh_settings, re.MULTILINE):
        parser.error('the SSH fixture must listen only on loopback')
    ssh_port = int(ssh_port.group(1))

    directory = acceptance.DIRECTORY
    backup = directory / 'backup'
    restored = directory / 'restored'
    backup.mkdir(mode=0o700)
    restored.mkdir(mode=0o700)
    created_containers = []
    created_volumes = []
    control = emissary = ssh_server = None
    handles = []
    evidence = directory / 'evidence.json'
    cleanup_errors = []
    failure = None

    def service(kind, image, arguments, data_path, command=()):
        name = 'artifactory-recovery-' + acceptance.RUN_ID + '-' + kind
        volume = name + '-data'
        for resource, identifier in (('container', name), ('volume', volume)):
            if subprocess.run(['docker', resource, 'inspect', identifier], capture_output=True).returncode == 0:
                raise RuntimeError('refusing to reuse an existing recovery ' + resource)
        created_volumes.append(volume)
        execute(['docker', 'volume', 'create', '--label', 'artifactory.acceptance.run=' + acceptance.RUN_ID, volume], stdout=subprocess.DEVNULL)
        # Track before run: Docker can create the container but fail to start it.
        created_containers.append(name)
        execute(['docker', 'run', '-d', '--pull=never', '--name', name, '--label', 'artifactory.acceptance.run=' + acceptance.RUN_ID, '-v', volume + ':' + data_path] + arguments + [image] + list(command), capture_output=True, text=True)
        return name

    def remove_owned(resource, name):
        result = subprocess.run(['docker', resource, 'inspect', name], capture_output=True, text=True, timeout=15)
        if result.returncode != 0:
            if 'No such' in result.stderr:
                return
            raise RuntimeError('cannot inspect recovery ' + resource + ': ' + name)
        inspected = json.loads(result.stdout)[0]
        labels = inspected['Config']['Labels'] if resource == 'container' else inspected['Labels']
        if (labels or {}).get('artifactory.acceptance.run') != acceptance.RUN_ID:
            raise RuntimeError('refusing to remove recovery resource without its ownership label: ' + name)
        execute(['docker', resource, 'rm'] + (['-f'] if resource == 'container' else []) + [name], stdout=subprocess.DEVNULL)

    try:
        acceptance.check('backup source has no active or queued builds', query(options.postgres_container, "SELECT count(*) FROM builds WHERE status IN ('resolving','planning','needs_input','replanning','queued','running','canceling')") == 0)
        before = database_inventory(options.postgres_container)
        acceptance.check('backup includes existing credentials and releases', before['credentials']['rows'] > 0 and before['releases']['rows'] > 0)
        with (backup / 'database.dump').open('wb') as output:
            execute(['docker', 'exec', options.postgres_container, 'pg_dump', '-U', 'artifactory', '-d', 'artifactory', '-Fc'], stdout=output)
        copy_private(options.env_file, backup / 'env')
        copy_private(Path(source['ARTIFACTORY_PKI_DIR']), backup / 'pki')
        copy_private(options.s3_config, backup / 's3.json')
        saved = load_environment(backup / 'env')
        acceptance.check('backup contains the original credential encryption key', saved['ARTIFACTORY_MASTER_KEY'] == source['ARTIFACTORY_MASTER_KEY'])
        source_objects = object_environment(source, source['ARTIFACTORY_S3_ENDPOINT'])
        bucket = 'artifactory_recovery:' + source['ARTIFACTORY_S3_BUCKET']
        manifest = json.loads(objects(['lsjson', bucket, '--recursive', '--files-only'], source_objects, True).stdout)
        objects(['copy', bucket, str(backup / 'objects')], source_objects)
        objects(['check', '--download', bucket, str(backup / 'objects')], source_objects)
        acceptance.check('all source object bytes match the backup', len(manifest) > 0)
        acceptance.check('source database stayed quiescent throughout backup', database_inventory(options.postgres_container) == before)

        postgres_image = json.loads(acceptance.command(['docker', 'inspect', options.postgres_container]))[0]['Image']
        s3_inspect = json.loads(acceptance.command(['docker', 'inspect', options.s3_container]))[0]
        pg_port, s3_port, http_port, grpc_port = free_ports(4)
        password = urllib.parse.unquote(urllib.parse.urlsplit(saved['DATABASE_URL']).password)
        (restored / 'database-password').write_text(password)
        postgres = service('postgres', postgres_image, ['-p', f'127.0.0.1:{pg_port}:5432', '-e', 'POSTGRES_DB=artifactory', '-e', 'POSTGRES_USER=artifactory', '-e', 'POSTGRES_PASSWORD_FILE=/run/db-password', '-v', str(restored / 'database-password') + ':/run/db-password:ro'], '/var/lib/postgresql/data')
        s3_name = service('s3', s3_inspect['Image'], ['-p', f'127.0.0.1:{s3_port}:8333', '-v', str(backup / 's3.json') + ':/etc/seaweed/s3.json:ro', '-v', str(acceptance.ROOT / 'deploy/seaweed-entrypoint.sh') + ':/run/artifactory-s3-entrypoint.sh:ro', '--entrypoint', '/bin/sh'], '/data', ['/run/artifactory-s3-entrypoint.sh', 'mini', '-dir=/data', '-s3.port=8333', '-s3.config=/run/artifactory-s3/s3.json', '-s3.iam.readOnly=true', '-admin.ui=false', '-ip.bind=0.0.0.0'])
        s3_endpoint = f'http://127.0.0.1:{s3_port}'
        execute(['python3', str(acceptance.ROOT / 'scripts/wait_local_services.py'), '--postgres', postgres, '--s3', s3_name, '--s3-url', s3_endpoint])
        execute(['docker','exec',postgres,'psql','-v','ON_ERROR_STOP=1','-U','artifactory','-d','artifactory','-c','SELECT timescaledb_pre_restore();'],capture_output=True)
        with (backup / 'database.dump').open('rb') as archive:
            execute(['docker', 'exec', '-i', postgres, 'pg_restore', '--exit-on-error', '--no-owner', '-U', 'artifactory', '-d', 'artifactory'], stdin=archive)
        execute(['docker','exec',postgres,'psql','-v','ON_ERROR_STOP=1','-U','artifactory','-d','artifactory','-c','SELECT timescaledb_post_restore();'],capture_output=True)
        acceptance.check('every public table and sequence matches after restore', database_inventory(postgres) == before)
        restored_objects = object_environment(saved, s3_endpoint)
        objects(['copy', str(backup / 'objects'), bucket], restored_objects)
        objects(['check', '--download', str(backup / 'objects'), bucket], restored_objects)
        acceptance.check('all restored S3 objects match backup bytes', True)
        copy_private(backup / 'pki', restored / 'pki')
        acceptance.check('restored CA, signing key and server identity match the backup', all((restored / 'pki' / path.name).read_bytes() == path.read_bytes() for path in (backup / 'pki').iterdir() if path.is_file()))

        base = f'http://127.0.0.1:{http_port}'
        environment = dict(saved, DATABASE_URL=f'postgres://artifactory:{urllib.parse.quote(password, safe="")}@127.0.0.1:{pg_port}/artifactory', ARTIFACTORY_HTTP_BIND=f'127.0.0.1:{http_port}', ARTIFACTORY_GRPC_BIND=f'127.0.0.1:{grpc_port}', ARTIFACTORY_PUBLIC_URL=base, ARTIFACTORY_PKI_DIR=str(restored / 'pki'), ARTIFACTORY_DATA_DIR=str(restored / "control's data with spaces"), ARTIFACTORY_S3_ENDPOINT=s3_endpoint, ARTIFACTORY_S3_PUBLIC_ENDPOINT=s3_endpoint, GIT_SSL_CAINFO=str(restored / 'pki/ca.pem'))
        log = (directory / 'control.log').open('ab')
        handles.append(log)
        control = subprocess.Popen([str(acceptance.ROOT / 'target/debug/artifactory-control')], cwd=acceptance.ROOT, env=environment, stdout=log, stderr=log)

        def control_ready():
            if control.poll() is not None:
                raise RuntimeError('restored control exited: ' + (directory / 'control.log').read_text()[-8192:])
            with urllib.request.urlopen(base + '/healthz', timeout=2) as response:
                return response.status == 200

        acceptance.wait_for(control_ready, 30)
        acceptance.BASE = base
        acceptance.FIXTURE_DATABASE_URL = environment['DATABASE_URL']
        os.environ['ARTIFACTORY_PUBLIC_URL'] = base
        acceptance.api('/auth/login', {'username': saved['ARTIFACTORY_BOOTSTRAP_USERNAME'], 'password': saved['ARTIFACTORY_BOOTSTRAP_PASSWORD']})
        for node in acceptance.api('/nodes'):
            settings = {key: node[key] for key in ('draining', 'revoked', 'slots', 'labels', 'signing_profiles', 'pool_id')}
            acceptance.api('/nodes/' + node['id'], dict(settings, draining=True), 'PATCH')
        release = acceptance.api('/releases/' + ssh['release_id'])
        artifact = release['artifacts'][0]
        with acceptance.HTTP.open(base + '/api/v1/artifacts/' + artifact['id'] + '/download', timeout=30) as response:
            content = response.read()
        acceptance.check('restored release retains its build and verified artifact bytes', release['build_id'] == ssh['build_id'] and len(content) == artifact['size'] and hashlib.sha256(content).hexdigest() == artifact['sha256'])
        binary = directory / 'restored-download'
        binary.write_bytes(content)
        binary.chmod(0o700)
        acceptance.check('artifact downloaded after recovery executes', acceptance.command([str(binary)]) == 'artifactory-ssh-ok')

        ssh_log = (directory / 'sshd.log').open('ab')
        handles.append(ssh_log)
        ssh_arguments = [shutil.which('sshd') or '/usr/sbin/sshd', '-D', '-e', '-f', str(ssh_config)]
        if options.sudo_sshd:
            ssh_arguments = ['sudo', '-n'] + ssh_arguments
        try:
            with socket.create_connection(('127.0.0.1', ssh_port), timeout=1):
                raise RuntimeError('retained SSH fixture port is already occupied')
        except ConnectionRefusedError:
            pass
        ssh_server = subprocess.Popen(ssh_arguments, stdout=ssh_log, stderr=ssh_log)

        def ssh_ready():
            if ssh_server.poll() is not None:
                raise RuntimeError('restored SSH fixture exited: ' + (directory / 'sshd.log').read_text()[-8192:])
            with socket.create_connection(('127.0.0.1', ssh_port), timeout=1):
                return True

        acceptance.wait_for(ssh_ready, 15)
        token = acceptance.api('/enrollment-tokens', {'pool_id': '00000000-0000-0000-0000-000000000001'})['token']
        config_file = restored / 'emissary.json'
        emissary_environment = {key: value for key, value in saved.items() if not key.startswith('ARTIFACTORY_') and key != 'DATABASE_URL'}
        emissary_binary = acceptance.ROOT / 'target/debug/artifactory-emissary'
        execute([str(emissary_binary), 'register', '--endpoint', f'localhost:{grpc_port}', '--ca', str(restored / 'pki/ca.pem'), '--config', str(config_file), '--root', str(restored / "emissary's data with spaces"), '--allow-http-uploads'], env=dict(emissary_environment, ARTIFACTORY_ENROLLMENT_TOKEN=token), stdout=subprocess.DEVNULL)
        config = json.loads(config_file.read_text())
        emissary_log = (directory / 'emissary.log').open('ab')
        handles.append(emissary_log)
        emissary = subprocess.Popen([str(emissary_binary), 'run', '--config', str(config_file)], env=emissary_environment, stdout=emissary_log, stderr=emissary_log)
        acceptance.wait_for(lambda: next((node for node in acceptance.api('/nodes') if node['id'] == config['emissary_id'] and 'cc' in node['capabilities']), None), 90)
        acceptance.check('restored PKI issues a usable native emissary identity', True)
        build_id = acceptance.api('/projects/' + ssh['project_id'] + '/builds', {'ref': 'main', 'release_version': 'recovery-' + acceptance.RUN_ID})['id']
        acceptance.EVIDENCE['restored_build_id'] = build_id

        def build_finished():
            result = acceptance.api('/builds/' + build_id)
            if result['status'] in ('succeeded', 'failed', 'canceled', 'lost'):
                acceptance.EVIDENCE['restored_build_status'] = result['status']
                acceptance.EVIDENCE['restored_build_error'] = result.get('error')
                return result
            return None

        rebuilt = acceptance.wait_for(build_finished, 120)
        acceptance.check('fresh native build succeeds after recovery', rebuilt['status'] == 'succeeded')
        acceptance.check('restored encrypted SSH credential supports a fresh native build', rebuilt['commit_sha'] == ssh['fixture_commit'])
        acceptance.wait_for(lambda: acceptance.api('/builds/' + build_id)['publish_status'] == 'succeeded')
        new_release = acceptance.api('/builds/' + build_id + '/publish', {})
        new_release = acceptance.api('/releases/' + new_release['id'])
        acceptance.check('fresh build publishes a new release after recovery', new_release['id'] != release['id'] and new_release['build_id'] == build_id)
        acceptance.check('fresh builds can publish after recovery without changing the old release', acceptance.api('/releases/' + ssh['release_id']) == release)
        acceptance.check('recovery left the source database unchanged', database_inventory(options.postgres_container) == before)
        acceptance.EVIDENCE.update({'kind': 'isolated-recovery', 'source_ssh_run': ssh['run_id'], 'source_sha': acceptance.command(['git', '-C', str(acceptance.ROOT), 'rev-parse', 'HEAD']), 'script_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'database_tables': len(before) - 1, 'source_objects': len(manifest), 'source_object_bytes': sum(item['Size'] for item in manifest), 'database_dump_sha256': hashlib.sha256((backup / 'database.dump').read_bytes()).hexdigest(), 'postgres_image': postgres_image, 's3_image': s3_inspect['Image'], 'restored_build_id': build_id, 'backup': str(backup), 'boundary': 'local isolated restore with private plaintext backup; production encryption, offsite storage and disaster recovery remain separate gates'})
    except BaseException as error:
        failure = error
        acceptance.EVIDENCE['failure'] = str(error)
        raise
    finally:
        for process in (emissary, ssh_server, control):
            try:
                acceptance.stop_process(process)
            except Exception as error:
                cleanup_errors.append(str(error))
        for handle in handles:
            try:
                handle.close()
            except Exception as error:
                cleanup_errors.append(str(error))
        for container in reversed(created_containers):
            try:
                remove_owned('container', container)
            except Exception as error:
                cleanup_errors.append(str(error))
        for volume in reversed(created_volumes):
            try:
                remove_owned('volume', volume)
            except Exception as error:
                cleanup_errors.append(str(error))
        acceptance.EVIDENCE['cleanup_errors'] = cleanup_errors
        evidence.write_text(json.dumps(acceptance.EVIDENCE, indent=2) + '\n')
        if cleanup_errors and failure is None:
            raise RuntimeError('recovery cleanup failed; see ' + str(evidence))
    (acceptance.ROOT / '.local/acceptance/latest-recovery.json').write_text(json.dumps(acceptance.EVIDENCE, indent=2) + '\n')
    print('RECOVERY ACCEPTANCE PASSED', json.dumps({'run_id': acceptance.RUN_ID, 'checks': len(acceptance.EVIDENCE['checks']), 'evidence': str(evidence)}), flush=True)


if __name__ == '__main__':
    main()
