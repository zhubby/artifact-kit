"""Read-only local Git/LFS server for immutable snapshot recovery tests."""
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import urllib.parse

root = Path(sys.argv[1])


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
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
        body = self.rfile.read(int(self.headers.get('content-length', '0')))
        if parsed.path.endswith('/info/lfs/objects/batch'):
            request = json.loads(body)
            if request.get('operation') != 'download':
                self.send_error(403)
                return
            objects = []
            for item in request['objects']:
                oid = item['oid']
                path = root / 'repo.git/.git/lfs/objects' / oid[:2] / oid[2:4] / oid
                objects.append({'oid': oid, 'size': path.stat().st_size, 'actions': {'download': {'href': f'http://127.0.0.1:{self.server.server_port}/repo.git/lfs/{oid}'}}})
            data = json.dumps({'transfer': 'basic', 'objects': objects}).encode()
            content_type = 'application/vnd.git-lfs+json'
        elif '/lfs/' in parsed.path:
            oid = parsed.path.rsplit('/', 1)[1]
            if len(oid) != 64 or any(c not in '0123456789abcdef' for c in oid):
                self.send_error(404)
                return
            data = (root / 'repo.git/.git/lfs/objects' / oid[:2] / oid[2:4] / oid).read_bytes()
            content_type = 'application/octet-stream'
        else:
            env = dict(os.environ, GIT_PROJECT_ROOT=str(root), GIT_HTTP_EXPORT_ALL='1', PATH_INFO=parsed.path,
                       REQUEST_METHOD=self.command, QUERY_STRING=parsed.query, CONTENT_TYPE=self.headers.get('content-type', ''), CONTENT_LENGTH=str(len(body)))
            result = subprocess.run(['git', 'http-backend'], input=body, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env, check=True)
            headers, _, data = result.stdout.partition(b'\r\n\r\n')
            content_type = next(line.partition(':')[2].strip() for line in headers.decode().splitlines() if line.lower().startswith('content-type:'))
        self.send_response(200)
        self.send_header('content-type', content_type)
        self.send_header('content-length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
print(server.server_port, flush=True)
server.serve_forever()
