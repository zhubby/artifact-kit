#!/usr/bin/env python3
"""Run a candidate Agent on an isolated Mac mini identity, using only control.

The isolated Tailnet ingress exposes control, never the upstream Git/LFS fixture.
The existing native service, config and identity are left in place for preflight.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import ssl
import time
import urllib.parse
import urllib.request

ROOT=Path(__file__).resolve().parents[1]
os.umask(0o077)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run',required=True,type=Path)
    parser.add_argument('--ssh-host',required=True)
    parser.add_argument('--identity',required=True,type=Path)
    args=parser.parse_args();run=args.run.resolve()
    access=json.loads((run/'access.json').read_text());environment=json.loads((run/'env.json').read_text())
    ssh=['ssh','-i',str(args.identity.resolve()),'-o','IdentitiesOnly=yes','-o','BatchMode=yes','-o','ConnectTimeout=12']
    ca=Path(environment['ARTIFACTORY_PKI_DIR'])/'ca.pem'
    context=ssl.create_default_context();context.load_verify_locations(cafile=str(ca));context.verify_flags &= ~ssl.VERIFY_X509_STRICT
    http=urllib.request.build_opener(urllib.request.ProxyHandler({}),urllib.request.HTTPSHandler(context=context))
    endpoint=json.loads((run/'worker.json').read_text())['endpoint']
    if endpoint.startswith('localhost:'):
        raise RuntimeError('start the isolated acceptance with --tailnet-address before the Mac mini check')
    headers={'Cookie':'; '.join(k+'='+v for k,v in access['cookies'].items()),'Content-Type':'application/json','Origin':access['base'],'x-artifactory-csrf':'1'}
    def api(path,body=None,method=None):
        with http.open(urllib.request.Request(access['base']+'/api/v1'+path,data=None if body is None else json.dumps(body).encode(),method=method,headers=headers),timeout=45) as response:return json.load(response)
    evidence={'kind':'isolated-macmini-platform-source','checks':[]}
    def check(name,condition):
        if not condition:raise AssertionError(name)
        evidence['checks'].append(name);print('PASS',name,flush=True)
        (run/'macmini-evidence.json').write_text(json.dumps(evidence,indent=2))
    def remote(script,payload=None):
        value=subprocess.run(ssh+[args.ssh_host,'python3 -c '+shlex.quote(script)],input=None if payload is None else json.dumps(payload).encode(),capture_output=True,timeout=90)
        if value.returncode:raise RuntimeError('remote acceptance operation failed: '+value.stderr.decode(errors='replace')[-1500:])
        return value.stdout.decode().strip()
    stage='/Users/codehub/.config/artifactory-ai-acceptance/'+run.name+'-'+time.strftime('%H%M%S')
    remote('from pathlib import Path;import os;os.umask(0o077);Path('+repr(stage)+').mkdir(parents=True,exist_ok=False)')
    binary=ROOT/'.local/ai-implementation/artifactory-agent'
    scp=['scp','-q','-i',str(args.identity.resolve()),'-o','IdentitiesOnly=yes','-o','BatchMode=yes']
    subprocess.run(scp+[str(binary),str(ca),args.ssh_host+':'+stage+'/'],check=True,timeout=90)
    node=pool=project=pid=None
    try:
        source=api('/projects/'+access['project_id'])
        port=urllib.parse.urlsplit(source['repository_url']).port
        result=json.loads(remote('import socket,json\ntry:\n socket.create_connection(("127.0.0.1",'+str(port)+'),timeout=2);value=False\nexcept OSError:value=True\nprint(json.dumps(value))'))
        check('Mac mini cannot reach loopback upstream Git/LFS',result)
        pool=api('/pools',{'name':'isolated-macmini-'+stage.rsplit('/',1)[-1]})['id']
        token=api('/enrollment-tokens',{'pool_id':pool})['token']
        payload={'stage':stage,'token':token,'endpoint':endpoint}
        result=json.loads(remote('''import os,json,subprocess
from pathlib import Path
os.umask(0o077);p=json.load(__import__('sys').stdin);stage=Path(p['stage']);binary=stage/'artifactory-agent';binary.chmod(0o700)
env={'PATH':'/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin','HOME':str(stage),'LANG':'en_US.UTF-8','ARTIFACTORY_ENROLLMENT_TOKEN':p['token']}
subprocess.run([str(binary),'register','--config',str(stage/'agent.json'),'--endpoint',p['endpoint'],'--ca',str(stage/'ca.pem'),'--root',str(stage/'data'),'--name','isolated-macmini-ai','--allow-http-uploads'],env=env,stdout=subprocess.DEVNULL,check=True)
config=json.loads((stage/'agent.json').read_text());config['signing_profiles']={'adhoc-test':{}};config['git_ca_file']=str(stage/'ca.pem');(stage/'agent.json').write_text(json.dumps(config));env.pop('ARTIFACTORY_ENROLLMENT_TOKEN')
with (stage/'agent.log').open('ab') as log:process=subprocess.Popen([str(binary),'run','--config',str(stage/'agent.json')],env=env,stdin=subprocess.DEVNULL,stdout=log,stderr=log,start_new_session=True)
(stage/'agent.pid').write_text(str(process.pid));print(json.dumps({'node_id':config['agent_id'],'pid':process.pid}))
''',payload))
        node,pid=result['node_id'],result['pid'];evidence.update(node_id=node,remote_stage=stage,agent_sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
        api('/nodes/'+node,{'pool_id':pool,'slots':1,'labels':{},'draining':False,'revoked':False,'signing_profiles':['adhoc-test']},'PATCH')
        for _ in range(120):
            detail=next(n for n in api('/nodes') if n['id']==node)
            if detail['online'] and all(c in detail['capabilities'] for c in ['source-cache-v1','external-build-plan-v1','git-lfs','cc','codesign']):break
            time.sleep(.5)
        check('Mac mini candidate supports platform source and LFS',detail['online'] and all(c in detail['capabilities'] for c in ['source-cache-v1','external-build-plan-v1','git-lfs','cc','codesign']))
        project=api('/projects',{'name':'Mac mini zero intrusion '+run.name,'repository_url':source['repository_url'],'pool_ids':[pool],'signing_profiles':['adhoc-test']})['id']
        created=api('/projects/'+project+'/builds',{'ref':'v-fixture-1','goal':'Inspect README and Git metadata. Build the native C executable using the main source, relative submodule and nested submodule. Verify the LFS payload contains artifactory-lfs-ok, verify tag v-fixture-1, and verify executable prints artifactory-ai-7. Collect dist/app. Use adhoc-test signing profile: sign with codesign --force --sign - dist/app and verify codesign --verify --strict dist/app in separate phases. Use one external script for the content checks.','target_requirements':[{'os':'darwin','arch':'arm64'}]})['id']
        evidence['build_id']=created;print('MAC MINI BUILD',created,flush=True)
        before=None;until=time.monotonic()+1800
        while time.monotonic()<until:
            detail=api('/builds/'+created)
            if detail['status']!=before:print('macmini',detail['status'],flush=True);before=detail['status']
            if detail['status'] in ['succeeded','needs_input','canceled','lost']:break
            if detail['status']=='failed':
                runs=api('/builds/'+created+'/plans')['runs']
                if not detail.get('current_plan_id') or detail.get('revision_count',0)>=2 or runs and runs[-1]['state']=='failed':break
                failed=[j for j in detail.get('jobs',[]) if j['status']=='failed']
                if failed and all(j.get('attempts') and j['attempts'][-1].get('failure_kind','execution')!='execution' for j in failed):break
            time.sleep(1)
        (run/'macmini-build.json').write_text(json.dumps(detail,indent=2))
        check('Mac mini builds, signs and verifies from platform source',detail['status']=='succeeded')
        check('all attempts execute on isolated Mac mini',all(a['node_id']==node for j in detail['jobs'] for a in j['attempts']))
        check('real Pi model planned the Mac mini build',sum(r['calls'] for r in api('/builds/'+created+'/plans')['runs'])>0)
        for artifact in detail['artifacts']:
            with http.open(urllib.request.Request(access['base']+'/api/v1/artifacts/'+artifact['id']+'/download',headers=headers),timeout=30) as response:data=response.read()
            check('Mac mini artifact download and SHA-256',hashlib.sha256(data).hexdigest()==artifact['sha256'])
        check('Mac mini execution retained fixed commit',detail['commit_sha']==json.loads((run/'evidence.json').read_text())['source_commit'])
    finally:
        if pid:
            remote('import os,signal;os.kill('+str(pid)+',signal.SIGTERM)')
            time.sleep(2)
            subprocess.run(scp+[args.ssh_host+':'+stage+'/agent.log',str(run/'macmini-agent.log')],check=False,timeout=30)
        if node:
            settings=next(n for n in api('/nodes') if n['id']==node)
            api('/nodes/'+node,{**{k:settings[k] for k in ['pool_id','slots','labels','signing_profiles']},'draining':True,'revoked':True},'PATCH')
        (run/'macmini-evidence.json').write_text(json.dumps(evidence,indent=2))


if __name__=='__main__':main()
