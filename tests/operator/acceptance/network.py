#!/usr/bin/env python3
"""Verify the shipped Pi policy from the first network attempt in a new Pod."""
import argparse
import importlib.util
import json
from pathlib import Path
import time

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('acceptance', Path(__file__).with_name('run.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def probe(a, restricted):
    name = 'pi-network-' + ('restricted' if restricted else 'control')
    upstream = json.loads(a.kubectl('get', 'service', 'upstream', '-o', 'json'))['spec']['clusterIP']
    targets = {'runtime': ['artifactory-runtime', 8091], 'source': ['sources-0.sources.' + a.ns + '.svc', 8090], 'business': ['control-plane', 8080], 'provider': ['upstream', 9443], 'provider_ip': [upstream, 9443], 'database': ['postgres', 5432], 'objects': ['s3', 8333]}
    code = 'const net=require("node:net"); const targets=' + json.dumps(targets) + '; Promise.all(Object.entries(targets).map(([key,[host,port]])=>new Promise(resolve=>{ const s=net.createConnection({host,port}); let done=false; const finish=ok=>{if(done)return;done=true;s.destroy();resolve([key,ok]);}; s.setTimeout(3000,()=>finish(false));s.on("connect",()=>finish(true));s.on("error",()=>finish(false));}))).then(x=>console.log(JSON.stringify(Object.fromEntries(x))));'
    pool = json.loads(a.kubectl('get', 'agentpool', 'default', '-o', 'json'))['spec']
    pod = {'apiVersion': 'v1', 'kind': 'Pod', 'metadata': {'name': name, 'labels': {'app.kubernetes.io/name': 'artifactory-pi' if restricted else 'network-control', 'artifactory.dev/network-probe': str(time.time_ns())}}, 'spec': {'restartPolicy': 'Never', 'automountServiceAccountToken': False, 'containers': [{'name': 'probe', 'image': pool['image'], 'imagePullPolicy': pool.get('imagePullPolicy', 'IfNotPresent'), 'command': ['node', '-e', code]}]}}
    a.kubectl('delete', 'pod', name, '--ignore-not-found')
    a.apply(pod)
    a.wait(lambda: json.loads(a.kubectl('get', 'pod', name, '-o', 'json'))['status']['phase'] in ['Succeeded', 'Failed'])
    values = json.loads(a.kubectl('logs', name).strip())
    a.kubectl('delete', 'pod', name)
    return values


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--context', required=True)
    parser.add_argument('--namespace', required=True)
    parser.add_argument('--skip-e2e', action='store_true')
    parser.add_argument('--repeat', type=int, default=3, help='Independent new-Pod probes with fresh network identities')
    parser.add_argument('--controller', default='cluster CNI', help='Installed CNI name/version for the evidence record')
    args = parser.parse_args()
    a = module.Acceptance(args.context, args.namespace, resume=True)
    a.evidence = json.loads((a.run / 'evidence.json').read_text())
    directory = ROOT / '.local/operator-load' / (time.strftime('%Y%m%d-%H%M%S') + '-network')
    directory.mkdir(parents=True)
    try:
        a.resume()
        unrestricted = probe(a, False)
        restricted = [probe(a, True) for _ in range(args.repeat)]
        print('NETWORK PROBES', {'unrestricted': unrestricted, 'restricted': restricted}, flush=True)
        evidence = {'context': args.context, 'namespace': args.namespace, 'controller': args.controller, 'images': a.images(), 'unrestricted': unrestricted, 'restricted': restricted, 'cold_pod_probe': True, 'e2e': False, 'passed': False}
        (directory / 'evidence.json').write_text(json.dumps(evidence, indent=2))
        assert all(unrestricted.values()), 'Positive control must reach all targets before asserting isolation'
        assert args.repeat > 0 and all(value == {key: key in ['runtime', 'source'] for key in unrestricted} for value in restricted), 'Pi must reach Runtime/Source while direct data/provider paths are blocked'
        evidence['passed'] = True
        (directory / 'evidence.json').write_text(json.dumps(evidence, indent=2))
        print('NETWORK ENFORCEMENT PASSED', evidence, flush=True)
        if not args.skip_e2e:
            a.verify()
            evidence['e2e'] = True
            (directory / 'evidence.json').write_text(json.dumps(evidence, indent=2))
        print('NETWORK ACCEPTANCE PASSED', directory / 'evidence.json', flush=True)
    finally:
        a.cleanup()
