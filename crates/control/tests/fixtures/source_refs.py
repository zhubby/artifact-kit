"""Read-only Git reference queries over authenticated HTTPS and isolated OpenSSH."""

import base64
import getpass
import http.server
import json
import os
from pathlib import Path
import shlex
import shutil
import socket
import ssl
import subprocess
import sys
import threading
import time
import urllib.parse

os.umask(0o077)
helper = sys.argv[1]
directory = Path(sys.argv[2])


def command(*arguments):
    return subprocess.run(arguments, cwd=directory, check=True, capture_output=True, text=True, timeout=20).stdout.strip()


command('openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1', '-keyout', 'ca.key', '-out', 'ca.pem', '-subj', '/CN=Git reference fixture CA', '-addext', 'basicConstraints=critical,CA:TRUE', '-addext', 'keyUsage=critical,keyCertSign')
command('openssl', 'req', '-newkey', 'rsa:2048', '-nodes', '-keyout', 'server.key', '-out', 'server.csr', '-subj', '/CN=localhost')
(directory / 'server.ext').write_text('subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\n')
command('openssl', 'x509', '-req', '-in', 'server.csr', '-CA', 'ca.pem', '-CAkey', 'ca.key', '-CAcreateserial', '-out', 'server.pem', '-days', '1', '-sha256', '-extfile', 'server.ext')
work = directory / 'work'
command('git', 'init', '-b', 'trunk', str(work))
(work / 'source.txt').write_text('immutable source fixture\n')
command('git', '-C', str(work), 'add', '.')
command('git', '-C', str(work), '-c', 'user.name=Reference fixture', '-c', 'user.email=refs@localhost', '-c', 'commit.gpgsign=false', 'commit', '-m', 'source')
command('git', '-C', str(work), 'branch', 'v1')
command('git', '-C', str(work), '-c', 'user.name=Reference fixture', '-c', 'user.email=refs@localhost', '-c', 'tag.gpgSign=false', 'tag', '-a', 'v1', '-m', 'annotated tag')
repository = directory / 'repo.git'
command('git', 'clone', '--bare', str(work), str(repository))
original_refs = command('git', '-C', str(repository), 'show-ref')
password = 'fixture-password-not-a-real-secret'
authorization = 'Basic ' + base64.b64encode(f'refs-user:{password}'.encode()).decode()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_arguments):
        pass

    def do_GET(self):
        self.serve()

    def do_POST(self):
        self.serve()

    def serve(self):
        parsed = urllib.parse.urlsplit(self.path)
        if not parsed.path.startswith('/repo.git/') or 'receive-pack' in self.path:
            self.send_error(403)
            return
        if self.headers.get('Authorization') != authorization:
            self.send_response(401)
            self.send_header('WWW-Authenticate', 'Basic realm="Git reference fixture"')
            self.send_header('Content-Length', '0')
            self.end_headers()
            return
        body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
        environment = dict(os.environ, GIT_PROJECT_ROOT=str(directory), GIT_HTTP_EXPORT_ALL='1', PATH_INFO=parsed.path,
                           REQUEST_METHOD=self.command, QUERY_STRING=parsed.query, REMOTE_USER='refs-user',
                           CONTENT_TYPE=self.headers.get('Content-Type', ''), CONTENT_LENGTH=str(len(body)))
        result = subprocess.run(['git', 'http-backend'], input=body, capture_output=True, env=environment, check=True, timeout=10)
        headers, _, content = result.stdout.partition(b'\r\n\r\n')
        content_type = next(line.partition(':')[2].strip() for line in headers.decode().splitlines() if line.lower().startswith('content-type:'))
        self.send_response(200)
        self.send_header('Content-Type', content_type)
        self.send_header('Content-Length', str(len(content)))
        self.end_headers()
        self.wfile.write(content)


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(directory / 'server.pem', directory / 'server.key')
server.socket = context.wrap_socket(server.socket, server_side=True)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
endpoint = f'https://localhost:{server.server_port}/repo.git'

