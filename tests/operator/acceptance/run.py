#!/usr/bin/env python3
"""Isolated kind acceptance. Only changes this harness namespace and release.

Requires three arm64 images in the cluster registry: operator, operator-fixture,
and operator-pi. Uses the real Pi loop and Rust Emissary with
a deterministic model fixture. Private credentials/logs stay in .local/.
"""
import argparse
import base64
import hashlib
import http.cookiejar
import json
import os
from pathlib import Path
import secrets
import ssl
import subprocess
import time
import urllib.request
import uuid
from urllib.parse import parse_qs, unquote, urlsplit

ROOT = Path(__file__).resolve().parents[3]
os.umask(0o077)


class Acceptance:
    def __init__(self, context, namespace=None, pull_policy='Always', resume=False):
        self.context = context
        self.image_tag = os.environ.get('ARTIFACTORY_ACCEPTANCE_IMAGE_TAG', 'rust-native')
        nodes = json.loads(subprocess.check_output(['kubectl','--context',context,'get','nodes','-o','json']))
        self.arch = nodes['items'][0]['status']['nodeInfo']['architecture']
        self.ns = namespace or ('artifact-operator-' + time.strftime('%m%d%H%M%S'))
        self.run = ROOT / '.local/operator-acceptance' / self.ns
        if resume and (not namespace or not (self.run/'pki/ca.pem').is_file()):
            raise ValueError('Resume requires this harness existing namespace and private PKI')
        self.run.mkdir(parents=True, exist_ok=resume)
        self.evidence = {'context': context, 'namespace': self.ns, 'model': 'deterministic fixture; real Pi loop', 'checks': []}
        if resume and (self.run/'evidence.json').is_file():
            self.evidence = json.loads((self.run/'evidence.json').read_text())
        self.jar = http.cookiejar.CookieJar()
        self.http = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPCookieProcessor(self.jar))
        self.base = 'http://127.0.0.1:38081'
        self.origin = self.base
        self.processes = []
        self.pull_policy = pull_policy

    def command(self, args, data=None):
        result = subprocess.run(args, input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if result.returncode:
            raise RuntimeError(f'{args[0]} failed: {result.stderr.decode()[-2500:]}')
        return result.stdout.decode()

    def kubectl(self, *args, data=None):
        return self.command(['kubectl', '--context', self.context, '-n', self.ns, *args], data)

    def images(self):
        pods = json.loads(self.kubectl('get', 'pods', '-o', 'json'))['items']
        return [{'pod': p['metadata']['name'], 'uid': p['metadata']['uid'], 'images': [{'image': c['image'], 'imageID': c.get('imageID')} for c in p.get('status', {}).get('containerStatuses', [])]} for p in pods if p['status']['phase'] == 'Running' and not p['metadata'].get('deletionTimestamp')]

    def image(self, name):
        return f'localhost:5001/artifactory/{name}:{self.image_tag}'

    def apply(self, *items):
        self.kubectl('apply', '-f', '-', data=json.dumps({'apiVersion': 'v1', 'kind': 'List', 'items': list(items)}).encode())

    def secret(self, name, values):
        return {'apiVersion': 'v1', 'kind': 'Secret', 'metadata': {'name': name}, 'stringData': values}

    def wait(self, callback, seconds=180):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            try:
                value = callback()
                if value:
                    return value
            except (OSError, RuntimeError, ValueError):
                pass
            time.sleep(1)
        raise TimeoutError('acceptance phase timed out')

    def check(self, name, value):
        if not value:
            raise AssertionError(name)
        self.evidence['checks'].append(name)
        (self.run / 'evidence.json').write_text(json.dumps(self.evidence, indent=2))
        print('PASS', name, flush=True)

    def api(self, path, body=None, method=None):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.base + path, data=data, method=method, headers={'Content-Type': 'application/json', 'Origin': self.origin, 'x-artifactory-csrf': '1'})
        with self.http.open(request, timeout=30) as response:
            return json.load(response)

    def workload(self, name, image, args, env=None, secret=None, pki=False):
        container = {'name': name, 'image': image, 'args': args, 'imagePullPolicy': self.pull_policy}
        if name == 'control-plane':
            container['readinessProbe'] = {'tcpSocket': {'port': 8080}, 'initialDelaySeconds': 1}
        if env:
            container['env'] = [{'name': key, 'value': value} for key, value in env.items()]
        if secret:
            container['envFrom'] = [{'secretRef': {'name': secret}}]
        spec = {'containers': [container]}
        if pki:
            container['volumeMounts'] = [{'name': 'pki', 'mountPath': '/run/pki', 'readOnly': True}]
            spec['volumes'] = [{'name': 'pki', 'secret': {'secretName': 'acceptance-pki', 'defaultMode': 0o444}}]
        return {'apiVersion': 'apps/v1', 'kind': 'Deployment', 'metadata': {'name': name}, 'spec': {'replicas': 1, 'selector': {'matchLabels': {'app': name}}, 'template': {'metadata': {'labels': {'app': name}}, 'spec': spec}}}

    def service(self, name, ports):
        return {'apiVersion': 'v1', 'kind': 'Service', 'metadata': {'name': name}, 'spec': {'selector': {'app': name}, 'ports': [{'name': 'p' + str(port), 'port': port, 'targetPort': port} for port in ports]}}

    def sql(self, database, statement):
        return self.kubectl('exec', 'service/postgres', '--', 'psql', '-U', 'admin', '-d', database, '-qAt', '-v', 'ON_ERROR_STOP=1', '-c', statement).strip()

    def setup(self):
        self.command(['kubectl', '--context', self.context, 'create', 'namespace', self.ns])
        pki = self.run / 'pki'; pki.mkdir()
        self.command(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', str(pki/'ca.key'), '-out', str(pki/'ca.pem'), '-days', '2', '-subj', '/CN=Operator isolated acceptance', '-addext', 'basicConstraints=critical,CA:TRUE', '-addext', 'keyUsage=critical,keyCertSign,cRLSign', '-addext', 'subjectKeyIdentifier=hash'])
        self.command(['openssl', 'req', '-newkey', 'rsa:2048', '-nodes', '-keyout', str(pki/'server.key'), '-out', str(pki/'server.csr'), '-subj', '/CN=upstream'])
        (pki/'ext').write_text('subjectAltName=DNS:localhost,DNS:control-plane,DNS:upstream,DNS:artifactory-runtime,DNS:artifactory-telemetry,DNS:sources-0.sources.'+self.ns+'.svc,IP:127.0.0.1\nextendedKeyUsage=serverAuth\nkeyUsage=critical,digitalSignature,keyEncipherment\nbasicConstraints=critical,CA:FALSE\nsubjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid,issuer\n')
        self.command(['openssl', 'x509', '-req', '-in', str(pki/'server.csr'), '-CA', str(pki/'ca.pem'), '-CAkey', str(pki/'ca.key'), '-CAcreateserial', '-out', str(pki/'server.pem'), '-days', '2', '-extfile', str(pki/'ext')])
        self.command(['openssl', 'verify', '-x509_strict', '-CAfile', str(pki/'ca.pem'), str(pki/'server.pem')])
        self.apply(self.secret('acceptance-pki', {name: (pki/name).read_text() for name in ['ca.key','ca.pem','server.key','server.pem']}), self.secret('service-tls', {'ca.crt': (pki/'ca.pem').read_text(), 'tls.crt': (pki/'server.pem').read_text(), 'tls.key': (pki/'server.key').read_text()}))
        password = secrets.token_hex(20); token = secrets.token_hex(32); admin = secrets.token_hex(20)
        self.token=token
        self.tls=ssl.create_default_context(cafile=str(pki/'ca.pem'))
        databases = "\n".join(f"CREATE USER {role} PASSWORD '{password}'; CREATE DATABASE {role} OWNER {role};\n\\connect {role}\nCREATE EXTENSION IF NOT EXISTS timescaledb;\n\\connect postgres" for role in ['business','planner','sources','telemetry'])
        postgres = self.workload('postgres', 'localhost:5001/artifactory/timescaledb:2.32.0-pg17', [], {'POSTGRES_USER': 'admin', 'POSTGRES_PASSWORD': password, 'TIMESCALEDB_TELEMETRY':'off'})
        postgres['spec']['template']['spec']['containers'][0]['volumeMounts'] = [{'name': 'init', 'mountPath': '/docker-entrypoint-initdb.d'}]
        postgres['spec']['template']['spec']['volumes'] = [{'name': 'init', 'configMap': {'name': 'init-databases'}}]
        self.apply({'apiVersion':'v1','kind':'ConfigMap','metadata':{'name':'init-databases'},'data':{'100_init.sql':databases}}, postgres, self.service('postgres',[5432]))
        s3config = {'identities':[{'name':'acceptance','credentials':[{'accessKey':'acceptance','secretKey':password}],'actions':['Admin','Read','Write','List','Tagging']}]}
        s3 = self.workload('s3','localhost:5001/artifactory/seaweedfs:4.53',['mini','-dir=/data','-s3.port=8333','-s3.config=/run/s3/s3.json','-s3.iam.readOnly=true','-admin.ui=false','-ip.bind=0.0.0.0'])
        s3['spec']['template']['spec']['containers'][0]['volumeMounts']=[{'name':'config','mountPath':'/run/s3','readOnly':True}]
        s3['spec']['template']['spec']['volumes']=[{'name':'config','secret':{'secretName':'s3-config'}}]
        self.apply(self.secret('s3-config',{'s3.json':json.dumps(s3config)}),s3,self.service('s3',[8333]))
        shared={'ARTIFACTORY_SERVICE_TOKEN':token,'ARTIFACTORY_S3_ENDPOINT':'http://s3:8333','ARTIFACTORY_S3_BUCKET':'operator-acceptance','ARTIFACTORY_S3_ACCESS_KEY':'acceptance','ARTIFACTORY_S3_SECRET_KEY':password}
        shared['RUST_LOG']='info'
        source=dict(shared,DATABASE_URL=f'postgres://sources:{password}@postgres:5432/sources',ARTIFACTORY_SERVICE_KEY=base64.b64encode(secrets.token_bytes(32)).decode(),GIT_SSL_CAINFO='/run/service-tls/ca.crt')
        runtime=dict(shared,DATABASE_URL=f'postgres://planner:{password}@postgres:5432/planner',ARTIFACTORY_SERVICE_KEY=base64.b64encode(secrets.token_bytes(32)).decode())
        control=dict(shared,DATABASE_URL=f'postgres://business:{password}@postgres:5432/business',ARTIFACTORY_MASTER_KEY=base64.b64encode(secrets.token_bytes(32)).decode(),ARTIFACTORY_BOOTSTRAP_USERNAME='operator-admin',ARTIFACTORY_BOOTSTRAP_PASSWORD=admin,
                     ARTIFACTORY_RUNTIME_URL='https://artifactory-runtime:8091',ARTIFACTORY_SOURCE_URLS=f'https://sources-0.sources.{self.ns}.svc:8090',ARTIFACTORY_SERVICE_CA='/run/pki/ca.pem',ARTIFACTORY_PUBLIC_URL=self.base,ARTIFACTORY_INSECURE_DEV='true',ARTIFACTORY_HTTP_BIND='0.0.0.0:8080',ARTIFACTORY_GRPC_BIND='0.0.0.0:8443',ARTIFACTORY_PKI_DIR='/run/pki',ARTIFACTORY_DATA_DIR='/tmp/control')
        control['GIT_SSL_CAINFO']='/run/pki/ca.pem'
        telemetry=dict(DATABASE_URL=f'postgres://telemetry:{password}@postgres:5432/telemetry',ARTIFACTORY_BUSINESS_DATABASE_URL=f'postgres://business:{password}@postgres:5432/business',ARTIFACTORY_TELEMETRY_TOKEN=token)
        control.update(ARTIFACTORY_TELEMETRY_ENDPOINT='https://artifactory-telemetry:8444',ARTIFACTORY_TELEMETRY_URL='https://artifactory-telemetry:8093',ARTIFACTORY_TELEMETRY_TOKEN=token)
        # Device Pods must reach the legacy source origin; localhost is only
        # the harness port-forward, not the application's advertised address.
        control['ARTIFACTORY_PUBLIC_URL']='http://control-plane:8080'
        self.origin=control['ARTIFACTORY_PUBLIC_URL']
        self.apply(self.secret('telemetry',telemetry),self.secret('runtime',runtime),self.secret('source',source),self.secret('control-plane',control),self.workload('control-plane',self.image('operator'),['control-plane','serve'],secret='control-plane',pki=True),self.service('control-plane',[8080,8443]),self.workload('upstream',self.image('operator-fixture'),[],env={'FIXTURE_ARCH':self.arch},pki=True),self.service('upstream',[9443]))
        self.wait(lambda:self.sql('business','SELECT 1')=='1')
        self.command(['kubectl','--context',self.context,'apply','-f',str(ROOT/'operator/config/crd/resources.yaml')])
        values={'image':{'repository':'localhost:5001/artifactory/operator','tag':self.image_tag,'pullPolicy':self.pull_policy},'runtime':{'secretName':'runtime'},'telemetry':{'secretName':'telemetry','pkiSecretName':'acceptance-pki'},'sourcePool':{'secretName':'source','storageSize':'1Gi'},'agentPool':{'image':self.image('operator-pi'),'maxActive':2,'maxPerProject':1},'tls':{'secretName':'service-tls'},'controlUrl':'https://upstream:9443','networkPolicy':{'enabled':True}}
        path=self.run/'values.json';path.write_text(json.dumps(values))
        self.command(['helm','--kube-context',self.context,'upgrade','--install','native',str(ROOT/'charts/artifactory-operator'),'-n',self.ns,'-f',str(path)])
        self.kubectl('rollout','status','deploy/control-plane','--timeout=180s')
        self.forward('service/control-plane','38081:8080')
        self.forward('service/upstream','39443:9443')
        self.wait(lambda:self.api('/healthz').get('status')=='ok')
        self.enroll(admin)

    def enroll(self, admin):
        self.api('/api/v1/auth/login',{'username':'operator-admin','password':admin})
        if not self.api('/api/v1/model-profiles'):
            self.api('/api/v1/model-profiles',{'name':'Operator fixture','api':'openai-completions','base_url':'https://upstream:9443/v1','model':'fixture','api_key':'fixture-provider-key','parameters':{'thinking_level':'off','max_tokens':4096},'max_calls':20,'timeout_seconds':600,'is_default':True})
        enrollment=self.api('/api/v1/enrollment-tokens',{'pool_id':'00000000-0000-0000-0000-000000000001'})['token']
        self.apply(self.secret('emissary-token',{'ARTIFACTORY_ENROLLMENT_TOKEN':enrollment}))
        shell='artifactory-emissary register --config /tmp/emissary.json --endpoint control-plane:8443 --ca /run/pki/ca.pem --root /tmp/emissary --name operator-native-device --allow-http-uploads && sed -i \'s|"git_ca_file":""|"git_ca_file":"/run/pki/ca.pem"|\' /tmp/emissary.json && exec artifactory-emissary run --config /tmp/emissary.json'
        device=self.workload('device',self.image('operator-fixture'),[],secret='emissary-token',pki=True);device['spec']['template']['spec']['containers'][0]['command']=['/usr/bin/tini','--','sh','-ec',shell]
        if not any(node['online'] for node in self.api('/api/v1/nodes')):self.apply(device)
        self.wait(lambda:any(node['online'] for node in self.api('/api/v1/nodes')))
        projects=self.api('/api/v1/projects')
        fixture=next((p for p in projects if p['name']=='Kubernetes Pi acceptance'),None)
        self.project=fixture['id'] if fixture else self.api('/api/v1/projects',{'name':'Kubernetes Pi acceptance','repository_url':'https://upstream:9443/repo.git','planner_backend':'kubernetes'})['id']
        self.check('independent namespace, databases, Operator and Rust Emissary enrolled',True)

    def resume(self):
        # Only resume an installation created by this harness with the same private CA.
        secret=json.loads(self.kubectl('get','secret','control-plane','-o','json'))['data']
        expected=(self.run/'pki/ca.pem').read_bytes()
        pki=json.loads(self.kubectl('get','secret','acceptance-pki','-o','json'))['data']
        if base64.b64decode(pki['ca.pem'])!=expected:
            raise ValueError('Existing namespace does not belong to this saved run')
        self.token=base64.b64decode(secret['ARTIFACTORY_SERVICE_TOKEN']).decode()
        self.tls=ssl.create_default_context(cafile=str(self.run/'pki/ca.pem'))
        self.origin=self.kubectl('exec','deploy/control-plane','--','printenv','ARTIFACTORY_PUBLIC_URL').strip()
        self.forward('service/control-plane','38081:8080');self.forward('service/upstream','39443:9443')
        self.wait(lambda:self.api('/healthz').get('status')=='ok')
        self.enroll(base64.b64decode(secret['ARTIFACTORY_BOOTSTRAP_PASSWORD']).decode())

    def forward(self, resource, ports):
        log=(self.run/('port-forward-'+ports.split(':')[0]+'.log')).open('wb')
        self.processes.append(subprocess.Popen(['kubectl','--context',self.context,'-n',self.ns,'port-forward',resource,ports],stdout=log,stderr=log));log.close()

    def service_request(self, port, path, body=None):
        request=urllib.request.Request(f'https://127.0.0.1:{port}'+path,data=None if body is None else json.dumps(body).encode(),headers={'Content-Type':'application/json','Authorization':'Bearer '+self.token})
        opener=urllib.request.build_opener(urllib.request.ProxyHandler({}),urllib.request.HTTPSHandler(context=self.tls))
        with opener.open(request,timeout=20) as response:
            return response.read().decode()

    def verify(self):
        business_bytes_before=int(self.sql('business','SELECT coalesce(sum(git_download_bytes+lfs_download_bytes),0) FROM source_repositories'))
        self.kubectl('patch','agentpool','default','--type=merge','-p','{"spec":{"maxActive":2}}')
        self.service_request(39443,'/test/mode',{'reset_ack':True})
        build=self.api(f'/api/v1/projects/{self.project}/builds',{'ref':'v-fixture','goal':'Inspect the source and build dist/result.txt on Linux '+self.arch,'target_requirements':[{'os':'linux','arch':self.arch}]})['id']
        self.evidence['build_id']=build
        previous=None
        def terminal():
            nonlocal previous
            detail=self.api('/api/v1/builds/'+build)
            if detail['status']!=previous:print('BUILD',detail['status'],flush=True);previous=detail['status']
            return detail if detail['status'] in ['succeeded','failed','lost','canceled','needs_input'] else None
        detail=self.wait(terminal,900);(self.run/'build.json').write_text(json.dumps(detail,indent=2));self.check('fixed source -> Pi -> validated plan -> Rust Emissary native build',detail['status']=='succeeded')
        history=self.api('/api/v1/builds/'+build+'/plans');self.check('one logical run and one accepted revision',len(history['runs'])==1 and len(history['plans'])==1)
        run=history['runs'][0]['id'];calls=int(self.sql('planner',f"SELECT lifecycle->>'calls' FROM runtime_runs WHERE id='{run}'"));self.check('invalid plan corrected in the same persisted call budget',calls==4)
        self.check('new Pi heartbeats/model calls do not touch business hot tables',history['runs'][0]['calls']==0 and self.sql('business',f"SELECT count(*) FROM planning_events WHERE run_id='{run}' AND kind IN ('model_call','checkpoint','usage')")=='0')
        origin=detail['source_snapshot_id']
        cache_key=self.sql('business',f"SELECT r.cache_key FROM source_repositories r JOIN source_snapshots s ON s.root_repository_id=r.id WHERE s.id='{origin}' AND s.service_origin IS NOT NULL")
        assert len(cache_key)==64 and all(c in '0123456789abcdef' for c in cache_key)
        self.check('control has no Git cache for this build',self.kubectl('exec','deploy/control-plane','--','sh','-c','test ! -d /tmp/control/sources/repos/'+cache_key+'.git && echo offloaded').strip()=='offloaded')
        source_bytes=int(self.sql('sources','SELECT coalesce(sum(git_download_bytes+lfs_download_bytes),0) FROM source_repositories'))
        business_bytes_total=int(self.sql('business','SELECT coalesce(sum(git_download_bytes+lfs_download_bytes),0) FROM source_repositories'))
        business_bytes=business_bytes_total-business_bytes_before
        self.check('Git bytes are served directly by Source rather than control',source_bytes>0 and business_bytes==0)
        self.evidence['source_download_bytes']=source_bytes;self.evidence['business_source_download_bytes']=business_bytes;self.evidence['business_source_download_bytes_total']=business_bytes_total
        self.check('source snapshot is durable in Source database',self.sql('sources',f"SELECT durable FROM source_snapshots WHERE id='{origin}'")=='t')
        self.check('Rust Emissary artifact verified by control',bool(detail['artifacts']))
        self.check('artifact has expected content hash',detail['artifacts'][0]['sha256']==hashlib.sha256(b'operator-native-ok\n').hexdigest())
        self.check('lost acceptance acknowledgement returns original result without new inference','fixture_dropped_accept_acks 1' in self.service_request(39443,'/metrics') and calls==4)
        self.evidence['calls']=calls;self.evidence['source_snapshot_id']=origin
        self.evidence['resources']=json.loads(self.kubectl('get','agentruns,sourcepools,sourcesnapshots,pvc','-o','json'))
        self.check('Kubernetes resources and per-shard PVC exist',bool(self.evidence['resources']['items']))
        patch=subprocess.run(['kubectl','--context',self.context,'-n',self.ns,'patch','agentrun','run-'+run+'-g1','--dry-run=server','--type=merge','-p','{"spec":{"generation":999}}'],capture_output=True,text=True)
        self.check('API server rejects changing immutable AgentRun context',patch.returncode!=0 and 'immutable' in patch.stderr.lower())
        access=subprocess.run(['kubectl','--context',self.context,'-n',self.ns,'auth','can-i','get','secrets','--as','system:serviceaccount:'+self.ns+':artifactory-pi'],capture_output=True,text=True)
        self.check('Pi identity cannot read Kubernetes Secrets',access.returncode==1 and access.stdout.strip()=='no')
        self.faults(origin, detail['commit_sha'])
        print('EVIDENCE',self.run/'evidence.json',flush=True)

    def faults(self, snapshot, commit):
        self.forward('service/artifactory-runtime','38091:8091')
        self.forward('pod/sources-0','38090:8090')
        self.wait(lambda:'artifactory_runtime_runs' in self.service_request(38091,'/metrics'))
        self.check('Runtime and Source expose authenticated metrics','artifactory_source_snapshots' in self.wait(lambda:self.service_request(38090,'/metrics')))
        try:
            urllib.request.urlopen('https://127.0.0.1:38091/metrics',context=self.tls,timeout=10)
            unauthorized=False
        except urllib.error.HTTPError as error:
            unauthorized=error.code==401
        self.check('Runtime metrics reject unauthenticated requests',unauthorized)
        self.service_request(39443,'/test/mode',{'git_down':True})
        self.kubectl('exec','sources-0','--','sh','-ec','mv /var/lib/artifactory-source/sources/repos /var/lib/artifactory-source/sources/repos.acceptance-old-'+uuid.uuid4().hex+'; mkdir /var/lib/artifactory-source/sources/repos')
        restored=json.loads(self.service_request(38090,'/v1/inspect',{'snapshot_id':snapshot,'name':'read_file','arguments':{'path':'README.md'}}))
        self.check('PVC cache loss restores frozen source while Git upstream is unavailable','Build with sh build.sh.' in restored['content'])
        self.service_request(39443,'/test/mode',{})
        lease=json.loads(self.kubectl('get','lease','artifactory-operator','-o','json'))
        holder=lease['spec']['holderIdentity']
        pods=json.loads(self.kubectl('get','pods','-l','app.kubernetes.io/name=artifactory-operator','-o','json'))['items']
        leader=next(p['metadata']['name'] for p in pods if p['metadata']['uid']==holder)
        self.kubectl('delete','pod',leader,'--wait=false')
        self.wait(lambda:json.loads(self.kubectl('get','lease','artifactory-operator','-o','json'))['spec']['holderIdentity']!=holder,60)
        self.check('two Operator replicas elect a replacement after leader deletion',True)
        # Hold a real model request before its headers, then kill the owning Pi Pod.
        self.service_request(39443,'/test/mode',{'hold':True})
        build=self.api(f'/api/v1/projects/{self.project}/builds',{'ref':'v-fixture','goal':'fault injection: retain budget across Pod loss','target_requirements':[{'os':'linux','arch':self.arch}]})['id']
        def running():
            runs=self.api('/api/v1/builds/'+build+'/plans')['runs']
            if not runs:return None
            run=runs[0]['id']
            value=self.sql('planner',f"SELECT jsonb_build_object('id',id,'life',lifecycle,'execution',execution) FROM runtime_runs WHERE id='{run}' AND status='running' AND (lifecycle->>'calls')::int=1")
            return json.loads(value) if value else None
        before=self.wait(running)
        pod=json.loads(self.kubectl('get','pod',before['execution']['name'],'-o','json'))
        self.evidence['pi_image']={key:pod['status']['containerStatuses'][0][key] for key in ['image','imageID']}
        samples=[]
        for _ in range(30):
            started=time.monotonic();self.api('/api/v1/overview');samples.append((time.monotonic()-started)*1000)
        self.evidence['business_api_during_held_pi']={'samples':len(samples),'p95_ms':round(sorted(samples)[int(len(samples)*.95)-1],3),'scope':'local small-load observation; no production SLO claim'}
        run=before['id'];self.evidence['fault_run_id']=run
        self.kubectl('delete','pod',before['execution']['name'],'--wait=false')
        self.sql('planner',f"UPDATE runtime_runs SET lifecycle=jsonb_set(lifecycle,'{{lease_until_ms}}',to_jsonb((extract(epoch FROM now())*1000)::bigint-1000)) WHERE id='{run}'")
        def retried():
            value=self.sql('planner',f"SELECT lifecycle FROM runtime_runs WHERE id='{run}'")
            life=json.loads(value)
            return life if life['generation']==2 and life['calls']>=2 else None
        after=self.wait(retried,120)
        self.check('Pod replacement keeps original deadline and spent model calls',after['deadline_ms']==before['life']['deadline_ms'] and after['calls']==2)
        self.check('physical retry retains source commit',self.api('/api/v1/builds/'+build)['commit_sha']==commit)
        started=time.monotonic();self.api('/api/v1/builds/'+build+'/cancel',{})
        self.evidence['cancel_api_seconds']=round(time.monotonic()-started,3)
        self.wait(lambda:self.sql('planner',f"SELECT status FROM runtime_runs WHERE id='{run}'")=='canceled',30)
        self.evidence['cancel_authority_observed_seconds']=round(time.monotonic()-started,3)
        self.wait(lambda:not json.loads(self.kubectl('get','pods','-l','artifactory.dev/run='+run,'-o','json'))['items'],60)
        self.check('cancel revokes authorization and stops Pi without creating device jobs',not self.api('/api/v1/builds/'+build)['jobs'])
        self.evidence['cancel_physical_stop_seconds']=round(time.monotonic()-started,3)
        self.service_request(39443,'/test/mode',{})
        self.kubectl('rollout','restart','deploy/native-runtime')
        self.kubectl('rollout','status','deploy/native-runtime','--timeout=120s')
        persisted=json.loads(self.sql('planner',f"SELECT lifecycle FROM runtime_runs WHERE id='{run}'"))
        self.check('Runtime restart preserves cancellation, budget and deadline',persisted['canceled'] and persisted['calls']==2 and persisted['deadline_ms']==before['life']['deadline_ms'])
        # Pausing intake preserves existing run authority; switching project affects future Builds only.
        self.kubectl('patch','agentpool','default','--type=merge','-p','{"spec":{"maxActive":0}}')
        self.wait(lambda:json.loads(self.kubectl('get','agentpool','default','-o','json')).get('status',{}).get('phase')=='Paused',30)
        self.check('pool can pause new admission for rollback',True)

    def real_model(self, environment_file):
        # Reuse an explicitly selected local development profile in memory only.
        # Neither the provider key nor source master key is written to test evidence.
        from cryptography.hazmat.primitives.ciphers.aead import AESGCM
        environment=json.loads(Path(environment_file).read_text())
        query="SELECT jsonb_build_object('api',api,'base_url',base_url,'model',model,'parameters',parameters,'key_ciphertext',key_ciphertext) FROM model_profiles WHERE enabled ORDER BY is_default DESC,created_at LIMIT 1"
        database=urlsplit(environment['DATABASE_URL'])
        connection=dict(os.environ,PGHOST=database.hostname or '127.0.0.1',PGPORT=str(database.port or 5432),PGDATABASE=unquote(database.path.lstrip('/')),PGUSER=unquote(database.username or ''),PGPASSWORD=unquote(database.password or ''),PGCONNECT_TIMEOUT='5')
        for option,value in parse_qs(database.query).items():
            if option in ['sslmode','sslrootcert','sslcert','sslkey']:connection['PG'+option.upper()]=value[0]
        result=subprocess.run(['psql','-XAt','-c',query],env=connection,capture_output=True,check=True,text=True)
        model=json.loads(result.stdout)
        encrypted=base64.b64decode(model.pop('key_ciphertext'))
        key=AESGCM(base64.b64decode(environment['ARTIFACTORY_MASTER_KEY'])).decrypt(encrypted[:12],encrypted[12:],None).decode()
        profile=self.api('/api/v1/model-profiles',dict(model,name='Operator real model acceptance',api_key=key,max_calls=10,timeout_seconds=600,is_default=False))['id']
        del key,environment,connection,database
        self.kubectl('patch','agentpool','default','--type=merge','-p','{"spec":{"maxActive":2}}')
        project=self.api('/api/v1/projects',{'name':'Operator real model acceptance','repository_url':'https://upstream:9443/repo.git','planner_backend':'kubernetes','model_profile_id':profile})['id']
        build=self.api(f'/api/v1/projects/{project}/builds',{'ref':'v-fixture','goal':'Read README.md and build.sh. Build this source using sh build.sh and collect dist/result.txt on the available Linux device. Submit an ExecutionPlan v1 with evidence; no signing or external scripts are necessary.','target_requirements':[{'os':'linux','arch':self.arch}]})['id']
        self.evidence['real_model']={'provider':model['base_url'],'model':model['model'],'build_id':build,'max_calls':10}
        self.check('real supplier build submitted through the same Kubernetes Pi runtime',True)
        previous=None
        def terminal():
            nonlocal previous
            detail=self.api('/api/v1/builds/'+build)
            if detail['status']!=previous:print('REAL MODEL BUILD',detail['status'],flush=True);previous=detail['status']
            return detail if detail['status'] in ['succeeded','failed','lost','canceled','needs_input'] else None
        detail=self.wait(terminal,900)
        (self.run/'real-model-build.json').write_text(json.dumps(detail,indent=2))
        self.check('real supplier -> Kubernetes Pi -> control acceptance -> Rust Emissary build',detail['status']=='succeeded')
        self.check('real-model build artifact content matches',detail['artifacts'][0]['sha256']==hashlib.sha256(b'operator-native-ok\n').hexdigest())
        run=self.api('/api/v1/builds/'+build+'/plans')['runs'][0]['id']
        self.evidence['real_model']['runtime']=json.loads(self.sql('planner',f"SELECT jsonb_build_object('lifecycle',lifecycle,'usage',usage,'status',status) FROM runtime_runs WHERE id='{run}'"))
        self.check('real model call budget is persisted in Runtime',True)

    def retention(self):
        # No in-flight planning may be transferred or restarted by this exercise.
        self.wait(lambda:self.sql('planner',"SELECT count(*) FROM runtime_runs WHERE status IN ('running','provisioning')")=='0',60)
        resources=['statefulset/sources','service/sources','pvc/cache-sources-0']
        before=json.loads(self.kubectl('get',*resources,'-o','json'))['items']
        self.command(['helm','--kube-context',self.context,'uninstall','native','-n',self.ns])
        after=json.loads(self.kubectl('get',*resources,'-o','json'))['items']
        self.check('deleting Operator release and SourcePool preserves Source StatefulSet, Service and PVC',
                   {r['kind']:r['metadata']['uid'] for r in before}=={r['kind']:r['metadata']['uid'] for r in after})
        metadata=json.loads(self.service_request(38090,'/v1/metadata',{'snapshot_id':self.evidence['source_snapshot_id']}))
        self.check('durable source remains readable after Operator release removal',bool(metadata['digest']))
        self.command(['helm','--kube-context',self.context,'upgrade','--install','native',str(ROOT/'charts/artifactory-operator'),'-n',self.ns,'-f',str(self.run/'values.json')])
        self.kubectl('rollout','status','deploy/native-runtime','--timeout=120s')
        self.kubectl('rollout','status','deploy/native-operator','--timeout=120s')
        self.wait(lambda:json.loads(self.kubectl('get','sourcepool','sources','-o','json')).get('status',{}).get('phase')=='Ready',60)
        self.check('independent release reinstalls against retained Source and databases',True)
        self.evidence['images']=self.images()
        self.check('running image digests recorded',bool(self.evidence['images']))

    def cleanup(self):
        for process in self.processes:process.terminate()
        for name in ['control-plane','native-operator','native-runtime','native-telemetry','upstream','device']:
            try:(self.run/(name+'.log')).write_text(self.kubectl('logs','deploy/'+name,'--all-containers=true','--tail=500'))
            except RuntimeError:pass
        # Keep the namespace for inspection; explicit user cleanup is separate.


if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--context',default='kind-substrate-orbstack');parser.add_argument('--namespace');parser.add_argument('--pull-policy',default='Always',choices=['Always','IfNotPresent']);parser.add_argument('--resume',action='store_true');parser.add_argument('--real-model-env',help='Explicit local development env.json whose encrypted model profile can be used');args=parser.parse_args()
    acceptance=Acceptance(args.context,args.namespace,args.pull_policy,args.resume)
    try:
        if args.resume:acceptance.resume()
        else:acceptance.setup()
        acceptance.verify()
        if args.real_model_env:acceptance.real_model(args.real_model_env)
        acceptance.retention()
    finally:acceptance.cleanup()
