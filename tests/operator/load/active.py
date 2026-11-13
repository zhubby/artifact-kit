#!/usr/bin/env python3
"""Measure real active Pi Pods, recovery and cancellation in a saved test namespace.

Capacity is checked before admission. This is distinct from admission.py's 10,000
database-only queued runs. Evidence never contains service credentials.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import importlib.util
import json
import math
from pathlib import Path
import re
import subprocess
import threading
import time
import uuid

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('operator_acceptance', ROOT/'tests/operator/acceptance/run.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def quantity_mib(value):
    for suffix, factor in [('Gi', 1024), ('Mi', 1), ('Ki', 1/1024), ('G', 1000000000/1048576), ('M', 1000000/1048576), ('K', 1000/1048576)]:
        if value.endswith(suffix):
            return float(value[:-len(suffix)]) * factor
    return float(value)/1048576


class Capacity:
    def __init__(self, args):
        self.args = args
        self.a = module.Acceptance(args.context, args.namespace, resume=True)
        self.directory = ROOT/'.local/operator-load'/(time.strftime('%Y%m%d-%H%M%S')+'-active-'+str(args.active))
        self.directory.mkdir(parents=True)
        self.builds = []
        self.samples = []
        self.observations = []
        self.errors = []
        self.project = None
        self.pod_watch = None
        self.evidence = {'context': args.context, 'namespace': args.namespace, 'requested_active': args.active, 'extra_queued': args.queued, 'hold_seconds': args.hold_seconds, 'passed': False, 'local_slo': {'overview_p95_ms_less_than': 1000, 'errors': 0}, 'production_capacity_claim': False}

    def save(self):
        (self.directory/'evidence.json').write_text(json.dumps(self.evidence, indent=2))

    def nodes(self):
        return json.loads(self.a.command(['kubectl','--context',self.a.context,'get','nodes','-o','json']))['items']

    def preflight(self):
        pods = json.loads(self.a.command(['kubectl','--context',self.a.context,'get','pods','-A','-o','json']))['items']
        available = 0
        for node in self.nodes():
            alloc = node['status']['allocatable']
            assigned = [p for p in pods if p['spec'].get('nodeName') == node['metadata']['name'] and p['status']['phase'] not in ['Succeeded','Failed']]
            requested_mib = sum(quantity_mib(c.get('resources',{}).get('requests',{}).get('memory','0')) for p in assigned for c in p['spec']['containers'])
            memory = quantity_mib(alloc['memory'])
            if self.args.node_memory_mib:
                memory = min(memory, self.args.node_memory_mib)
            available += max(0, min(int(alloc['pods'])-len(assigned), int((memory-requested_mib)/quantity_mib(self.args.memory))))
        self.evidence['explicit_node_memory_cap_mib'] = self.args.node_memory_mib
        self.evidence['preflight_available_pod_slots_by_memory_and_pod_limit'] = available
        self.save()
        if available < self.args.active:
            raise ValueError(f'Requires {self.args.active} real Pod slots; this cluster has at most {available}. Use a larger dedicated cluster.')

    def observation(self):
        current = json.loads(self.a.kubectl('get','pods','-o','json'))['items']
        live = {p['metadata']['uid'] for p in current if p['status']['phase'] == 'Running' and not p['metadata'].get('deletionTimestamp')}
        values = []
        for node in self.nodes():
            summary = json.loads(self.a.command(['kubectl','--context',self.a.context,'get','--raw','/api/v1/nodes/'+node['metadata']['name']+'/proxy/stats/summary']))
            for pod in summary['pods']:
                if pod['podRef']['namespace'] != 'kube-system' and pod['podRef']['uid'] not in live:
                    continue
                values.append({'name': pod['podRef']['name'], 'namespace': pod['podRef']['namespace'], 'uid': pod['podRef']['uid'], 'cpu_ns': pod.get('cpu',{}).get('usageCoreNanoSeconds',0), 'rss_bytes': pod.get('memory',{}).get('rssBytes',0), 'working_set_bytes': pod.get('memory',{}).get('workingSetBytes',0), 'rx_bytes': pod.get('network',{}).get('rxBytes',0), 'tx_bytes': pod.get('network',{}).get('txBytes',0)})
        database = json.loads(self.a.sql('postgres', "SELECT json_build_object('connections',(SELECT json_agg(s) FROM (SELECT datname,wait_event_type,count(*) FROM pg_stat_activity WHERE datname IN ('business','planner','sources') GROUP BY datname,wait_event_type) s),'databases',(SELECT json_agg(s) FROM (SELECT datname,numbackends,xact_commit,xact_rollback,deadlocks FROM pg_stat_database WHERE datname IN ('business','planner','sources')) s))"))
        counts = json.loads(self.a.sql('planner', "SELECT coalesce(json_object_agg(status,n),'{}') FROM (SELECT status,count(*) n FROM runtime_runs GROUP BY status) s"))
        pi = [self.pod_state(p) for p in current if p['metadata'].get('labels',{}).get('app.kubernetes.io/name') == 'artifactory-pi']
        executions = [] if not self.project else json.loads(self.a.sql('planner', "SELECT coalesce(json_agg(json_build_object('id',id,'status',status,'generation',lifecycle->'generation','owner',lifecycle->'owner','calls',lifecycle->'calls','lease_until_ms',lifecycle->'lease_until_ms')),'[]') FROM runtime_runs WHERE project_id='"+self.project+"'"))
        owners = {r['owner'] for r in executions if r['status'] == 'running'}
        running = {p['uid'] for p in pi if p['uid'] in live}
        return {'time': time.time(), 'pods': values, 'database': database, 'runtime_counts': counts, 'pi_pods': pi, 'runtime_executions': executions, 'running_owners_without_running_pod': sorted(owners-running), 'running_pods_without_runtime_owner': sorted(running-owners), 'pi_running_pods': len(running), 'agentrun_crds': len(json.loads(self.a.kubectl('get','agentruns','-o','json'))['items'])}

    @staticmethod
    def pod_state(pod):
        meta, status = pod['metadata'], pod.get('status',{})
        return {'name':meta['name'], 'uid':meta['uid'], 'run':meta.get('labels',{}).get('artifactory.dev/run'), 'phase':status.get('phase'), 'reason':status.get('reason'), 'deleted_at':meta.get('deletionTimestamp'), 'containers':[{'name':c['name'], 'restarts':c.get('restartCount'), 'state':c.get('state'), 'last_state':c.get('lastState')} for c in status.get('containerStatuses',[])]}

    def watch_pods(self):
        # Watch transitions so short-lived failed Pods remain diagnosable after
        # the Operator removes a superseded Job. Never record Pod env or tokens.
        decoder, buffer = json.JSONDecoder(), ''
        captured = set()
        with (self.directory/'pod-events.jsonl').open('w') as events, (self.directory/'pod-watch.log').open('w') as errors:
            self.pod_watch = subprocess.Popen(['kubectl','--context',self.a.context,'-n',self.a.ns,'get','pods','-l','app.kubernetes.io/name=artifactory-pi','--watch','--output-watch-events','-o','json'], stdout=subprocess.PIPE, stderr=errors)
            try:
                while chunk := self.pod_watch.stdout.read1(65536):
                    buffer += chunk.decode()
                    while buffer.strip():
                        buffer = buffer.lstrip()
                        try:
                            event, end = decoder.raw_decode(buffer)
                        except json.JSONDecodeError:
                            break
                        buffer = buffer[end:]
                        pod = event.get('object',{})
                        if pod.get('kind') != 'Pod':
                            continue
                        state = self.pod_state(pod)
                        events.write(json.dumps({'time':time.time(), 'event':event['type'], **state})+'\n')
                        events.flush()
                        if state['uid'] not in captured and any('terminated' in c['state'] for c in state['containers']):
                            captured.add(state['uid'])
                            try:
                                log = self.a.kubectl('logs',state['name'],'--tail=80','--request-timeout=5s')
                                (self.directory/(state['name']+'.log')).write_text(log)
                            except RuntimeError:
                                pass
            finally:
                self.pod_watch.stdout.close()

    def api_counters(self):
        result = {}
        for line in self.a.command(['kubectl','--context',self.a.context,'get','--raw','/metrics']).splitlines():
            if line.startswith('apiserver_request_total{') and 'group="agents.artifactory.dev"' in line:
                labels, value = line.rsplit(' ',1)
                if re.search(r'verb="(CREATE|UPDATE|PATCH|DELETE)"', labels):
                    result[labels] = float(value)
        return result

    def monitor(self, stop):
        next_observation = 0
        while not stop.is_set():
            tick = time.monotonic()
            try:
                self.a.api('/api/v1/overview')
                self.samples.append((time.monotonic()-tick)*1000)
                if tick >= next_observation:
                    self.observations.append(self.observation())
                    next_observation = tick+10
            except Exception as error:
                self.errors.append(type(error).__name__)
            stop.wait(max(0, 1-(time.monotonic()-tick)))

    def fingerprint(self, project):
        return json.loads(self.a.sql('planner', "SELECT coalesce(json_agg(json_build_object('id',id,'generation',lifecycle->'generation','calls',lifecycle->'calls','deadline',lifecycle->'deadline_ms','snapshot_id',snapshot_id,'snapshot_digest',snapshot_digest) ORDER BY id),'[]') FROM runtime_runs WHERE project_id='"+project+"' AND status='running'"))

    def run(self):
        self.preflight()
        self.a.resume()
        assert self.a.sql('planner', "SELECT count(*) FROM runtime_runs WHERE status IN ('running','provisioning')") == '0', 'Use a drained dedicated namespace'
        original = json.loads(self.a.kubectl('get','agentpool','default','-o','json'))['spec']
        baseline_crds = len(json.loads(self.a.kubectl('get','agentruns','-o','json'))['items'])
        self.evidence['baseline_agentrun_crds'] = baseline_crds
        self.evidence['images'] = self.a.images()
        stop = threading.Event()
        thread = threading.Thread(target=self.monitor,args=(stop,),daemon=True)
        watcher = threading.Thread(target=self.watch_pods,daemon=True)
        started = time.monotonic()
        try:
            self.a.kubectl('patch','agentpool','default','--type=merge','-p',json.dumps({'spec':{'maxActive':self.args.active,'maxPerProject':self.args.active,'cpu':self.args.cpu,'memory':self.args.memory}}))
            self.a.service_request(39443,'/test/mode',{'hold':True})
            project = self.a.api('/api/v1/projects',{'name':'active-capacity-'+uuid.uuid4().hex[:8],'repository_url':'https://upstream:9443/repo.git','planner_backend':'kubernetes'})['id']
            self.project = project
            self.evidence['project_id'] = project
            self.evidence['apiserver_mutations_before'] = self.api_counters()
            thread.start()
            watcher.start()
            for _ in range(self.args.active+self.args.queued):
                self.builds.append(self.a.api('/api/v1/projects/'+project+'/builds',{'ref':'v-fixture','goal':'Inspect the fixed source and build dist/result.txt','target_requirements':[{'os':'linux','arch':self.a.arch}]})['id'])

            def active():
                value = json.loads(self.a.sql('planner',"SELECT json_build_object('running',count(*) FILTER(WHERE status='running' AND (lifecycle->>'calls')::int>=1),'active',count(*) FILTER(WHERE status IN ('running','provisioning')),'queued',count(*) FILTER(WHERE status='queued'),'bad',count(*) FILTER(WHERE status IN ('failed','succeeded','needs_input'))) FROM runtime_runs WHERE project_id='"+project+"'"))
                assert value['active'] <= self.args.active, 'Pool admission overflowed'
                assert not value['bad'], 'Pi failed before the load window'
                if value['running'] != self.args.active or value['queued'] != self.args.queued:
                    return None
                owners = set(json.loads(self.a.sql('planner', "SELECT json_agg(lifecycle->>'owner') FROM runtime_runs WHERE project_id='"+project+"' AND status='running'")))
                pods = json.loads(self.a.kubectl('get','pods','-l','app.kubernetes.io/name=artifactory-pi','-o','json'))['items']
                live = {p['metadata']['uid'] for p in pods if p['status']['phase'] == 'Running' and not p['metadata'].get('deletionTimestamp')}
                # A crashed process can retain its live lease for 90 seconds.
                # Wait for real current owners, including after Runtime restart.
                return value if len(live) == self.args.active and owners == live else None

            counts = self.a.wait(active,self.args.startup_seconds)
            self.evidence['startup_seconds'] = round(time.monotonic()-started,3)
            print('ACTIVE PI ACQUIRED',counts,flush=True)
            first = self.fingerprint(project)
            self.evidence['apiserver_mutations_before_hold'] = self.api_counters()
            assert len(first) == self.args.active and len({r['id'] for r in first}) == self.args.active
            assert self.a.sql('planner',"SELECT count(*) FROM runtime_runs WHERE project_id='"+project+"' AND status='queued' AND ((lifecycle->>'calls')::int<>0 OR lifecycle->>'deadline_ms' IS NOT NULL)") == '0'
            end = time.monotonic()+self.args.hold_seconds
            while time.monotonic() < end:
                assert active(), 'Active count or quota changed during the hold window'
                time.sleep(min(5,end-time.monotonic()))
            held = self.observation()
            self.observations.append(held)
            self.evidence['held_observation'] = held
            self.save()
            print('HELD PODS',json.dumps({'running':held['pi_running_pods'],'missing_owners':held['running_owners_without_running_pod'],'unowned_pods':held['running_pods_without_runtime_owner']}),flush=True)
            assert held['pi_running_pods'] == self.args.active, 'Database counts must correspond to real Pods'
            assert not held['running_owners_without_running_pod'] and not held['running_pods_without_runtime_owner'], 'Each running Pi must hold the current execution identity'
            assert held['agentrun_crds'] <= baseline_crds + self.args.active, 'Queued runs generated extra AgentRun CRDs'
            assert self.fingerprint(project) == first, 'Hold must not reset budgets or change snapshots'
            self.evidence['apiserver_mutations_after_hold'] = self.api_counters()
            before_hold = self.evidence['apiserver_mutations_before_hold']
            hold_writes = sum(v-before_hold.get(k,0) for k,v in self.evidence['apiserver_mutations_after_hold'].items())
            self.evidence['hold_crd_mutations'] = hold_writes
            assert hold_writes <= 2*self.args.active, 'Steady heartbeats caused excessive CRD writes'
            (self.directory/'runtime-before-restart.log').write_text(self.a.kubectl('logs','-l','app.kubernetes.io/name=artifactory-runtime','--prefix','--tail=200'))
            old_services = {p['metadata']['uid'] for p in json.loads(self.a.kubectl('get','pods','-o','json'))['items'] if p['metadata'].get('labels',{}).get('app.kubernetes.io/name') in ['artifactory-runtime','artifactory-operator']}
            restarted = time.monotonic()
            for deployment in ['native-runtime','native-operator']:
                self.a.kubectl('rollout','restart','deployment/'+deployment)
                self.a.kubectl('rollout','status','deployment/'+deployment,'--timeout=120s')
            self.a.wait(lambda: not old_services.intersection(p['metadata']['uid'] for p in json.loads(self.a.kubectl('get','pods','-o','json'))['items']),120)
            self.evidence['old_runtime_and_operator_pods_deleted'] = True
            prior = {r['id']:r for r in first}

            def recovered_active():
                if not active():
                    return False
                values = self.fingerprint(project)
                # Every held request was waiting on the old Runtime process.
                # Its replacement must accept a new, budgeted model request;
                # live leases during Pod termination do not prove recovery.
                return len(values) == self.args.active and all(r['id'] in prior and r['calls'] > prior[r['id']]['calls'] for r in values)

            self.a.wait(recovered_active,self.args.startup_seconds)
            self.evidence['restart_new_model_reservation_per_run'] = True
            recovery_end = time.monotonic()+15
            while time.monotonic() < recovery_end:
                assert recovered_active(), 'Recovered Pi must remain active on the replacement Runtime'
                time.sleep(min(3,recovery_end-time.monotonic()))
            self.evidence['restart_stable_seconds'] = 15
            self.evidence['restart_real_active_recovery_seconds'] = round(time.monotonic()-restarted,3)
            self.evidence['recovered_observation'] = self.observation()
            recovered = self.fingerprint(project)
            assert {r['id'] for r in recovered} == set(prior), 'Restart must resume the original logical runs'
            for value in recovered:
                previous = prior[value['id']]
                assert all(value[k] == previous[k] for k in ['snapshot_id','snapshot_digest','deadline']), 'Restart changed fixed source or deadline'
                assert value['generation'] >= previous['generation'] and previous['calls'] <= value['calls'] <= 20, 'Restart reset or exceeded cumulative budgets'
            self.evidence['restart_preserved_sources_deadlines_and_cumulative_budgets'] = True
            self.evidence['recovered_generations'] = {str(g):sum(r['generation']==g for r in recovered) for g in {r['generation'] for r in recovered}}
            self.evidence['recovered_model_calls'] = sum(r['calls'] for r in recovered)
            canceled = time.monotonic()
            with ThreadPoolExecutor(max_workers=10) as executor:
                list(executor.map(lambda build:self.a.api('/api/v1/builds/'+build+'/cancel',{}),self.builds))
            self.a.wait(lambda:self.a.sql('planner',"SELECT count(*) FROM runtime_runs WHERE project_id='"+project+"' AND status<>'canceled'")=='0',60)
            self.evidence['cancel_authority_seconds'] = round(time.monotonic()-canceled,3)
            self.a.wait(lambda:not json.loads(self.a.kubectl('get','pods','-l','app.kubernetes.io/name=artifactory-pi','-o','json'))['items'],120)
            self.evidence['cancel_physical_pods_seconds'] = round(time.monotonic()-canceled,3)
            assert self.a.sql('business',"SELECT count(*) FROM jobs WHERE build_id IN (SELECT id FROM builds WHERE project_id='"+project+"')") == '0'
            assert self.a.sql('business',"SELECT count(*) FROM planning_events WHERE kind IN ('model_call','usage','checkpoint') AND run_id IN (SELECT id FROM planning_runs WHERE build_id IN (SELECT id FROM builds WHERE project_id='"+project+"'))") == '0'
            assert self.a.sql('planner',"SELECT count(*) FROM runtime_runs WHERE project_id='"+project+"' AND (lifecycle->>'calls')::int>(lifecycle->>'max_calls')::int") == '0'
            p95 = sorted(self.samples)[max(0,math.ceil(len(self.samples)*.95)-1)]
            self.evidence.update({'overview_samples':len(self.samples),'overview_p95_ms':round(p95,3),'errors':self.errors,'observations':self.observations,'fingerprints_sha256':hashlib.sha256(json.dumps(first,sort_keys=True).encode()).hexdigest(),'real_active_pods':held['pi_running_pods'],'pi_peak_total_rss_bytes':max(sum(p['rss_bytes'] for p in o['pods'] if p['name'].startswith('pi-')) for o in self.observations),'business_jobs_after_cancel':0,'business_pi_hot_events':0})
            self.evidence['apiserver_mutations_after_cancel'] = self.api_counters()
            assert not self.errors and p95 < 1000, 'Local business API SLO failed'
            self.evidence['passed'] = True
            print('ACTIVE CAPACITY PASSED',self.directory/'evidence.json',flush=True)
        except BaseException as error:
            self.evidence['failure'] = {'type':type(error).__name__, 'message':str(error)[:300]}
            # Capture the original failure before cancellation adds expected
            # fencing errors, and before a later rollout discards Pod logs.
            for name in ['runtime', 'operator']:
                try:
                    log = self.a.kubectl('logs','-l','app.kubernetes.io/name=artifactory-'+name,'--prefix','--tail=1000')
                    (self.directory/(name+'-failure.log')).write_text(log)
                except RuntimeError:
                    pass
            raise
        finally:
            stop.set()
            if thread.is_alive(): thread.join(timeout=40)
            if self.pod_watch is not None:
                self.pod_watch.terminate()
            if watcher.is_alive(): watcher.join(timeout=10)
            self.evidence.update({'elapsed_seconds':round(time.monotonic()-started,3),'overview_samples':len(self.samples),'errors':self.errors,'observations':self.observations,'observed_peak_running_pi_pods':max((o['pi_running_pods'] for o in self.observations),default=0)})
            self.save()
            for build in self.builds:
                if self.a.api('/api/v1/builds/'+build)['status'] not in ['succeeded','failed','canceled','lost']:
                    self.a.api('/api/v1/builds/'+build+'/cancel',{})
            if self.project:
                self.a.wait(lambda:self.a.sql('planner',"SELECT count(*) FROM runtime_runs WHERE project_id='"+self.project+"' AND status NOT IN ('canceled','failed','succeeded','needs_input')")=='0',60)
            self.a.service_request(39443,'/test/mode',{})
            self.a.kubectl('patch','agentpool','default','--type=merge','-p',json.dumps({'spec':original}))
            self.save()
            self.a.cleanup()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--context',required=True)
    parser.add_argument('--namespace',required=True)
    parser.add_argument('--active',type=int,default=100)
    parser.add_argument('--queued',type=int,default=10)
    parser.add_argument('--hold-seconds',type=int,default=60)
    parser.add_argument('--startup-seconds',type=int,default=480)
    parser.add_argument('--cpu',default='25m',help='Explicit fixture load request, not a production recommendation')
    parser.add_argument('--memory',default='64Mi',help='Explicit fixture load request, not a production recommendation')
    parser.add_argument('--node-memory-mib',type=int,help='Optional per-node hard cap when kind reports host memory above its container limit')
    args = parser.parse_args()
    if not 1 <= args.active <= 1000 or not 0 <= args.queued <= 10000 or not 10 <= args.hold_seconds <= 180:
        parser.error('active 1..1000, queued 0..10000, hold 10..180 seconds')
    Capacity(args).run()