for name in ('host', 'client', 'wrong-host', 'wrong-client'):
    command('ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(directory / name))
(directory / 'authorized_keys').write_text((directory / 'client.pub').read_text())
with socket.socket() as listener:
    listener.bind(('127.0.0.1', 0))
    ssh_port = listener.getsockname()[1]
username = getpass.getuser()
git_shell = shutil.which('git-shell') or str(Path(command('git', '--exec-path')) / 'git-shell')
config = directory / 'sshd.conf'
config.write_text(f'''Port {ssh_port}
ListenAddress 127.0.0.1
HostKey "{directory / 'host'}"
PidFile "{directory / 'sshd.pid'}"
AuthorizedKeysFile "{directory / 'authorized_keys'}"
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
PermitRootLogin prohibit-password
AllowUsers {username}
AllowTcpForwarding no
AllowAgentForwarding no
X11Forwarding no
PermitTTY no
ForceCommand {shlex.quote(git_shell)} -c "$SSH_ORIGINAL_COMMAND"
''')
host_key = ' '.join((directory / 'host.pub').read_text().split()[:2])
wrong_host = ' '.join((directory / 'wrong-host.pub').read_text().split()[:2])
ssh_url = f'ssh://{username}@127.0.0.1:{ssh_port}{urllib.parse.quote(str(repository))}'
known_hosts = f'[127.0.0.1]:{ssh_port} {host_key}\n'
cases = [
    {'name': 'HTTPS credentials and CA', 'kind': 'https', 'url': endpoint, 'password': password, 'status': 200},
    {'name': 'HTTPS bad credentials', 'kind': 'https', 'url': endpoint, 'password': 'wrong-password', 'status': 502},
    {'name': 'HTTPS wrong hostname', 'kind': 'https', 'url': endpoint.replace('localhost', '127.0.0.1'), 'password': password, 'status': 502},
    {'name': 'SSH key and host trust', 'kind': 'ssh', 'url': ssh_url, 'key_path': str(directory / 'client'), 'known_hosts': known_hosts, 'status': 200},
    {'name': 'SSH wrong host', 'kind': 'ssh', 'url': ssh_url, 'key_path': str(directory / 'client'), 'known_hosts': f'[127.0.0.1]:{ssh_port} {wrong_host}\n', 'status': 502},
    {'name': 'SSH unauthorized key', 'kind': 'ssh', 'url': ssh_url, 'key_path': str(directory / 'wrong-client'), 'known_hosts': known_hosts, 'status': 502},
    {'name': 'HTTPS ignores ambient credentials', 'kind': 'none', 'url': endpoint, 'status': 502},
    {'name': 'SSH ignores ambient credentials and trust', 'kind': 'none', 'url': ssh_url, 'status': 502},
]
fixture = directory / 'fixture.json'
fixture.write_text(json.dumps({'cases': cases}))
ambient_hosts = directory / 'ambient_known_hosts'
ambient_hosts.write_text(known_hosts)
ambient_askpass = directory / 'ambient-askpass'
ambient_askpass.write_text(f'#!/bin/sh\ncase "$1" in *sername*) printf "%s\\n" refs-user;; *) printf "%s\\n" {shlex.quote(password)};; esac\n')
ambient_askpass.chmod(0o700)
ambient_home = directory / 'ambient-home'
ambient_home.mkdir()
(ambient_home / '.netrc').write_text(f'machine localhost login refs-user password {password}\n')
ambient_ssh = shlex.join(['ssh', '-F', '/dev/null', '-i', str(directory / 'client'), '-o', 'IdentitiesOnly=yes', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes', '-o', 'UserKnownHostsFile="' + str(ambient_hosts) + '"'])
process = None
try:
    with (directory / 'sshd.log').open('w') as log:
        process = subprocess.Popen([shutil.which('sshd') or '/usr/sbin/sshd', '-D', '-e', '-f', str(config)], stdout=log, stderr=log)
        deadline = time.monotonic() + 10
        while True:
            if process.poll() is not None:
                raise RuntimeError('isolated SSH server exited: ' + (directory / 'sshd.log').read_text())
            try:
                with socket.create_connection(('127.0.0.1', ssh_port), timeout=0.2):
                    break
            except OSError:
                if time.monotonic() > deadline:
                    raise TimeoutError('isolated SSH server did not start')
                time.sleep(0.05)
        environment = dict(os.environ, ARTIFACTORY_TEST_REFS_FIXTURE=str(fixture), GIT_SSL_CAINFO=str(directory / 'ca.pem'),
                           HOME=str(ambient_home), GIT_ASKPASS=str(ambient_askpass), GIT_SSH_COMMAND=ambient_ssh,
                           NO_PROXY='localhost,127.0.0.1', no_proxy='localhost,127.0.0.1')
        result = subprocess.run([helper, '--exact', 'source_refs::repository_refs_probe_helper', '--nocapture'], env=environment, capture_output=True, text=True, timeout=90)
        assert result.returncode == 0, result.stdout + result.stderr + '\n' + (directory / 'sshd.log').read_text()
        assert command('git', '-C', str(repository), 'show-ref') == original_refs, 'query changed remote references'
        print('HTTPS credentials and hostname trust; SSH keys and host fingerprints; immutable references verified')
finally:
    if process is not None:
        process.terminate()
        process.wait(timeout=5)
    server.shutdown()
    server.server_close()
    thread.join(timeout=5)
