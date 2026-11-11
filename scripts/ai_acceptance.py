#!/usr/bin/env python3
"""Isolated real-model acceptance. Keys are read from the invoking environment.

Never starts against the deployment database/schema. Retains evidence and
private configuration in .local/ai-acceptance; upstream accepts reads only.
"""
import argparse
import base64
import hashlib
import http.client
import http.cookiejar
import http.server
import json
import os
from pathlib import Path
import platform
import shlex
import shutil
import signal
import ssl
import subprocess
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
os.umask(0o077)


def command(args, **kwargs):
    result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs)
    if result.returncode:
        raise RuntimeError(f"{args[0]} failed ({result.returncode}): " + result.stderr.decode(errors='replace')[:1500])
    return result.stdout.decode().strip()


def environment():
    deployed=ROOT/'.local/artifactory-deploy/latest/env.json'
    if deployed.exists():
        return json.loads(deployed.read_text())
    result = {}
    for line in (ROOT / '.local/env').read_text().splitlines():
        if not line.strip() or line.lstrip().startswith('#'):
            continue
        key, value = line.removeprefix('export ').split('=', 1)
        parsed = shlex.split(value)
        result[key] = parsed[0] if parsed else ''
    return result


class Acceptance:
    def __init__(self, args):
        self.args = args
        self.run = ROOT / '.local/ai-acceptance' / time.strftime('%Y%m%dT%H%M%S')
        self.run.mkdir(parents=True)
        self.processes = []
        self.config = environment()
        self.schema = 'ai_' + uuid.uuid4().hex
        self.db = self.config['DATABASE_URL']
        self.sql(f'CREATE SCHEMA {self.schema}', isolated=False)
        parts = urllib.parse.urlsplit(self.db)
        query = urllib.parse.parse_qsl(parts.query) + [('options', '-csearch_path=' + self.schema)]
        self.config['DATABASE_URL'] = urllib.parse.urlunsplit(parts._replace(query=urllib.parse.urlencode(query)))
        self.config.update(ARTIFACTORY_HTTP_BIND='127.0.0.1:38080', ARTIFACTORY_GRPC_BIND='127.0.0.1:38443',
                           ARTIFACTORY_PUBLIC_URL='http://127.0.0.1:38080', ARTIFACTORY_INSECURE_DEV='true',
                           ARTIFACTORY_DATA_DIR=str(self.run / 'control'), ARTIFACTORY_BOOTSTRAP_USERNAME='ai-admin',
                           ARTIFACTORY_BOOTSTRAP_PASSWORD=uuid.uuid4().hex, ARTIFACTORY_S3_BUCKET='ai-' + uuid.uuid4().hex[:12])
        self.grpc_endpoint = 'localhost:38443'
        if args.tailnet_address:
            self.config['ARTIFACTORY_PUBLIC_URL'] = 'https://' + args.tailnet_address + ':39444'
            self.config['ARTIFACTORY_GRPC_BIND'] = args.tailnet_address + ':38443'
            self.grpc_endpoint = args.tailnet_address + ':38443'
        (self.run / 'env.json').write_text(json.dumps(self.config, indent=2))
        self.base = self.config['ARTIFACTORY_PUBLIC_URL']
        self.jar = http.cookiejar.CookieJar()
        context=ssl.create_default_context()
        context.load_verify_locations(cafile=str(Path(self.config['ARTIFACTORY_PKI_DIR'])/'ca.pem'))
        # Existing deployment PKI predates Python 3.13's strict AKI defaults.
        # Keep signature, trust-chain and hostname verification enabled.
        context.verify_flags &= ~ssl.VERIFY_X509_STRICT
        self.http = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPCookieProcessor(self.jar), urllib.request.HTTPSHandler(context=context))
        self.evidence = {'schema': self.schema, 'checks': [], 'builds': {}, 'upstream': {'git_bytes': 0, 'lfs_bytes': 0, 'reads': 0, 'writes': 0}}

    def sql(self, query, isolated=True):
        parts=urllib.parse.urlsplit(self.config['DATABASE_URL'] if isolated else self.db)
        env=dict(os.environ,PGHOST=parts.hostname,PGPORT=str(parts.port or 5432),PGUSER=urllib.parse.unquote(parts.username or ''),
                 PGPASSWORD=urllib.parse.unquote(parts.password or ''),PGDATABASE=parts.path.lstrip('/'))
        env.update({'PG'+key.upper():value for key,value in urllib.parse.parse_qsl(parts.query)})
        return command(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1'], input=query.encode(), env=env)

    def check(self, name, condition):
        if not condition:
            raise AssertionError(name)
        self.evidence['checks'].append(name)
        print('PASS', name, flush=True)
        self.save()

    def save(self):
        (self.run / 'evidence.json').write_text(json.dumps(self.evidence, indent=2))

    def start(self, name, args, env=None, cwd=None, stdin=None):
        output = (self.run / (name + '.log')).open('ab')
        process = subprocess.Popen(args, cwd=cwd or ROOT, env=env, stdin=subprocess.PIPE if stdin is not None else subprocess.DEVNULL,
                                   stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        output.close()
        if stdin is not None:
            process.stdin.write(stdin.encode()); process.stdin.close()
        self.processes.append(process)
        (self.run / (name + '.pid')).write_text(str(process.pid))
        return process

    def api(self, path, body=None, method=None):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.base + '/api/v1' + path, data=data, method=method,
                                        headers={'Content-Type': 'application/json', 'Origin': self.base, 'x-artifactory-csrf': '1'})
        try:
            with self.http.open(request, timeout=45) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            raise RuntimeError(f'{path}: HTTP {error.code}: {error.read().decode()[:1000]}') from None

    def wait(self, callback, seconds=60):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            try:
                value = callback()
                if value:
                    return value
            except (OSError, urllib.error.URLError):
                pass
            time.sleep(.5)
        raise TimeoutError('acceptance wait timed out')

    def fixtures(self):
        root = self.run / 'upstream'; root.mkdir()
        def repo(name, files):
            path = root / (name + '.git')
            command(['git', 'init', '-b', 'main', str(path)])
            for name, content in files.items():
                destination = path / name; destination.parent.mkdir(parents=True, exist_ok=True); destination.write_text(content)
            return path
        def git(path, *args):
            return command(['git', '-C', str(path), '-c', 'user.name=AI Acceptance', '-c', 'user.email=acceptance@localhost', *args])
        def commit(path):
            git(path, 'add', '.'); git(path, 'commit', '-m', 'Read-only AI source fixture')
        base = repo('base', {'base.c': 'int base_value(void) { return 3; }\n'}); commit(base)
        library = repo('library', {'lib.c': 'int base_value(void); int lib_value(void) { return base_value() + 4; }\n'})
        git(library, '-c', 'protocol.file.allow=always', 'submodule', 'add', str(base), 'nested/base')
        git(library, 'config', '-f', '.gitmodules', 'submodule.nested/base.url', '../base.git'); commit(library)
        main = repo('app', {
            'main.c': '#include <stdio.h>\nint lib_value(void); int main(void) { printf("artifactory-ai-%d\\n", lib_value()); return 0; }\n',
            'README.md': '# Native example\nBuild main.c, modules/library/lib.c and modules/library/nested/base/base.c with cc.\nThe executable must print artifactory-ai-7. payload.dat is a Git LFS text object and must contain artifactory-lfs-ok.\nCreate final deliverables in the untracked dist directory. Do not modify source.\n',
            '.gitignore': 'dist/\n',
        })
        git(main, '-c', 'protocol.file.allow=always', 'submodule', 'add', str(library), 'modules/library')
        git(main, 'config', '-f', '.gitmodules', 'submodule.modules/library.url', '../library.git')
        git(main, 'lfs', 'install', '--local', '--skip-smudge'); git(main, 'lfs', 'track', 'payload.dat')
        (main / 'payload.dat').write_text('artifactory-lfs-ok\n' * 4096)
        commit(main); git(main, 'tag', '-a', 'v-fixture-1', '-m', 'Frozen annotated fixture tag')
        self.upstream_before = {p.name: git(p, 'show-ref') for p in [base, library, main]}
        self.source_commit = git(main, 'rev-parse', 'HEAD')
        self.source_root = root
        owner = self
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass
            def do_GET(self):
                self.serve()
            def do_POST(self):
                self.serve()
            def serve(self):
                parsed = urllib.parse.urlsplit(self.path)
                path = parsed.path
                parts = path.strip('/').split('/')
                if not parts or parts[0] not in ['app.git', 'library.git', 'base.git']:
                    self.send_error(404); return
                repository = root / parts[0]
                if 'receive-pack' in self.path:
                    owner.evidence['upstream']['writes'] += 1; self.send_error(403); return
                if path.endswith('/info/lfs/objects/batch'):
                    payload = json.loads(self.rfile.read(int(self.headers['content-length'])))
                    if payload.get('operation') != 'download':
                        owner.evidence['upstream']['writes'] += 1; self.send_error(403); return
                    objects = []
                    for item in payload['objects']:
                        oid = item['oid']; file = repository / '.git/lfs/objects' / oid[:2] / oid[2:4] / oid
                        if not file.exists():
                            self.send_error(404); return
                        objects.append({'oid': oid, 'size': file.stat().st_size, 'actions': {'download': {'href': owner.upstream_url + '/' + parts[0] + '/lfs/' + oid}}})
                    data = json.dumps({'transfer': 'basic', 'objects': objects}).encode()
                    self.send_response(200); self.send_header('content-type', 'application/vnd.git-lfs+json'); self.send_header('content-length', str(len(data))); self.end_headers(); self.wfile.write(data); return
                if len(parts) == 3 and parts[1] == 'lfs':
                    oid = parts[2]
                    if len(oid) != 64 or any(c not in '0123456789abcdef' for c in oid):
                        self.send_error(404); return
                    file = repository / '.git/lfs/objects' / oid[:2] / oid[2:4] / oid
                    data = file.read_bytes(); owner.evidence['upstream']['lfs_bytes'] += len(data)
                    self.send_response(200); self.send_header('content-length', str(len(data))); self.end_headers(); self.wfile.write(data); return
                env = dict(os.environ, GIT_PROJECT_ROOT=str(root), GIT_HTTP_EXPORT_ALL='1', PATH_INFO=path,
                           REQUEST_METHOD=self.command, QUERY_STRING=parsed.query, CONTENT_TYPE=self.headers.get('content-type', ''),
                           CONTENT_LENGTH=self.headers.get('content-length', '0'))
                env['GIT_CONFIG_COUNT']='1'; env['GIT_CONFIG_KEY_0']='http.receivepack'; env['GIT_CONFIG_VALUE_0']='false'
                data = self.rfile.read(int(self.headers.get('content-length', '0')))
                result = subprocess.run(['git', 'http-backend'], input=data, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env)
                headers, _, data = result.stdout.partition(b'\r\n\r\n')
                owner.evidence['upstream']['git_bytes'] += len(data); owner.evidence['upstream']['reads'] += 1
                self.send_response(200)
                for line in headers.decode().splitlines():
                    key, _, value = line.partition(':')
                    if key.lower() in ['content-type','cache-control']: self.send_header(key,value.strip())
                self.send_header('content-length', str(len(data))); self.end_headers(); self.wfile.write(data)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(str(Path(self.config['ARTIFACTORY_PKI_DIR'])/'server.pem'), str(Path(self.config['ARTIFACTORY_PKI_DIR'])/'server.key'))
        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.socket = context.wrap_socket(self.server.socket, server_side=True)
        self.upstream_url = f'https://localhost:{self.server.server_port}'
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.repository_url = self.upstream_url + '/app.git'
        self.check('fixture has no platform recipe', not (main/'artifactory.yaml').exists())

    def setup(self):
        self.fixtures()
        if self.args.tailnet_address:
            # Isolated test ingress. Production uses the existing TLS ingress;
            # this listener does not change it or expose the upstream fixture.
            class Proxy(http.server.BaseHTTPRequestHandler):
                protocol_version='HTTP/1.1'
                def log_message(self, *_): pass
                def do_HEAD(self): self.forward()
                def do_GET(self): self.forward()
                def do_POST(self): self.forward()
                def do_PATCH(self): self.forward()
                def do_DELETE(self): self.forward()
                def forward(self):
                    connection=http.client.HTTPConnection('127.0.0.1',38080,timeout=120)
                    try:
                        body=self.rfile.read(int(self.headers.get('content-length','0')))
                        headers={k:v for k,v in self.headers.items() if k.lower() not in ['connection','transfer-encoding']}
                        connection.request(self.command,self.path,body=body,headers=headers)
                        response=connection.getresponse();self.send_response(response.status)
                        chunked=response.getheader('content-length') is None and self.command!='HEAD'
                        for key,value in response.getheaders():
                            if key.lower() not in ['connection','transfer-encoding']: self.send_header(key,value)
                        if chunked:self.send_header('Transfer-Encoding','chunked')
                        self.end_headers()
                        if self.command != 'HEAD':
                            while chunk:=response.read1(65536):
                                if chunked:self.wfile.write(('%x\r\n'%len(chunk)).encode())
                                self.wfile.write(chunk)
                                if chunked:self.wfile.write(b'\r\n')
                                self.wfile.flush()
                            if chunked:self.wfile.write(b'0\r\n\r\n');self.wfile.flush()
                    except (OSError,http.client.HTTPException):
                        self.send_error(502)
                    finally:connection.close()
            self.proxy=http.server.ThreadingHTTPServer((self.args.tailnet_address,39444),Proxy)
            context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(str(Path(self.config['ARTIFACTORY_PKI_DIR'])/'server.pem'),str(Path(self.config['ARTIFACTORY_PKI_DIR'])/'server.key'))
            self.proxy.socket=context.wrap_socket(self.proxy.socket,server_side=True)
            threading.Thread(target=self.proxy.serve_forever,daemon=True).start()
        env = {key: os.environ[key] for key in ['PATH','HOME','LANG','TMPDIR'] if key in os.environ}
        env.update(self.config, GIT_SSL_CAINFO=str(Path(self.config['ARTIFACTORY_PKI_DIR'])/'ca.pem'))
        self.start('control', [str(ROOT/'target/debug/artifactory-control'),'serve'], env=env)
        self.wait(lambda: self.http.open(self.base+'/healthz',timeout=2).status==200)
        self.api('/auth/login', {'username':self.config['ARTIFACTORY_BOOTSTRAP_USERNAME'],'password':self.config['ARTIFACTORY_BOOTSTRAP_PASSWORD']})
        self.check('new session cookie', any(c.name=='artifactory_session' for c in self.jar))
        key = os.environ.get('ARTIFACTORY_ACCEPTANCE_MODEL_KEY') or os.environ.get('ANTHROPIC_AUTH_TOKEN') or os.environ.get('ANTHROPIC_API_KEY')
        if not key:
            raise RuntimeError('a real model key is required in ARTIFACTORY_ACCEPTANCE_MODEL_KEY or ANTHROPIC_API_KEY')
        model = self.api('/model-profiles', {'name':'Real acceptance model','api':os.environ.get('ARTIFACTORY_ACCEPTANCE_MODEL_API','anthropic-messages'),
            'base_url':os.environ.get('ARTIFACTORY_ACCEPTANCE_MODEL_URL',os.environ.get('ANTHROPIC_BASE_URL','https://api.anthropic.com')),
            'model':os.environ.get('ARTIFACTORY_ACCEPTANCE_MODEL',os.environ.get('ANTHROPIC_MODEL','claude-sonnet-4-5')),
            'api_key':key,'parameters':{'max_tokens':8192,'context_window':128000,'thinking_level':'off','requests_per_minute':int(os.environ.get('ARTIFACTORY_ACCEPTANCE_MODEL_RPM','3'))},'is_default':True})
        self.check('model key masked in API', key not in json.dumps(self.api('/model-profiles')))
        config = {'endpoint':self.grpc_endpoint,'ca_file':str(Path(self.config['ARTIFACTORY_PKI_DIR'])/'ca.pem'),
            'certificate_file':str(self.run/'worker.pem'),'key_file':str(self.run/'worker.key'),'name':'isolated-pi-worker'}
        path=self.run/'worker.json';path.write_text(json.dumps(config))
        token=self.api('/ai-enrollment-tokens',{})['token']
        result=self.start('worker-enroll',[self.args.node,str(ROOT/'ai-worker/dist/main.js'),'enroll','--config',str(path)],stdin=token,env=env)
        if result.wait(timeout=60)!=0:raise RuntimeError('worker enrollment failed; inspect worker-enroll.log')
        self.start('worker',[self.args.node,str(ROOT/'ai-worker/dist/main.js'),'--config',str(path)],env=env)
        self.agent_config=self.run/'agent.json'
        host_os={'Darwin':'darwin','Linux':'linux','Windows':'windows'}[platform.system()]
        arch='arm64' if platform.machine() in ['arm64','aarch64'] else 'amd64'
        self.host_os,self.arch=host_os,arch
        agent={'endpoint':self.grpc_endpoint,'name':'isolated-ai-agent','root':str(self.run/'agent'),
            'ca_file':str(Path(self.config['ARTIFACTORY_PKI_DIR'])/'ca.pem'),'allow_http_uploads':True,
            'signing_profiles':{'adhoc-test':{}} if host_os=='darwin' else {}}
        token=self.api('/enrollment-tokens',{'pool_id':'00000000-0000-0000-0000-000000000001'})['token']
        command([str(ROOT/'.local/ai-implementation/artifactory-agent'),'register','--config',str(self.agent_config),
                 '--endpoint',self.grpc_endpoint,'--ca',agent['ca_file'],'--root',agent['root'],'--name',agent['name'],'--allow-http-uploads'],
                env=dict(env,ARTIFACTORY_ENROLLMENT_TOKEN=token))
        agent=json.loads(self.agent_config.read_text());agent['signing_profiles']={'adhoc-test':{}} if host_os=='darwin' else {};agent['git_ca_file']=agent['ca_file']
        self.agent_config.write_text(json.dumps(agent))
        self.start('agent',[str(ROOT/'.local/ai-implementation/artifactory-agent'),'run','--config',str(self.agent_config)],env=env)
        self.wait(lambda:any(n['name']=='isolated-ai-agent' and n['online'] for n in self.api('/nodes')))
        node=next(n for n in self.api('/nodes') if n['name']=='isolated-ai-agent')
        self.api('/nodes/'+node['id'],{'pool_id':node['pool_id'],'slots':1,'labels':{},'draining':False,'revoked':False,
                                     'signing_profiles':['adhoc-test'] if host_os=='darwin' else []},'PATCH')
        self.project=self.api('/projects',{'name':'Zero intrusion AI acceptance','repository_url':self.repository_url,
            'pool_ids':['00000000-0000-0000-0000-000000000001'],'signing_profiles':['adhoc-test'] if host_os=='darwin' else []})['id']
        self.evidence.update(project_id=self.project,source_commit=self.source_commit)
        (self.run/'access.json').write_text(json.dumps({'base':self.base,'project_id':self.project,'schema':self.schema,
            'agent_config':str(self.agent_config),'cookies':{c.name:c.value for c in self.jar}}))

    def build(self,name,goal,parameters=None):
        build=self.api(f'/projects/{self.project}/builds',{'ref':'v-fixture-1','goal':goal,'target_requirements':[{'os':self.host_os,'arch':self.arch}],'parameters':parameters or {}})['id']
        self.evidence['builds'][name]={'id':build};self.save();print('BUILD',name,build,flush=True)
        previous=None
        until=time.monotonic()+1800
        while time.monotonic()<until:
            detail=self.api('/builds/'+build)
            status=detail['status']
            if status!=previous:print(name,status,flush=True);previous=status
            # Failed execution is transient while the scheduler enqueues a revision.
            if self.terminal(detail):
                self.evidence['builds'][name].update(status=status,revision_count=detail.get('revision_count'),commit=detail.get('commit_sha'))
                (self.run/(name+'-build.json')).write_text(json.dumps(detail,indent=2));self.save();return detail
            time.sleep(1)
        raise TimeoutError('build did not finish')

    def basic(self):
        goal='Build the native C executable for this host. Inspect the README, Git metadata and source tree, including nested submodules and LFS. Compile all required C files, verify the executable prints artifactory-ai-7, verify payload.dat has the expected LFS text and verify the v-fixture-1 tag exists. Use one external script in plan.scripts for these checks. Collect dist/app. '
        if self.host_os=='darwin':goal+='Use authorized adhoc-test signing profile, codesign --force --sign - dist/app, and verify codesign --verify --strict dist/app in separate sign and verify phases. '
        detail=self.build('basic',goal)
        self.check('real Pi-generated build succeeds',detail['status']=='succeeded')
        self.check('build pins annotated tag commit',detail['commit_sha']==self.source_commit)
        history=self.api('/builds/'+detail['id']+'/plans')
        self.check('real model calls recorded',sum(r['calls'] for r in history['runs'])>0)
        self.check('external scripts generated',bool(history['plans'][-1]['contents']['scripts']))
        self.check('file evidence recorded',bool(history['plans'][-1]['contents']['evidence']))
        artifacts=detail['artifacts'];self.check('artifact uploaded and verified',len(artifacts)>0)
        for artifact in artifacts:
            with self.http.open(self.base+'/api/v1/artifacts/'+artifact['id']+'/download',timeout=30) as response:data=response.read()
            self.check('artifact download hash '+artifact['path'],hashlib.sha256(data).hexdigest()==artifact['sha256'])
        refs={p.name:command(['git','-C',str(p),'show-ref']) for p in self.source_root.glob('*.git')}
        self.check('upstream refs unchanged',refs==self.upstream_before)
        self.check('upstream write requests absent',self.evidence['upstream']['writes']==0)
        self.check('nested submodules and LFS served',self.evidence['upstream']['lfs_bytes']>0 and len(self.api('/source-metrics'))==3)
        self.evidence['source_metrics']=self.api('/source-metrics');self.save()

    def full(self):
        clarification=self.build('clarification',
            'Before making a plan, use request_input to ask whether the user wants the executable or the library. '
            'After the user answers, build that choice from the C files described in README.md. Collect dist/app for the executable.')
        self.check('real model asks for missing product choice',clarification['status']=='needs_input')
        self.api('/builds/'+clarification['id']+'/clarifications',{'answer':'Build the executable, using all three C sources. Verify artifactory-ai-7 and collect dist/app.'})
        detail=self.wait_build(clarification['id'])
        self.check('real model continues after user clarification',detail['status']=='succeeded')

        goal=('Build two independent targets named stable and repaired for this same native host; both compile the executable described in README.md and collect dist/app. '
              'This is a controlled compiler-failure acceptance: in the INITIAL plan only, repaired must compile using the deliberately unsupported '
              'compiler flag -fartifactory-initial-failure so the compiler rejects that target. stable uses normal compiler flags. '
              'After reading the actual failed logs on revision, remove this unsupported flag from repaired and verify the executable instead. '
              'Keep stable exactly unchanged. This initial command failure is required to exercise repair; no source changes or clarification are needed.')
        revised=self.build('revision',goal)
        self.check('real model repairs failed target',revised['status']=='succeeded' and revised['revision_count']>=1)
        stable=next(j for j in revised['jobs'] if j['target']=='stable')
        repaired=next(j for j in revised['jobs'] if j['target']=='repaired')
        self.check('successful target retained across revision',len(stable['attempts'])==1 and stable['status']=='succeeded')
        self.check('compiler failure recorded before revision',any(a['status']=='failed' and a.get('failure_kind')=='execution' for a in repaired['attempts']))
        self.check('repaired target gets clean attempt',len(repaired['attempts'])>=2 and repaired['attempts'][-1]['status']=='succeeded')

        exhausted=self.build('exhausted',
            'Exercise a deliberately failing native build contract. In every plan and every revision the FIRST build step must execute '
            'the exact command exit 42. This is the fixed acceptance goal and must never be removed, bypassed, or changed to a success. '
            'Use one target fail-contract, declare dist/app as its artifact, and ground the plan in README.md. '
            'Submit the executable plan rather than asking questions. On revision read the actual failure logs and retain exit 42.')
        self.check('automatic revision budget exhausted after two revisions',exhausted['status']=='failed' and exhausted['revision_count']==2)
        attempts=exhausted['jobs'][0]['attempts']
        self.check('initial execution plus exactly two revised executions',len(attempts)==3)
        upstream=dict(self.evidence['upstream'])
        job=exhausted['jobs'][0]
        self.api('/jobs/'+job['id']+'/retry',{})
        retried=self.wait_build(exhausted['id'])
        latest=retried['jobs'][0]['attempts'][-1]
        self.check('manual retry retains original commit and plan',retried['commit_sha']==exhausted['commit_sha'] and latest['plan_revision_id']==attempts[-1]['plan_revision_id'] and len(retried['jobs'][0]['attempts'])==4)
        self.check('manual retry transfers no upstream Git or LFS payload',all(self.evidence['upstream'][k]==upstream[k] for k in ['git_bytes','lfs_bytes']))
        self.evidence['source_metrics']=self.api('/source-metrics')
        self.check('Agent reports source cache hits',any(m.get('agent_cache_hits',0)>0 for m in self.evidence['source_metrics']))
        self.check('upstream refs remain unchanged after all scenarios',{p.name:command(['git','-C',str(p),'show-ref']) for p in self.source_root.glob('*.git')}==self.upstream_before)
        self.check('no upstream write requests in any scenario',self.evidence['upstream']['writes']==0)
        key=os.environ.get('ARTIFACTORY_ACCEPTANCE_MODEL_KEY') or os.environ.get('ANTHROPIC_AUTH_TOKEN') or os.environ.get('ANTHROPIC_API_KEY')
        leaked=[]
        for root in [self.run/'agent',self.run/'control']:
            for path in root.rglob('*'):
                if path.is_file() and path.stat().st_size<8*1024*1024 and key.encode() in path.read_bytes():leaked.append(str(path.relative_to(self.run)))
        for path in self.run.glob('*.log'):
            if key.encode() in path.read_bytes():leaked.append(path.name)
        self.check('model key absent from task directories and logs',not leaked)
        self.save()

    def wait_build(self, build):
        until=time.monotonic()+1800
        while time.monotonic()<until:
            detail=self.api('/builds/'+build)
            if self.terminal(detail):
                return detail
            time.sleep(1)
        raise TimeoutError('build did not finish')

    def terminal(self, detail):
        if detail['status'] in ['succeeded','needs_input','canceled','lost']:
            return True
        if detail['status'] != 'failed':
            return False
        if not detail.get('current_plan_id') or detail.get('revision_count',0)>=2:
            return True
        failed=[j for j in detail.get('jobs',[]) if j['status']=='failed']
        if failed and all(j.get('attempts') and j['attempts'][-1].get('failure_kind','execution')!='execution' for j in failed):
            return True
        runs=self.api('/builds/'+detail['id']+'/plans')['runs']
        return bool(runs and runs[-1]['state']=='failed')

    def close(self):
        for process in reversed(self.processes):
            if process.poll() is None:process.terminate()
        for process in reversed(self.processes):
            try:process.wait(timeout=15)
            except subprocess.TimeoutExpired:process.kill();process.wait()
        if hasattr(self,'server'):self.server.shutdown()
        if hasattr(self,'proxy'):self.proxy.shutdown()
        self.save()


def main():
    parser=argparse.ArgumentParser();parser.add_argument('--node',default=shutil.which('node'));parser.add_argument('--keep-alive',action='store_true');parser.add_argument('--full',action='store_true');parser.add_argument('--tailnet-address')
    args=parser.parse_args();run=Acceptance(args);print('EVIDENCE',run.run,flush=True)
    try:
        try:
            run.setup();run.basic()
            if args.full:run.full()
        except Exception as error:
            run.evidence['failure']=str(error);run.save()
            if not args.keep_alive:raise
            print('AI ACCEPTANCE NEEDS ATTENTION',str(error),flush=True)
        else:
            print('AI ACCEPTANCE READY',flush=True)
        if args.keep_alive:
            while True:time.sleep(1)
    finally:run.close()


if __name__=='__main__':
    main()
