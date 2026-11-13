#!/usr/bin/env python3
"""Check cancellation cleanup with an unreachable kubelet in a dedicated kind.

Freezes one real Pi and stops its node's kubelet; the API server and Docker
management path remain available. Always resumes Pi and restarts kubelet.
Requires the existing acceptance harness namespace and its saved private PKI.
This is a kubelet outage, not a complete network-partition experiment.
"""
import argparse
import json
import subprocess
import time
import uuid

from run import Acceptance, ROOT


def docker(node, *args, data=None, check=True):
    result = subprocess.run(['docker', 'exec', '-i', node, *args], input=data,
                            capture_output=True, timeout=30)
    if check and result.returncode:
        raise RuntimeError('Dedicated node management command failed: ' + args[0])
    return result


def run(args):
    a = Acceptance(args.context, args.namespace, resume=True)
    directory = ROOT / '.local/operator-load' / (time.strftime('%Y%m%d-%H%M%S') + '-cleanup')
    directory.mkdir(mode=0o700)
    evidence = {'fault': 'Pi SIGSTOP and kubelet stopped; API and Docker reachable',
                'context': a.context, 'namespace': a.ns, 'passed': False}
    frozen_pid = None
    kubelet_stopped = False
    build = None
    request = None
    original_pool = None
    canceled = False
    try:
        # A kind name alone is insufficient: check Docker ownership and workloads.
        cluster = subprocess.check_output(['docker', 'inspect', '--format',
            '{{index .Config.Labels "io.x-k8s.kind.cluster"}}', args.node_container],
            text=True, timeout=15).strip()
        assert cluster and args.context == 'kind-' + cluster, 'Node is not owned by the selected kind'
        nodes = json.loads(a.command(['kubectl', '--context', a.context, 'get', 'nodes', '-o', 'json']))['items']
        assert len(nodes) == 1 and nodes[0]['metadata']['name'] == args.node_container, 'Use a dedicated single-node kind'
        pods = json.loads(a.command(['kubectl', '--context', a.context, 'get', 'pods', '-A', '-o', 'json']))['items']
        assert not any(p['metadata']['namespace'] not in [a.ns, 'kube-system', 'local-path-storage'] for p in pods), 'Unrelated workloads found on fault target'
        assert docker(args.node_container, 'systemctl', 'is-active', 'kubelet').stdout.strip() == b'active'
        a.resume()
        assert a.sql('planner', "SELECT count(*) FROM runtime_runs WHERE status IN ('running','provisioning','queued')") == '0', 'Drain this test namespace first'
        original_pool = json.loads(a.kubectl('get', 'agentpool', 'default', '-o', 'json'))['spec']
        a.kubectl('patch', 'agentpool', 'default', '--type=merge', '-p', json.dumps({'spec': {'maxActive': 1}}))
        pods = json.loads(a.kubectl('get', 'pods', '-o', 'json'))['items']
        evidence['operator_images'] = [{'uid': p['metadata']['uid'], 'containers': [
            {key: c.get(key) for key in ['image', 'imageID']} for c in p['status'].get('containerStatuses', [])]}
            for p in pods if p['metadata'].get('labels', {}).get('app.kubernetes.io/name') == 'artifactory-operator']
        control = next(p for p in pods if p['metadata'].get('labels', {}).get('app') == 'control-plane')
        runtime = next(p for p in pods if p['metadata'].get('labels', {}).get('app.kubernetes.io/name') == 'artifactory-runtime')
        upstream = next(p for p in pods if p['metadata'].get('labels', {}).get('app') == 'upstream')
        runtime_id = runtime['status']['containerStatuses'][0]['containerID'].split('://', 1)[1]
        runtime_pid = json.loads(docker(args.node_container, 'crictl', 'inspect', runtime_id).stdout)['info']['pid']
        ca = '/proc/' + str(runtime_pid) + '/root/run/service-tls/ca.crt'
        cookie = '; '.join(c.name + '=' + c.value for c in a.jar)

        def request(role, path, body=None):
            # Credentials enter curl through stdin, never argv or captured headers.
            config = ['silent', 'show-error', 'fail', 'max-time = 15', 'noproxy = "*"']
            if role == 'control-plane':
                url = 'http://' + control['status']['podIP'] + ':8080' + path
                headers = {'Cookie': cookie, 'Origin': a.origin, 'x-artifactory-csrf': '1'}
            else:
                pod = runtime if role == 'artifactory-runtime' else upstream
                port = 8091 if role == 'artifactory-runtime' else 9443
                url = 'https://' + role + ':' + str(port) + path
                config += ['resolve = ' + json.dumps(role + ':' + str(port) + ':' + pod['status']['podIP']),
                           'cacert = ' + json.dumps(ca)]
                headers = {'Authorization': 'Bearer ' + a.token}
            config.append('url = ' + json.dumps(url))
            for key, value in headers.items():
                config.append('header = ' + json.dumps(key + ': ' + value))
            if body is not None:
                config += ['header = "Content-Type: application/json"',
                           'data = ' + json.dumps(json.dumps(body))]
            result = docker(args.node_container, 'curl', '--config', '-',
                            data=('\n'.join(config) + '\n').encode())
            return json.loads(result.stdout)

        assert request('control-plane', '/healthz')['status'] == 'ok', 'Independent control path unavailable'
        request('upstream', '/test/mode', {'hold': True})
        project = a.api('/api/v1/projects', {'name': 'cleanup-fault-' + uuid.uuid4().hex[:8],
                         'repository_url': 'https://upstream:9443/repo.git', 'planner_backend': 'kubernetes'})['id']
        build = a.api('/api/v1/projects/' + project + '/builds', {
            'ref': 'v-fixture', 'goal': 'Hold a real model request for node cleanup verification',
            'target_requirements': [{'os': 'linux', 'arch': a.arch}]})['id']
        evidence['build_id'] = build

        def running():
            runs = a.api('/api/v1/builds/' + build + '/plans')['runs']
            if not runs:
                return None
            value = a.sql('planner', "SELECT jsonb_build_object('id',id,'life',lifecycle,'execution',execution) FROM runtime_runs WHERE id='" + runs[0]['id'] + "' AND status='running' AND (lifecycle->>'calls')::int=1")
            return json.loads(value) if value else None

        before = a.wait(running, 180)
        run_id = before['id']
        run_name = 'run-' + run_id + '-g' + str(before['life']['generation'])
        # Verify the independent Runtime path before disabling Kubernetes exec.
        assert request('artifactory-runtime', '/v1/get', {'run_id': run_id})['status'] == 'running'
        pod = json.loads(a.kubectl('get', 'pod', before['execution']['name'], '-o', 'json'))
        pod_uid = pod['metadata']['uid']
        assert pod_uid == before['execution']['uid']
        a.wait(lambda: json.loads(a.kubectl('get', 'agentrun', run_name, '-o', 'json')).get('status', {}).get('podUid') == pod_uid, 30)
        container_id = pod['status']['containerStatuses'][0]['containerID'].split('://', 1)[1]
        frozen_pid = json.loads(docker(args.node_container, 'crictl', 'inspect', container_id).stdout)['info']['pid']
        evidence.update({'run_id': run_id, 'pod_uid': pod_uid, 'before': before})
        docker(args.node_container, 'kill', '-STOP', str(frozen_pid))
        # Mark first so a partially successful systemctl command still triggers recovery.
        kubelet_stopped = True
        docker(args.node_container, 'systemctl', 'stop', 'kubelet')
        assert docker(args.node_container, 'systemctl', 'is-active', 'kubelet', check=False).returncode != 0
        print('Pi paused; dedicated node kubelet stopped; independent API path retained', flush=True)
        probe = subprocess.run(['kubectl', '--context', a.context, 'get', '--raw',
            '/api/v1/nodes/' + args.node_container + '/proxy/healthz', '--request-timeout=5s'],
            capture_output=True, timeout=10)
        assert probe.returncode != 0, 'Kubelet is still reachable'
        evidence['kubelet_unreachable'] = True
        started = time.monotonic()
        request('control-plane', '/api/v1/builds/' + build + '/cancel', {})
        canceled = True
        a.wait(lambda: request('artifactory-runtime', '/v1/get', {'run_id': run_id})['status'] == 'canceled', 30)
        evidence['authority_revoked_seconds'] = round(time.monotonic() - started, 3)
        a.wait(lambda: json.loads(a.kubectl('get', 'pod', pod['metadata']['name'], '-o', 'json'))['metadata'].get('deletionTimestamp'), 30)
        a.wait(lambda: json.loads(a.kubectl('get', 'agentrun', run_name, '-o', 'json')).get('status', {}).get('phase') == 'canceled', 30)
        # Keep the fault in place across several reconcile attempts and node health checks.
        samples = []
        for _ in range(8):
            current = json.loads(a.kubectl('get', 'agentrun', run_name, '-o', 'json'))
            physical = docker(args.node_container, 'cat', '/proc/' + str(frozen_pid) + '/stat').stdout.decode().split(') ', 1)[1].split()[0]
            samples.append({'elapsed_seconds': round(time.monotonic() - started, 3),
                            'status': current.get('status', {}), 'process_state': physical})
            time.sleep(5)
        evidence['isolated_samples'] = samples
        node = json.loads(a.command(['kubectl', '--context', a.context, 'get', 'node', args.node_container, '-o', 'json']))
        evidence['isolated_node_ready'] = next(c['status'] for c in node['status']['conditions'] if c['type'] == 'Ready')
        evidence['pending_identity_preserved'] = all(s['status'].get('podUid') == pod_uid and
            s['status'].get('message', '').startswith('CleanupPending:') and s['status'].get('phase') == 'canceled'
            and s['process_state'] == 'T' for s in samples)
        assert not request('control-plane', '/api/v1/builds/' + build)['jobs'], 'Cancel created device jobs'
        evidence['logical_cancellation_no_device_jobs'] = True
    finally:
        if frozen_pid is not None:
            docker(args.node_container, 'kill', '-CONT', str(frozen_pid), check=False)
        if kubelet_stopped:
            docker(args.node_container, 'systemctl', 'start', 'kubelet')
            a.command(['kubectl', '--context', a.context, 'wait', 'node/' + args.node_container,
                       '--for=condition=Ready', '--timeout=120s'])
            evidence['kubelet_restored'] = True
        try:
            if request is not None:
                if build and not canceled:
                    request('control-plane', '/api/v1/builds/' + build + '/cancel', {})
                request('upstream', '/test/mode', {})
        finally:
            for process in a.processes:
                process.terminate()
            if original_pool is not None:
                a.kubectl('patch', 'agentpool', 'default', '--type=merge', '-p', json.dumps({'spec': original_pool}))
            (directory / 'evidence.json').write_text(json.dumps(evidence, indent=2))
    a.wait(lambda: not json.loads(a.kubectl('get', 'pods', '-l', 'artifactory.dev/run=' + run_id, '-o', 'json'))['items'], 120)
    a.wait(lambda: (s := json.loads(a.kubectl('get', 'agentrun', run_name, '-o', 'json')).get('status', {})).get('phase') == 'canceled' and not s.get('podUid') and not s.get('message'), 90)
    after = request('artifactory-runtime', '/v1/get', {'run_id': run_id})
    evidence['pod_cleanup_confirmed'] = True
    evidence['status_cleared_after_cleanup'] = True
    evidence['pi_process_exited'] = docker(args.node_container, 'test', '-e', '/proc/' + str(frozen_pid), check=False).returncode == 1
    evidence['budget_and_deadline_preserved'] = after['lifecycle']['calls'] == before['life']['calls'] and after['lifecycle']['deadline_ms'] == before['life']['deadline_ms']
    evidence['passed'] = evidence['pending_identity_preserved'] and evidence['budget_and_deadline_preserved'] and evidence['pi_process_exited']
    (directory / 'evidence.json').write_text(json.dumps(evidence, indent=2))
    print(json.dumps({'evidence': str(directory.relative_to(ROOT)), 'passed': evidence['passed'],
                      'pending_identity_preserved': evidence['pending_identity_preserved'],
                      'kubelet_restored': evidence.get('kubelet_restored')}), flush=True)
    assert evidence['passed'], 'Operator discarded the Pod identity before physical cleanup'


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--context', required=True)
    parser.add_argument('--namespace', required=True)
    parser.add_argument('--node-container', required=True)
    run(parser.parse_args())
