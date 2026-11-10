"""Exercise the real Rust Storage client with trusted and untrusted TLS peers."""

import http.server
import os
from pathlib import Path
import ssl
import subprocess
import sys
import threading
import urllib.parse
import urllib.request

os.umask(0o077)
helper = sys.argv[1]
directory = Path(sys.argv[2])


def openssl(*arguments):
    subprocess.run(['openssl', *arguments], cwd=directory, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)


for name in ('trusted', 'unrelated'):
    openssl('req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1', '-keyout', name + '.key', '-out', name + '.pem', '-subj', '/CN=Storage TLS ' + name, '-addext', 'basicConstraints=critical,CA:TRUE', '-addext', 'keyUsage=critical,keyCertSign', '-addext', 'subjectKeyIdentifier=hash')
openssl('req', '-newkey', 'rsa:2048', '-nodes', '-keyout', 'server.key', '-out', 'server.csr', '-subj', '/CN=localhost')
(directory / 'server.ext').write_text('subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nsubjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid,issuer\n')
openssl('x509', '-req', '-in', 'server.csr', '-CA', 'trusted.pem', '-CAkey', 'trusted.key', '-CAcreateserial', '-out', 'server.pem', '-days', '1', '-sha256', '-extfile', 'server.ext')
openssl('verify', '-verify_hostname', 'localhost', '-CAfile', 'trusted.pem', 'server.pem')
(directory / 'empty-ca-directory').mkdir()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_arguments):
        pass

    def do_PUT(self):
        if urllib.parse.urlsplit(self.path).path.rstrip('/') != '/tls-probe':
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header('Content-Length', '0')
        self.end_headers()


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.minimum_version = ssl.TLSVersion.TLSv1_3
context.load_cert_chain(directory / 'server.pem', directory / 'server.key')
server.socket = context.wrap_socket(server.socket, server_side=True)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
endpoint = 'https://localhost:' + str(server.server_port)


def rust_client(ca, address):
    environment = dict(os.environ, AK_TEST_STORAGE_TLS_HELPER='1', AK_TEST_STORAGE_TLS_ENDPOINT=address, AK_TEST_STORAGE_TLS_DIRECTORY=str(directory), SSL_CERT_FILE=str(directory / ca), SSL_CERT_DIR=str(directory / 'empty-ca-directory'), NO_PROXY='localhost,127.0.0.1', no_proxy='localhost,127.0.0.1')
    return subprocess.run([helper, '--exact', 'storage_tls_probe_helper', '--nocapture'], env=environment, capture_output=True, text=True, timeout=15)


try:
    request = urllib.request.Request(endpoint + '/tls-probe', data=b'', method='PUT')
    trusted = ssl.create_default_context(cafile=str(directory / 'trusted.pem'))
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=trusted))
    with opener.open(request, timeout=10) as response:
        assert response.status == 200

    result = rust_client('trusted.pem', endpoint)
    assert result.returncode == 0, 'product rejected its configured trust store:\n' + result.stdout + result.stderr

    result = rust_client('unrelated.pem', endpoint)
    assert result.returncode != 0 and 'UnknownIssuer' in result.stdout + result.stderr, 'product accepted an untrusted issuer or failed for the wrong reason:\n' + result.stdout + result.stderr

    result = rust_client('trusted.pem', endpoint.replace('localhost', '127.0.0.1'))
    assert result.returncode != 0 and 'certificate not valid for name "127.0.0.1"' in result.stdout + result.stderr, 'product accepted a wrong hostname or failed for the wrong reason:\n' + result.stdout + result.stderr
    print('trusted private CA accepted; unrelated CA and wrong hostname rejected')
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=5)
