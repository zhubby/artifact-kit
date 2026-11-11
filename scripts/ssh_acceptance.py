#!/usr/bin/env python3
"""Exercise real SSH Git through the running local acceptance control plane and agent."""

import argparse
import getpass
import hashlib
import json
import os
import platform
import shlex
import shutil
import socket
import subprocess
import urllib.parse
from pathlib import Path

import acceptance


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--sudo-sshd', action='store_true', help='run only the isolated SSH daemon via passwordless sudo on an ephemeral Linux CI runner')
    options = parser.parse_args()
    if os.environ.get('ARTIFACTORY_INSECURE_DEV') != 'true':
        parser.error('this destructive fixture test requires the explicit local acceptance environment')
    if platform.system() not in ('Darwin', 'Linux'):
        parser.error('the SSH fixture requires a Unix host with OpenSSH server')
    sshd = shutil.which('sshd') or '/usr/sbin/sshd'
    if not Path(sshd).is_file():
        parser.error('OpenSSH server is required')

    directory = acceptance.DIRECTORY
    private = directory / 'SSH fixture with spaces'
    private.mkdir(mode=0o700)
    for name in ('host', 'client', 'wrong-host', 'wrong-client'):
        acceptance.command(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(private / name)])
    (private / 'authorized_keys').write_text((private / 'client.pub').read_text())
    (private / 'authorized_keys').chmod(0o600)
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        port = listener.getsockname()[1]
    username = getpass.getuser()
    git_shell = Path(shutil.which('git-shell') or (Path(acceptance.command(['git', '--exec-path'])) / 'git-shell'))
    if not git_shell.is_file():
        parser.error('git-shell is required for the restricted SSH fixture')
    settings = f'''Port {port}
ListenAddress 127.0.0.1
HostKey "{private / 'host'}"
PidFile "{private / 'sshd.pid'}"
AuthorizedKeysFile "{private / 'authorized_keys'}"
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM {'yes' if platform.system() == 'Linux' and options.sudo_sshd else 'no'}
PermitRootLogin no
AllowUsers {username}
AllowTcpForwarding no
AllowAgentForwarding no
X11Forwarding no
PermitTunnel no
PermitTTY no
ForceCommand {shlex.quote(str(git_shell))} -c "$SSH_ORIGINAL_COMMAND"
LogLevel VERBOSE
'''
    config_file = private / 'sshd.conf'
    config_file.write_text(settings)
    host_key = ' '.join((private / 'host.pub').read_text().split()[:2])
    wrong_host = ' '.join((private / 'wrong-host.pub').read_text().split()[:2])
    known_hosts = f'[127.0.0.1]:{port} {host_key}\n'
    repository = private / 'fixture.git'
    repository.mkdir()
    acceptance.command(['git', 'init', '-b', 'main', str(repository)])
    (repository / 'main.c').write_text('#include <stdio.h>\nint main(void) { puts("artifactory-ssh-ok"); return 0; }\n')
    host_os = {'Darwin': 'darwin', 'Linux': 'linux'}[platform.system()]
    arch = 'arm64' if platform.machine() in ('arm64', 'aarch64') else 'amd64'
    (repository / 'artifactory.yaml').write_text(f'''version: 1
release: {{enabled: true}}
targets:
  native:
    os: {host_os}
    arch: {arch}
    capabilities: [cc]
    steps:
      - name: compile
        run: mkdir -p dist && cc main.c -o dist/app
      - name: verify
        phase: verify
        run: ./dist/app
    artifacts: [dist/app]
''')
    acceptance.command(['git', '-C', str(repository), 'add', '.'])
    acceptance.command(['git', '-C', str(repository), '-c', 'user.name=SSH acceptance', '-c', 'user.email=acceptance@localhost', 'commit', '-m', 'SSH native fixture'])
    commit = acceptance.command(['git', '-C', str(repository), 'rev-parse', 'HEAD'])
    repository_url = f'ssh://{urllib.parse.quote(username)}@127.0.0.1:{port}{urllib.parse.quote(str(repository))}'
    arguments = [sshd, '-D', '-e', '-f', str(config_file)]
    if options.sudo_sshd:
        arguments = ['sudo', '-n'] + arguments
    process = None
    evidence = directory / 'evidence.json'
    acceptance.EVIDENCE.update({'kind': 'ssh-git', 'ssh_config': str(config_file), 'script_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()})
    with (directory / 'sshd.log').open('w') as log:
        try:
            process = subprocess.Popen(arguments, stdout=log, stderr=log)

            def ready():
                if process.poll() is not None:
                    raise RuntimeError('isolated SSH server exited: ' + (directory / 'sshd.log').read_text())
                with socket.create_connection(('127.0.0.1', port), timeout=1):
                    return True

            acceptance.wait_for(ready, 15)
            acceptance.api('/auth/login', {'username': os.environ['ARTIFACTORY_BOOTSTRAP_USERNAME'], 'password': os.environ['ARTIFACTORY_BOOTSTRAP_PASSWORD']})
            credential = acceptance.api('/credentials', {'name': 'SSH acceptance ' + acceptance.RUN_ID, 'kind': 'ssh', 'secret': (private / 'client').read_text(), 'known_hosts': known_hosts})['id']
            credentials = json.dumps(acceptance.api('/credentials'))
            acceptance.check('SSH credential response redacts private key', (private / 'client').read_text() not in credentials)
            project = acceptance.api('/projects', {'name': 'SSH native ' + acceptance.RUN_ID, 'repository_url': repository_url, 'credential_id': credential})['id']
            build_id = acceptance.api(f'/projects/{project}/builds', {'ref': 'main', 'release_version': 'ssh-' + acceptance.RUN_ID})['id']
            result = acceptance.terminal(build_id, 'succeeded')
            acceptance.check('SSH control and worker use the same pinned source', result['commit_sha'] == commit)
            artifact = result['artifacts'][0]
            with acceptance.HTTP.open(acceptance.BASE + '/api/v1/artifacts/' + artifact['id'] + '/download', timeout=30) as response:
                content = response.read()
            acceptance.check('SSH native artifact download matches SHA-256 and size', hashlib.sha256(content).hexdigest() == artifact['sha256'] and len(content) == artifact['size'])
            binary = directory / 'ssh-downloaded-app'
            binary.write_bytes(content)
            binary.chmod(0o700)
            acceptance.check('downloaded SSH-built native artifact runs', acceptance.command([str(binary)]) == 'artifactory-ssh-ok')
            acceptance.wait_for(lambda: acceptance.api('/builds/' + build_id)['publish_status'] == 'succeeded')
            release = acceptance.api('/builds/' + build_id + '/publish', {})
            acceptance.check('SSH build publishes an immutable release', acceptance.api('/releases/' + release['id'])['build_id'] == build_id)

            for failure, key, hosts in (
                ('wrong host fingerprint', private / 'client', f'[127.0.0.1]:{port} {wrong_host}\n'),
                ('unauthorized SSH key', private / 'wrong-client', known_hosts),
            ):
                rejected = acceptance.api('/credentials', {'name': failure + ' ' + acceptance.RUN_ID, 'kind': 'ssh', 'secret': key.read_text(), 'known_hosts': hosts})['id']
                project_id = acceptance.api('/projects', {'name': failure + ' ' + acceptance.RUN_ID, 'repository_url': repository_url, 'credential_id': rejected})['id']
                rejected_build = acceptance.api(f'/projects/{project_id}/builds', {'ref': 'main'})['id']
                failed = acceptance.terminal(rejected_build, 'failed')
                acceptance.check(f'SSH rejects {failure} before worker assignment', not failed['jobs'] and not failed['artifacts'])
            acceptance.EVIDENCE.update({'kind': 'ssh-git', 'project_id': project, 'build_id': build_id, 'release_id': release['id'], 'fixture_commit': commit, 'ssh_config': str(config_file), 'source_sha': acceptance.command(['git', '-C', str(acceptance.ROOT), 'rev-parse', 'HEAD'])})
        except BaseException as error:
            acceptance.EVIDENCE['failure'] = str(error)
            raise
        finally:
            try:
                acceptance.stop_process(process)
            finally:
                evidence.write_text(json.dumps(acceptance.EVIDENCE, indent=2) + '\n')
    (acceptance.ROOT / '.local/acceptance/latest-ssh.json').write_text(json.dumps(acceptance.EVIDENCE, indent=2) + '\n')
    print('SSH ACCEPTANCE PASSED', json.dumps({'run_id': acceptance.RUN_ID, 'checks': len(acceptance.EVIDENCE['checks']), 'evidence': str(evidence)}), flush=True)


if __name__ == '__main__':
    main()
