#!/usr/bin/env python3
"""Restore all four TimescaleDB databases, S3, service keys and routing in isolated kind.

Creates fresh PostgreSQL/S3 Pods and promotes them behind stable Services. The
original data Pods remain available for rollback. Never targets a live project.
"""
import argparse
import base64
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import time

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('acceptance', Path(__file__).with_name('run.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class Recovery:
    def __init__(self, args):
        if args.context not in ['kind-artifact-operator-acceptance', 'kind-artifact-operator-acceptance-cilium', 'kind-operator'] or not args.namespace.startswith('artifact-operator-'):
            raise ValueError('This destructive drill only supports this task dedicated kind cluster')
        self.a = module.Acceptance(args.context, args.namespace, resume=True)
        self.a.evidence = json.loads((self.a.run / 'evidence.json').read_text())
        self.directory = ROOT / '.local/operator-load' / (time.strftime('%Y%m%d-%H%M%S') + '-recovery')
        self.directory.mkdir(parents=True)
        self.promoted = False
        self.restored = {name: name + '-restored-' + str(time.time_ns())[-8:] for name in ['postgres', 's3']}
        self.pg_restore = shutil.which('pg_restore')
        self.psql = shutil.which('psql')
        self.pg_port = str(args.postgres_port)
        if not self.pg_restore or not self.psql:
            raise ValueError('Install a PostgreSQL client compatible with the PostgreSQL 17 archive before running this drill')
        self.replicas = {'deploy/control-plane': 1, 'deploy/native-runtime': 1, 'deploy/native-operator': 2, 'deploy/native-telemetry':1, 'statefulset/sources': 1}

    def scale(self, stopped):
        if stopped:
            self.a.kubectl('scale', 'deploy/native-operator', '--replicas=0')
            self.a.wait(lambda: not json.loads(self.a.kubectl('get', 'pods', '-l', 'app.kubernetes.io/name=artifactory-operator', '-o', 'json'))['items'], 120)
        for resource, replicas in self.replicas.items():
            self.a.kubectl('scale', resource, '--replicas=' + str(0 if stopped else replicas))
        if stopped:
            self.a.wait(lambda: not any(p['metadata']['name'].startswith(('control-plane-', 'native-runtime-', 'native-operator-', 'native-telemetry-','sources-')) for p in json.loads(self.a.kubectl('get', 'pods', '-o', 'json'))['items']), 120)

    def telemetry_state(self):
        tables=['telemetry_batches','host_samples','process_samples','build_samples','inventory_versions','inventory_observations','latest_hosts','latest_inventories','host_minutes','host_hours','build_minutes','build_hours']
        state={table:self.a.sql('telemetry',f"SELECT md5(coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text)::text,'[]')) FROM {table} t") for table in tables}
        state['policies']=self.a.sql('telemetry',"SELECT md5(jsonb_agg(jsonb_build_array(job_id,proc_name,schedule_interval,config) ORDER BY job_id)::text) FROM timescaledb_information.jobs WHERE proc_name IN ('policy_retention','policy_refresh_continuous_aggregate','policy_compression')")
        state['columnstore_chunks']=self.a.sql('telemetry',"SELECT md5(jsonb_agg(jsonb_build_array(hypertable_name,chunk_name,is_compressed) ORDER BY chunk_name)::text) FROM timescaledb_information.chunks WHERE hypertable_schema='public'")
        return state

    def state(self):
        queries = {
            'business': "SELECT jsonb_build_object('builds',(SELECT jsonb_agg(jsonb_build_array(id,status,source_snapshot_id,current_plan_id,revision_count,planner_backend) ORDER BY id) FROM builds),'snapshots',(SELECT jsonb_agg(jsonb_build_array(id,digest,service_origin,public_origin) ORDER BY id) FROM source_snapshots),'receipts',(SELECT jsonb_agg(jsonb_build_array(event_id,input_hash,result,run_id,generation) ORDER BY event_id) FROM runtime_receipts))",
            'planner': "SELECT jsonb_agg(jsonb_build_array(id,status,lifecycle,checkpoint,usage,snapshot_id,snapshot_digest) ORDER BY id) FROM runtime_runs",
            'sources': "SELECT jsonb_agg(jsonb_build_array(id,digest,manifest,durable) ORDER BY id) FROM source_snapshots"}
        return {db: hashlib.sha256(self.a.sql(db, query).encode()).hexdigest() for db, query in queries.items()}

    def dump(self, db):
        path = self.directory / (db + '.dump')
        args = ['kubectl', '--context', self.a.context, '-n', self.a.ns, 'exec', 'service/postgres', '--', 'pg_dump', '-Fc', '-U', 'admin', '-d', db]
        with path.open('wb') as output:
            subprocess.run(args, stdout=output, stderr=subprocess.PIPE, check=True)
        return hashlib.sha256(path.read_bytes()).hexdigest()

    def clone(self, name):
        value = json.loads(self.a.kubectl('get', 'deploy', name, '-o', 'json'))
        target = self.restored[name]
        result = {'apiVersion': 'apps/v1', 'kind': 'Deployment', 'metadata': {'name': target}, 'spec': copy.deepcopy(value['spec'])}
        result['spec']['selector']['matchLabels'] = {'app': target}
        result['spec']['template']['metadata']['labels'] = {'app': target}
        result['spec']['replicas'] = 1
        if name == 'postgres':
            # The entrypoint's temporary initialization server uses only a Unix
            # socket. Wait for the final TCP server before opening a forward.
            result['spec']['template']['spec']['containers'][0]['readinessProbe'] = {'exec': {'command': ['pg_isready', '-h', '127.0.0.1', '-U', 'admin', '-d', 'business']}, 'periodSeconds': 1, 'failureThreshold': 120}
        elif name == 's3':
            result['spec']['template']['spec']['containers'][0]['readinessProbe'] = {'tcpSocket': {'port': 8333}, 'periodSeconds': 1, 'failureThreshold': 120}
        self.a.apply(result)
        self.a.kubectl('rollout', 'status', 'deploy/' + target, '--timeout=180s')

    def rclone(self, operation, source, target, port):
        secret = json.loads(self.a.kubectl('get', 'secret', 'source', '-o', 'json'))['data']
        values = {k: base64.b64decode(v).decode() for k, v in secret.items()}
        environment = dict(os.environ)
        environment.update({'RCLONE_CONFIG_DR_TYPE': 's3', 'RCLONE_CONFIG_DR_PROVIDER': 'Other', 'RCLONE_CONFIG_DR_ACCESS_KEY_ID': values['ARTIFACTORY_S3_ACCESS_KEY'], 'RCLONE_CONFIG_DR_SECRET_ACCESS_KEY': values['ARTIFACTORY_S3_SECRET_KEY'], 'RCLONE_CONFIG_DR_ENDPOINT': 'http://127.0.0.1:' + str(port), 'RCLONE_CONFIG_DR_FORCE_PATH_STYLE': 'true', 'RCLONE_CONFIG_DR_REGION': 'us-east-1'})
        args = ['rclone', operation, source, target, '--retries', '2', '--low-level-retries', '2', '--contimeout', '5s', '--timeout', '30s']
        if operation == 'check':
            args.append('--download')
        subprocess.run(args, env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True, timeout=180)

    def close_forwards(self):
        for process in self.a.processes:
            process.terminate()
        for process in self.a.processes:
            process.wait(timeout=10)
        self.a.processes.clear()

    def run(self):
        a = self.a
        a.resume()
        active = int(a.sql('planner', "SELECT count(*) FROM runtime_runs WHERE status NOT IN ('succeeded','failed','canceled')"))
        if active:
            raise AssertionError('Drain active Runtime runs before this recovery drill')
        pool = json.loads(a.kubectl('get', 'agentpool', 'default', '-o', 'json'))['spec']
        a.kubectl('patch', 'agentpool', 'default', '--type=merge', '-p', '{"spec":{"maxActive":0}}')
        a.wait(lambda: int(a.sql('planner', 'SELECT count(*) FROM runtime_outbox WHERE NOT delivered')) == 0)
        old_selectors = {name: json.loads(a.kubectl('get', 'service', name, '-o', 'json'))['spec']['selector'] for name in ['postgres', 's3']}
        try:
            self.close_forwards()
            self.scale(True)
            # Restore actual materialized and columnstore data, not only empty policy definitions.
            for view in ['host_minutes','build_minutes','host_hours','build_hours']:
                a.sql('telemetry',f"CALL refresh_continuous_aggregate('{view}',now()-interval '1 day',now()+interval '1 hour')")
            chunks=a.sql('telemetry',"SELECT format('%I.%I',chunk_schema,chunk_name) FROM timescaledb_information.chunks WHERE hypertable_schema='public' AND NOT is_compressed").splitlines()
            for chunk in chunks:
                a.sql('telemetry',f"CALL convert_to_columnstore('{chunk}'::regclass)")
            assert a.sql('telemetry',"SELECT count(*) FROM timescaledb_information.chunks WHERE hypertable_schema='public' AND is_compressed")!='0','Telemetry recovery needs populated columnstore chunks'
            before = self.state()
            telemetry_before=self.telemetry_state()
            dumps = {db: self.dump(db) for db in ['business', 'planner', 'sources','telemetry']}
            # Full private backup; only hashes are included in exported evidence.
            resources = [json.loads(a.kubectl('get', 'secret', name, '-o', 'json')) for name in ['control-plane', 'runtime', 'source','telemetry', 'service-tls', 'acceptance-pki']]
            routes = [json.loads(a.kubectl('get', kind, '-o', 'json')) for kind in ['agentpools', 'sourcepools']]
            (self.directory / 'private-keys-and-routes.json').write_text(json.dumps({'secrets': resources, 'routes': routes}))
            key_hash = hashlib.sha256(json.dumps([r['data'] for r in resources], sort_keys=True).encode()).hexdigest()
            a.forward('service/s3', '38333:8333')
            a.wait(lambda: self.s3_ready(38333))
            objects = self.directory / 'objects'
            self.rclone('copy', 'dr:operator-acceptance', str(objects), 38333)
            self.rclone('check', 'dr:operator-acceptance', str(objects), 38333)
            manifest = {str(p.relative_to(objects)): hashlib.sha256(p.read_bytes()).hexdigest() for p in objects.rglob('*') if p.is_file()}
            assert manifest, 'S3 backup must contain snapshots and artifacts'
            self.clone('postgres')
            self.clone('s3')
            a.forward('deploy/' + self.restored['postgres'], self.pg_port+':5432')
            postgres = json.loads(a.kubectl('get', 'deployment', 'postgres', '-o', 'json'))
            password = next(item['value'] for item in postgres['spec']['template']['spec']['containers'][0]['env'] if item['name'] == 'POSTGRES_PASSWORD')
            restore_environment = dict(os.environ, PGPASSWORD=password, PGCONNECT_TIMEOUT='2')
            # A listening kubectl port-forward can still target a PostgreSQL
            # entrypoint that is initializing roles/databases or restarting.
            a.wait(lambda: self.postgres_ready(restore_environment), 120)
            for db in ['business', 'planner', 'sources','telemetry']:
                path = self.directory / (db + '.dump')
                assert hashlib.sha256(path.read_bytes()).hexdigest() == dumps[db]
                # Use PostgreSQL's native protocol, not an exec stdin archive
                # transfer: truncated exec streams were caught by SHA-256 here.
                # Fresh databases already have their extension. Restore serially in Timescale restore mode.
                sql_args=[self.psql,'-v','ON_ERROR_STOP=1','-h','127.0.0.1','-p',self.pg_port,'-U','admin','-d',db,'-c']
                subprocess.run(sql_args+['SELECT timescaledb_pre_restore();'],env=restore_environment,capture_output=True,check=True,timeout=30)
                args = [self.pg_restore, '--exit-on-error', '-h', '127.0.0.1', '-p', self.pg_port, '-U', 'admin', '-d', db, str(path)]
                result = subprocess.run(args, env=restore_environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=180)
                if result.returncode:
                    (self.directory / (db + '-restore-private.log')).write_bytes(result.stderr)
                    raise RuntimeError('PostgreSQL restore failed; private diagnostics saved for ' + db)
                subprocess.run(sql_args+['SELECT timescaledb_post_restore();'],env=restore_environment,capture_output=True,check=True,timeout=30)
            a.apply(a.service(self.restored['s3'], [8333]))
            a.forward('service/' + self.restored['s3'], '38334:8333')
            a.wait(lambda: self.s3_ready(38334))
            self.rclone('copy', str(objects), 'dr:operator-acceptance', 38334)
            self.rclone('check', str(objects), 'dr:operator-acceptance', 38334)
            restored = json.loads((self.directory / 'private-keys-and-routes.json').read_text())
            for resource in restored['secrets']:
                a.apply({'apiVersion': 'v1', 'kind': 'Secret', 'metadata': {'name': resource['metadata']['name']}, 'data': resource['data']})
            for collection in restored['routes']:
                for resource in collection['items']:
                    a.apply({'apiVersion': resource['apiVersion'], 'kind': resource['kind'], 'metadata': {'name': resource['metadata']['name']}, 'spec': resource['spec']})
            assert key_hash == hashlib.sha256(json.dumps([r['data'] for r in restored['secrets']], sort_keys=True).encode()).hexdigest()
            for name in ['postgres', 's3']:
                a.kubectl('patch', 'service', name, '--type=merge', '-p', json.dumps({'spec': {'selector': {'app': self.restored[name]}}}))
            assert self.state() == before, 'Restored source identity, receipts, budgets and checkpoints must match'
            telemetry_after=self.telemetry_state()
            assert telemetry_after==telemetry_before,'Restored samples, inventories, aggregates, columnstore and policies must match'
            a.check('all four TimescaleDB databases restore with telemetry data intact',True)
            self.close_forwards()
            self.scale(False)
            a.kubectl('rollout', 'status', 'deploy/control-plane', '--timeout=180s')
            a.kubectl('rollout', 'status', 'deploy/native-runtime', '--timeout=180s')
            a.kubectl('rollout', 'status', 'statefulset/sources', '--timeout=180s')
            a.resume()
            a.forward('pod/sources-0', '38090:8090')
            a.service_request(39443, '/test/mode', {'git_down': True})
            a.kubectl('exec', 'sources-0', '--', 'sh', '-ec', 'mv /var/lib/artifactory-source/sources/repos /var/lib/artifactory-source/sources/repos.before-dr-' + str(int(time.time())) + '; mkdir /var/lib/artifactory-source/sources/repos')
            snapshot = a.evidence['source_snapshot_id']
            result = json.loads(a.wait(lambda: a.service_request(38090, '/v1/inspect', {'snapshot_id': snapshot, 'name': 'read_file', 'arguments': {'path': 'README.md'}})))
            assert 'Build with sh build.sh.' in result['content'], 'Fresh restored S3 must recover frozen bytes with Git unavailable'
            assert self.state() == before, 'Restart must not reset or revive terminal budgets'
            a.service_request(39443, '/test/mode', {})
            self.promoted = True
            evidence = {'context': a.context, 'namespace': a.ns, 'scope': 'isolated full data-service restore; production deployment requires its own backup store/key management', 'database_sha256': dumps, 'before': before, 'after': self.state(), 'telemetry_before':telemetry_before,'telemetry_after':telemetry_after,'service_keys_sha256': key_hash, 'object_count': len(manifest), 'objects_manifest_sha256': hashlib.sha256(json.dumps(manifest, sort_keys=True).encode()).hexdigest(), 'source_restored_without_cache_or_upstream': True, 'new_data_pods': list(self.restored.values()), 'old_data_pods_retained_for_rollback': True, 'e2e': False}
            evidence['images'] = a.images()
            (self.directory / 'evidence.json').write_text(json.dumps(evidence, indent=2))
            print('DATABASE/S3/KEY RESTORE PASSED', self.directory / 'evidence.json', flush=True)
            a.verify()
            evidence['e2e'] = True
            (self.directory / 'evidence.json').write_text(json.dumps(evidence, indent=2))
            print('RECOVERY ACCEPTANCE PASSED', self.directory / 'evidence.json', flush=True)
        finally:
            if not self.promoted:
                for name, selector in old_selectors.items():
                    a.kubectl('patch', 'service', name, '--type=merge', '-p', json.dumps({'spec': {'selector': selector}}))
                self.scale(False)
            a.kubectl('patch', 'agentpool', 'default', '--type=merge', '-p', json.dumps({'spec': pool}))
            a.cleanup()

    @staticmethod
    def s3_ready(port):
        # A TCP listener is sufficient before rclone's authenticated requests.
        import socket
        with socket.create_connection(('127.0.0.1', port), timeout=1):
            return True

    def postgres_ready(self, environment):
        try:
            result = subprocess.run([self.psql, '--no-psqlrc', '-h', '127.0.0.1', '-p', self.pg_port, '-U', 'admin', '-d', 'business', '-Atc', 'SELECT 1'], env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=5)
            return result.returncode == 0 and result.stdout.strip() == b'1'
        except subprocess.TimeoutExpired:
            return False


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--context', required=True)
    parser.add_argument('--namespace', required=True)
    parser.add_argument('--postgres-port',type=int,default=35433)
    Recovery(parser.parse_args()).run()
