#!/usr/bin/env python3
"""Opt-in real DashScope acceptance on an owned Rust Emissary / Kubernetes Pi stack.

Provider keys are decrypted in memory from the explicitly selected private profile.
Only the relay's sanitized timing, counters and tool names become evidence.
"""
import argparse
import base64
import copy
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import time
from urllib.parse import unquote, urlsplit
import uuid
import zlib

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('operator_acceptance', Path(__file__).with_name('run.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def load_profile(path):
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    environment = json.loads(path.read_text())
    database = urlsplit(environment['DATABASE_URL'])
    connection = dict(os.environ, PGHOST=database.hostname or '127.0.0.1',
                      PGPORT=str(database.port or 5432), PGDATABASE=unquote(database.path.lstrip('/')),
                      PGUSER=unquote(database.username or ''), PGPASSWORD=unquote(database.password or ''),
                      PGCONNECT_TIMEOUT='5')
    query = "SELECT jsonb_build_object('api',api,'base_url',base_url,'model',model,'parameters',parameters,'key_ciphertext',key_ciphertext) FROM model_profiles WHERE enabled AND base_url='https://dashscope.aliyuncs.com/compatible-mode/v1' ORDER BY is_default DESC,created_at LIMIT 1"
    result = subprocess.run(['psql', '-XAt', '-c', query], env=connection, capture_output=True, check=True, text=True)
    model = json.loads(result.stdout)
    encrypted = base64.b64decode(model.pop('key_ciphertext'))
    key = AESGCM(base64.b64decode(environment['ARTIFACTORY_MASTER_KEY'])).decrypt(encrypted[:12], encrypted[12:], None).decode()
    assert model['api'] == 'openai-completions'
    return model, key


class ProviderAcceptance:
    def __init__(self, args):
        self.args = args
        self.a = module.Acceptance(args.context, args.namespace, resume=True)
        self.directory = ROOT / '.local/operator-provider' / (time.strftime('%Y%m%d-%H%M%S') + '-' + uuid.uuid4().hex[:6])
        self.directory.mkdir(parents=True)
        self.evidence = {'started_at': datetime.now(timezone.utc).isoformat(), 'context': args.context,
                         'namespace': args.namespace, 'checks': [], 'scenarios': {}, 'passed': False}
        self.profiles = []
        self.builds = []
        self.original_upstream = None
        self.original_pool = None
        self.stem = uuid.uuid4().hex[:8]

    def save(self):
        (self.directory / 'evidence.json').write_text(json.dumps(self.evidence, indent=2))

    def check(self, name, ok):
        if not ok:
            raise AssertionError(name)
        self.evidence['checks'].append(name)
        self.save()
        print('PASS', name, flush=True)

    def relay(self, scenario=None, **mode):
        if scenario is not None:
            return json.loads(self.a.service_request(39443, '/test/provider', dict(scenario=scenario, **mode)))
        return json.loads(self.a.service_request(39443, '/test/provider'))

    def records(self, scenario):
        return self.relay()[scenario]['requests']

    def profile(self, scenario, key, calls=10, timeout=600, rpm=20, mode=None):
        self.relay(scenario, **(mode or {}))
        body = dict(api=self.model['api'], name='Real supplier '+scenario,
                    base_url='https://upstream:9443/provider/'+scenario+'/v1', model=self.model['model'],
                    api_key=key, parameters=dict(self.model['parameters'], requests_per_minute=rpm, max_tokens=4096),
                    max_calls=calls, timeout_seconds=timeout, is_default=False, enabled=True)
        ident = self.a.api('/api/v1/model-profiles', body)['id']
        self.profiles.append((ident, {**body, 'api_key': '', 'enabled': False}))
        return ident

    def build(self, profile, label, goal=None):
        project = self.a.api('/api/v1/projects', {'name':'Live supplier '+label+' '+self.stem,
                            'repository_url':'https://upstream:9443/repo.git', 'planner_backend':'kubernetes',
                            'model_profile_id':profile})['id']
        goal = goal or ('First inspect README.md and build.sh using read_file and inspect node_capabilities. '
                        'Use the actual tool results before submitting your plan. Build with sh build.sh and '
                        'collect dist/result.txt on the available Linux device. Include README.md and build.sh '
                        'as evidence. No signing or external scripts are necessary.')
        build = self.a.api(f'/api/v1/projects/{project}/builds', {'ref':'v-fixture','goal':goal,
                          'target_requirements':[{'os':'linux','arch':self.a.arch}]})['id']
        self.builds.append(build)
        print('BUILD',label,build,flush=True)
        return build

    def run_state(self, build):
        history = self.a.api('/api/v1/builds/'+build+'/plans')
        if not history['runs']:
            return None
        run = history['runs'][0]['id']
        value = json.loads(self.a.sql('planner', f"SELECT jsonb_build_object('id',id,'status',status,'lifecycle',lifecycle,'usage',usage,'snapshot_id',snapshot_id,'snapshot_digest',snapshot_digest,'requests',(SELECT jsonb_agg(jsonb_build_object('id',request_id,'generation',generation,'state',state,'created_at',created_at) ORDER BY created_at) FROM runtime_model_requests WHERE run_id=r.id)) FROM runtime_runs r WHERE id='{run}'"))
        value['accepted_plans'] = len(history['plans'])
        return value

    def wait_terminal(self, build, seconds=700):
        def terminal():
            value = self.a.api('/api/v1/builds/'+build)
            return value if value['status'] in ('succeeded','failed','lost','canceled','needs_input') else None
        return self.a.wait(terminal, seconds)

    def checkpoint_summary(self, run):
        value = json.loads(self.a.sql('planner', f"SELECT coalesce(checkpoint,'{{}}') FROM runtime_runs WHERE id='{run}'"))
        encoded = value.get('messages')
        if not encoded:
            return {}
        messages = json.loads(zlib.decompress(base64.b64decode(encoded)))
        return {'messages':len(messages),
                'tool_results':[{'name':m.get('toolName'), 'error':m.get('isError',False)} for m in messages if m.get('role')=='toolResult'],
                'tool_calls':[c.get('name') for m in messages if m.get('role')=='assistant' for c in m.get('content',[]) if c.get('type')=='toolCall']}

    def store_scenario(self, name, build, scenario, **extra):
        value = {'build_id':build,'runtime':self.run_state(build),'relay':self.records(scenario),**extra}
        self.evidence['scenarios'][name] = value
        self.save()
        return value

    def success(self, key, limited=False):
        scenario = self.stem+('-retry' if limited else '-normal')
        profile = self.profile(scenario, key, mode={'reject':1} if limited else None)
        build = self.build(profile, '429 recovery' if limited else 'streaming')
        if limited:
            self.a.wait(lambda:self.records(scenario))
            before=self.run_state(build)
        detail = self.wait_terminal(build)
        value = self.store_scenario('rate_recovery' if limited else 'streaming',build,scenario)
        self.check('real supplier completes Rust Emissary build'+(' after 429' if limited else ''), detail['status']=='succeeded')
        self.check('real supplier artifact SHA-256 is correct'+(' after 429' if limited else ''), len(detail['artifacts'])==1 and detail['artifacts'][0]['sha256']==hashlib.sha256(b'operator-native-ok\n').hexdigest())
        self.check('streamed tool arguments span multiple supplier deltas'+(' after 429' if limited else ''), any(r['argument_deltas']>1 for r in value['relay']))
        self.check('tool results return to the supplier before plan acceptance'+(' after 429' if limited else ''), any(r['input_tool_results']>=2 for r in value['relay']))
        self.check('one plan and one device attempt'+(' after 429' if limited else ''), value['runtime']['accepted_plans']==1 and sum(len(j['attempts']) for j in detail['jobs'])==1)
        self.check('all physical requests retain Runtime call reservations'+(' after 429' if limited else ''), value['runtime']['lifecycle']['calls']==len(value['relay'])==len(value['runtime']['requests']))
        if limited:
            self.check('one injected 429 recovers through real supplier without resetting budget',value['relay'][0]['status']==429 and value['relay'][0]['injected'] and sum(r['injected'] for r in value['relay'])==1 and value['runtime']['lifecycle']['generation']>before['lifecycle']['generation'] and value['runtime']['lifecycle']['deadline_ms']==before['lifecycle']['deadline_ms'] and value['runtime']['snapshot_id']==before['snapshot_id'])
            value['before_retry']=before
        value['checkpoint'] = self.checkpoint_summary(value['runtime']['id'])
        self.save()

    def cancellation(self, key):
        scenario = self.stem+'-cancel'
        profile = self.profile(scenario,key,mode={'hold_after_arguments':True})
        build = self.build(profile,'stream cancellation')
        self.a.wait(lambda:any(r['held'] for r in self.records(scenario)),180)
        before = self.run_state(build)
        started = time.monotonic()
        self.a.api('/api/v1/builds/'+build+'/cancel',{})
        self.a.wait(lambda:any(r['client_disconnected'] for r in self.records(scenario)),20)
        detail = self.wait_terminal(build,60)
        elapsed = time.monotonic()-started
        value = self.store_scenario('stream_cancel',build,scenario,before=before,disconnect_seconds=elapsed)
        self.check('cancel closes a real supplier stream within 20 seconds',elapsed<20 and detail['status']=='canceled')
        self.check('cancel retains spent budget and original deadline',value['runtime']['lifecycle']['canceled'] and value['runtime']['lifecycle']['calls']==before['lifecycle']['calls'] and value['runtime']['lifecycle']['deadline_ms']==before['lifecycle']['deadline_ms'])
        self.check('canceled planning creates no device task or plan',not detail['jobs'] and value['runtime']['accepted_plans']==0)

    def call_budget(self,key):
        scenario=self.stem+'-calls'
        profile=self.profile(scenario,key,calls=1,timeout=180,rpm=6)
        builds=[self.build(profile,'shared limit '+str(i)) for i in range(2)]
        results=[self.wait_terminal(build,250) for build in builds]
        records=self.records(scenario)
        for i,(build,detail) in enumerate(zip(builds,results)):
            value=self.store_scenario('call_budget_'+str(i),build,scenario)
            self.check('one-call budget stops planning '+str(i),detail['status']=='failed' and value['runtime']['lifecycle']['calls']==1 and len(value['runtime']['requests'])==1 and not detail['jobs'])
        sent=sorted(r['sent_ms'] for r in records)
        self.check('shared profile enforces six RPM across separate Pi Pods',len(sent)==2 and sent[1]-sent[0]>=10500)
        self.evidence['shared_profile_spacing_ms']=sent[1]-sent[0]
        self.save()

    def deadline(self,key):
        scenario=self.stem+'-deadline'
        profile=self.profile(scenario,key,calls=5,timeout=30,mode={'hold_after_arguments':True})
        build=self.build(profile,'deadline')
        self.a.wait(lambda:any(r['held'] for r in self.records(scenario)),60)
        before=self.run_state(build)
        detail=self.wait_terminal(build,75)
        self.a.wait(lambda:any(r['client_disconnected'] for r in self.records(scenario)),20)
        value=self.store_scenario('deadline',build,scenario,before=before)
        self.check('planning deadline aborts an active real supplier stream',detail['status']=='failed' and not detail['jobs'] and value['runtime']['lifecycle']['deadline_ms']==before['lifecycle']['deadline_ms'] and value['runtime']['lifecycle']['calls']==1)

    def device_cancel(self,key):
        scenario=self.stem+'-device-cancel'
        profile=self.profile(scenario,key)
        build=self.build(profile,'device cancellation',goal='Read README.md and build.sh and node_capabilities. Submit a native Linux build plan with a single step whose shell field is sh, running exactly sleep 60; sh build.sh . The delay is required to exercise cancellation; set step timeout 90 and target timeout 120. Collect dist/result.txt. Use the actual tool results before submitting the plan.')
        def running():
            value=self.a.api('/api/v1/builds/'+build)
            if value['status'] in ('failed','lost','needs_input','succeeded'):
                raise AssertionError('Real planned device task did not remain running')
            return value if value['status']=='running' else None
        detail=self.a.wait(running,600)
        self.check('real supplier sh step is assigned to Rust Emissary',
                   len(detail['jobs'])==1 and len(detail['jobs'][0]['target_config']['steps'])==1
                   and detail['jobs'][0]['target_config']['steps'][0]['shell']=='sh')
        attempt=next(t for j in detail['jobs'] for t in j['attempts'] if t['status']=='running')
        ident=attempt['id']
        def processes():
            value=json.loads(self.a.sql('telemetry',f"SELECT coalesce(jsonb_agg(DISTINCT pid),'[]') FROM process_samples WHERE attempt_id='{ident}'"))
            return value or None
        pids=self.a.wait(processes,20)
        before=self.run_state(build)
        self.store_scenario('device_cancel',build,scenario,attempt_id=ident,observed_processes=len(pids),cleanup_complete=False)
        started=time.monotonic()
        self.a.api('/api/v1/builds/'+build+'/cancel',{})
        detail=self.a.wait(lambda:(v if (v:=self.a.api('/api/v1/builds/'+build))['status']=='canceled' else None),30)
        script='for pid in '+ ' '.join(str(int(p)) for p in pids)+'; do if kill -0 "$pid" 2>/dev/null; then exit 1; fi; done'
        self.a.wait(lambda:self.a.kubectl('exec','deploy/device','--','sh','-ec',script)=='',30)
        elapsed=time.monotonic()-started
        value=self.store_scenario('device_cancel',build,scenario,attempt_id=ident,observed_processes=len(pids),cleanup_seconds=elapsed,cleanup_complete=True)
        self.check('cancel terminates the real-supplier-planned Rust Emissary process tree',elapsed<30 and not detail['artifacts'])
        self.check('device cancellation keeps one accepted plan and spent planning budget',value['runtime']['accepted_plans']==1 and value['runtime']['lifecycle']['calls']==before['lifecycle']['calls'])

    def execute(self):
        a=self.a
        key=None
        try:
            a.resume()
            self.check('test namespace has no active builds',a.sql('business',"SELECT count(*) FROM builds WHERE status IN ('resolving','planning','replanning','queued','running','canceling')")=='0')
            self.check('device container has a process-reaping init',a.kubectl('exec','deploy/device','--','cat','/proc/1/comm').strip()=='tini')
            deployment=json.loads(a.kubectl('get','deploy','upstream','-o','json'))
            self.original_upstream=copy.deepcopy(deployment['spec']['template']['spec']['containers'])
            self.original_pool=json.loads(a.kubectl('get','agentpool','default','-o','json'))['spec']
            self.model,key=load_profile(self.args.profile_env)
            self.evidence.update(provider=self.model['base_url'],model=self.model['model'],protocol=self.model['api'],git_head=a.command(['git','rev-parse','HEAD']).strip(),relay_image=self.args.relay_image)
            containers=copy.deepcopy(self.original_upstream)
            containers[0]['image']=self.args.relay_image
            containers[0]['env']=[e for e in containers[0].get('env',[]) if e['name']!='LIVE_PROVIDER_BASE_URL']+[{'name':'LIVE_PROVIDER_BASE_URL','value':self.model['base_url']}]
            a.kubectl('patch','deploy','upstream','--type=strategic','-p',json.dumps({'spec':{'template':{'spec':{'containers':containers}}}}))
            a.kubectl('rollout','status','deploy/upstream','--timeout=180s')
            for process in a.processes:process.terminate();process.wait(timeout=10)
            a.processes.clear();a.resume()
            a.wait(lambda:self.relay() is not None)
            a.kubectl('patch','agentpool','default','--type=merge','-p','{"spec":{"maxActive":2}}')
            self.evidence['images']=a.images()
            self.evidence['emissary_binary_sha256']=a.kubectl('exec','deploy/device','--','sha256sum','/usr/local/bin/artifactory-emissary').split()[0]
            source_hash=hashlib.sha256()
            sources=[ROOT/'Cargo.toml',ROOT/'Cargo.lock',ROOT/'packages/agent-worker/package-lock.json']
            for directory in ('crates','operator/src','packages/agent-worker/src','proto'):
                sources.extend(path for path in (ROOT/directory).rglob('*') if path.is_file())
            for path in sorted(sources):
                source_hash.update(str(path.relative_to(ROOT)).encode()+b'\0'+path.read_bytes()+b'\0')
            self.evidence['source_sha256']=source_hash.hexdigest()
            self.save()
            self.success(key)
            self.success(key,limited=True)
            self.cancellation(key)
            self.call_budget(key)
            self.deadline(key)
            self.device_cancel(key)
        finally:
            del key
            for build in self.builds:
                try:
                    if a.api('/api/v1/builds/'+build)['status'] not in ('succeeded','failed','lost','canceled','needs_input'):
                        a.api('/api/v1/builds/'+build+'/cancel',{})
                        self.wait_terminal(build,60)
                except (OSError,RuntimeError):pass
            for ident,body in self.profiles:
                a.api('/api/v1/model-profiles/'+ident,body,method='PATCH')
            if self.original_pool is not None:
                a.kubectl('patch','agentpool','default','--type=merge','-p',json.dumps({'spec':self.original_pool}))
            if self.original_upstream is not None:
                a.kubectl('patch','deploy','upstream','--type=json','-p',json.dumps([{'op':'replace','path':'/spec/template/spec/containers','value':self.original_upstream}]))
                a.kubectl('rollout','status','deploy/upstream','--timeout=180s')
            if self.profiles:
                ids=','.join("'"+str(uuid.UUID(ident))+"'" for ident,_ in self.profiles)
                self.check('all temporary supplier profiles are disabled',a.sql('business',f'SELECT count(*) FROM model_profiles WHERE enabled AND id IN ({ids})')=='0')
            upstream_restored=self.original_upstream is not None and json.loads(a.kubectl('get','deploy','upstream','-o','json'))['spec']['template']['spec']['containers']==self.original_upstream
            pool_restored=self.original_pool is not None and json.loads(a.kubectl('get','agentpool','default','-o','json'))['spec']==self.original_pool
            if self.original_upstream is not None:
                self.check('supplier relay deployment is restored',upstream_restored)
            if self.original_pool is not None:
                self.check('AgentPool configuration is restored',pool_restored)
            active=sum(a.api('/api/v1/builds/'+build)['status'] not in ('succeeded','failed','lost','canceled','needs_input') for build in self.builds)
            self.check('no active supplier test build remains',active==0)
            self.evidence['cleanup']={'test_profiles_disabled':len(self.profiles),'upstream_restored':upstream_restored,'pool_restored':pool_restored,'active_test_builds':active}
            self.evidence['finished_at']=datetime.now(timezone.utc).isoformat()
            self.save();a.cleanup()
        self.evidence['passed']=True
        self.save()
        print('REAL SUPPLIER ACCEPTANCE PASSED',self.directory/'evidence.json',flush=True)


if __name__=='__main__':
    os.umask(0o077)
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--context',required=True)
    parser.add_argument('--namespace',required=True)
    parser.add_argument('--profile-env',type=Path,required=True)
    parser.add_argument('--relay-image',required=True)
    ProviderAcceptance(parser.parse_args()).execute()
