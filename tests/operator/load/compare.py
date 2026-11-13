#!/usr/bin/env python3
"""Compare real Poll and Kubernetes Pi against one frozen repository in kind.

Reuses only an existing private acceptance installation. Credentials remain in
memory/Kubernetes Secrets; the exported evidence contains measurements and IDs.
"""
import argparse
import hashlib
import importlib.util
import json
import math
from pathlib import Path
import random
import threading
import time
import uuid

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location("operator_acceptance", ROOT / "tests/operator/acceptance/run.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class Comparison:
    def __init__(self, args):
        self.args = args
        self.a = module.Acceptance(args.context, args.namespace, resume=True)
        self.a.evidence = json.loads((self.a.run / "evidence.json").read_text())
        self.directory = ROOT / ".local/operator-load" / (time.strftime("%Y%m%d-%H%M%S") + "-compare")
        self.directory.mkdir(parents=True)
        self.suffix = uuid.uuid4().hex[:8]
        self.created = []
        self.workers = []
        self.results = {}

    def stats(self):
        nodes = json.loads(self.a.command(["kubectl", "--context", self.a.context, "get", "nodes", "-o", "json"]))
        current = json.loads(self.a.kubectl('get', 'pods', '-o', 'json'))['items']
        live = {p['metadata']['uid']: p for p in current if p['status']['phase'] == 'Running' and not p['metadata'].get('deletionTimestamp')}
        pods = []
        for node in nodes["items"]:
            summary = json.loads(self.a.command(["kubectl", "--context", self.a.context, "get", "--raw", "/api/v1/nodes/" + node["metadata"]["name"] + "/proxy/stats/summary"]))
            for pod in summary["pods"]:
                if pod["podRef"]["namespace"] != self.a.ns or pod['podRef']['uid'] not in live:
                    continue
                pods.append({"name": pod["podRef"]["name"], "uid": pod['podRef']['uid'], "image_ids": [c.get('imageID') for c in live[pod['podRef']['uid']]['status'].get('containerStatuses', [])], "cpu_ns": pod.get("cpu", {}).get("usageCoreNanoSeconds", 0), "rss_bytes": pod.get("memory", {}).get("rssBytes", 0), "working_set_bytes": pod.get("memory", {}).get("workingSetBytes", 0), "rx_bytes": pod.get("network", {}).get("rxBytes", 0), "tx_bytes": pod.get("network", {}).get("txBytes", 0)})
        return {"time": time.time(), "pods": pods}

    def database(self):
        statement = """SELECT json_build_object('databases',
          (SELECT json_agg(s) FROM (SELECT datname,numbackends,xact_commit,xact_rollback,tup_inserted,tup_updated,blks_read,blks_hit,deadlocks FROM pg_stat_database WHERE datname IN ('business','planner','sources')) s),
          'activity',(SELECT json_agg(s) FROM (SELECT datname,wait_event_type,count(*) FROM pg_stat_activity WHERE datname IN ('business','planner','sources') GROUP BY datname,wait_event_type) s))"""
        return json.loads(self.a.sql("postgres", statement))

    def source_bytes(self):
        return {db: int(self.a.sql(db, "SELECT coalesce(sum(git_download_bytes+lfs_download_bytes),0) FROM source_repositories")) for db in ["business", "sources"]}

    def git_trace_events(self):
        return int(self.a.kubectl('exec', 'deploy/control-plane', '--', 'sh', '-ec', 'if test -f /tmp/load-git-trace.json; then wc -l < /tmp/load-git-trace.json; else echo 0; fi').strip())

    def prepare_repository(self):
        payload = random.Random(20261112).randbytes(self.args.payload_mib * 1024 * 1024)
        tag = "load-" + hashlib.sha256(payload).hexdigest()[:16]
        # Keep v-fixture and all existing snapshots; add a separate immutable tag.
        script = 'cd /tmp/upstream/repo.git && cat > load-payload.bin && git -c user.name=OperatorLoad -c user.email=load@localhost -c commit.gpgsign=false add load-payload.bin && (git -c user.name=OperatorLoad -c user.email=load@localhost -c commit.gpgsign=false commit -m "fixed load dataset" || test -z "$(git status --porcelain)") && (git rev-parse --verify refs/tags/' + tag + ' >/dev/null 2>&1 || git tag ' + tag + ') && git rev-parse ' + tag
        commit = self.a.kubectl("exec", "-i", "deploy/upstream", "--", "sh", "-ec", script, data=payload).strip().splitlines()[-1]
        if len(commit) != 40:
            raise ValueError("fixed load commit missing")
        assert self.a.kubectl('exec', 'deploy/upstream', '--', 'sha256sum', '/tmp/upstream/repo.git/load-payload.bin').split()[0] == hashlib.sha256(payload).hexdigest(), 'Fixed dataset transfer must preserve every byte'
        self.dataset = {"ref": tag, "commit": commit, "payload_bytes": len(payload), "payload_sha256": hashlib.sha256(payload).hexdigest()}

    def enroll_poll_workers(self):
        ca = (self.a.run / "pki/ca.pem").read_text()
        self.a.apply({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "load-ca-" + self.suffix}, "data": {"ca.pem": ca}})
        for index in range(self.args.concurrency):
            name = f"poll-load-{self.suffix}-{index}"
            token = self.a.api("/api/v1/ai-enrollment-tokens", {})["token"]
            secret = self.a.secret(name, {"ENROLLMENT_TOKEN": token})
            workload = self.a.workload(name, "localhost:5001/artifact-kit/operator-pi:rust-native", [], env={"ARTIFACTORY_AI_CONFIG": "/tmp/worker.json", "ARTIFACTORY_AI_ENDPOINT": "control-plane:8443", "ARTIFACTORY_AI_CA_FILE": "/run/load-ca/ca.pem", "ARTIFACTORY_AI_SERVER_NAME": "control-plane", "ARTIFACTORY_AI_NAME": name, "NODE_EXTRA_CA_CERTS": "/run/load-ca/ca.pem"}, secret=name)
            pod = workload["spec"]["template"]["spec"]
            pod["automountServiceAccountToken"] = False
            pod["containers"][0]["command"] = ["sh", "-ec", 'printf "%s" "$ENROLLMENT_TOKEN" | node dist/main.js enroll; unset ENROLLMENT_TOKEN; exec node dist/main.js']
            pod["containers"][0]["volumeMounts"] = [{"name": "ca", "mountPath": "/run/load-ca", "readOnly": True}]
            pod["volumes"] = [{"name": "ca", "configMap": {"name": "load-ca-" + self.suffix}}]
            self.a.apply(secret, workload)
            self.workers.append(name)
        self.a.wait(lambda: int(self.a.sql("business", "SELECT count(*) FROM ai_workers WHERE NOT revoked AND name LIKE 'poll-load-" + self.suffix + "-%'")) == self.args.concurrency)

    def scenario(self, backend):
        profile = next(p["id"] for p in self.a.api("/api/v1/model-profiles") if p["name"] == "Operator fixture")
        project = self.a.api("/api/v1/projects", {"name": backend + " load " + self.suffix, "repository_url": "https://upstream:9443/repo.git", "planner_backend": backend, "model_profile_id": profile})["id"]
        self.a.kubectl("patch", "agentpool", "default", "--type=merge", "-p", json.dumps({"spec": {"maxActive": self.args.concurrency if backend == "kubernetes" else 0, "maxPerProject": self.args.concurrency}}))
        self.a.service_request(39443, "/test/mode", {"hold": True})
        samples, failures, observations = [], [], []
        stop = threading.Event()
        started = time.monotonic()
        before = self.source_bytes()
        before_db = self.database()
        git_before = self.git_trace_events()

        def monitor():
            next_stats = 0
            while not stop.is_set():
                tick = time.monotonic()
                try:
                    self.a.api("/api/v1/overview")
                    samples.append((time.monotonic() - tick) * 1000)
                except Exception as error:
                    failures.append(type(error).__name__)
                if tick >= next_stats:
                    try:
                        observations.append({"resources": self.stats(), "pg": self.database()})
                    except Exception as error:
                        failures.append("metrics:" + type(error).__name__)
                    next_stats = tick + 5
                stop.wait(max(0, 0.2 - (time.monotonic() - tick)))

        thread = threading.Thread(target=monitor, daemon=True)
        thread.start()
        builds = []
        try:
            for _ in range(self.args.concurrency):
                build = self.a.api(f"/api/v1/projects/{project}/builds", {"ref": self.dataset["ref"], "goal": "Inspect README.md and build dist/result.txt", "target_requirements": [{"os": "linux", "arch": self.a.arch}]})["id"]
                builds.append(build)
                self.created.append(build)

            def all_running():
                for build in builds:
                    detail = self.a.api('/api/v1/builds/' + build)
                    if detail['status'] in ['failed', 'canceled', 'lost', 'needs_input']:
                        raise AssertionError('load startup failed: ' + json.dumps({k: detail.get(k) for k in ['id', 'status', 'error']}))
                if backend == "poll":
                    query = f"SELECT count(*) FROM planning_runs WHERE build_id IN ({','.join(repr(b) for b in builds)}) AND state='leased' AND calls>=1"
                    return int(self.a.sql("business", query)) == len(builds)
                query = f"SELECT count(*) FROM runtime_runs WHERE project_id='{project}' AND status='running' AND (lifecycle->>'calls')::int>=1"
                return int(self.a.sql("planner", query)) == len(builds)

            self.a.wait(all_running, 240)
            acquired = time.monotonic() - started
            print(backend, "all Pi acquired; holding", self.args.hold_seconds, "seconds", flush=True)
            end_hold = time.monotonic() + self.args.hold_seconds
            while time.monotonic() < end_hold:
                time.sleep(min(1, end_hold - time.monotonic()))
            self.a.service_request(39443, "/test/mode", {})

            def completed():
                details = [self.a.api("/api/v1/builds/" + build) for build in builds]
                bad = [d for d in details if d["status"] in ["failed", "lost", "canceled", "needs_input"]]
                if bad:
                    raise AssertionError("load build terminated: " + json.dumps([{k: d.get(k) for k in ["id", "status", "error"]} for d in bad]))
                return details if all(d["status"] == "succeeded" for d in details) else None

            details = self.a.wait(completed, 300)
            expected = hashlib.sha256(b"operator-native-ok\n").hexdigest()
            assert all(d["commit_sha"] == self.dataset["commit"] and d["artifacts"] and d["artifacts"][0]["sha256"] == expected for d in details)
            # Include a fresh kubelet sample after Git/clone/plan acceptance.
            observations.append({"resources": self.stats(), "pg": self.database()})
        finally:
            stop.set()
            thread.join(timeout=40)
            self.a.service_request(39443, "/test/mode", {})
        after = self.source_bytes()
        p95 = sorted(samples)[max(0, math.ceil(len(samples) * .95) - 1)] if samples else None
        control = [p for o in observations for p in o["resources"]["pods"] if p["name"].startswith("control-plane-")]
        result = {"backend": backend, "build_ids": builds, "elapsed_seconds": round(time.monotonic() - started, 3), "acquisition_seconds": round(acquired, 3), "overview_samples": len(samples), "overview_p95_ms": round(p95, 3) if p95 is not None else None, "errors": failures, "source_download_bytes_delta": {db: after[db] - before[db] for db in before}, "control_peak_rss_bytes": max((p["rss_bytes"] for p in control), default=0), "pg_before": before_db, "pg_after": self.database(), "observations": observations}
        if len(control) > 1:
            assert len({p['uid'] for p in control}) == 1, 'Do not compare counters across replaced control Pods'
            for field in ["cpu_ns", "rx_bytes", "tx_bytes"]:
                result["control_" + field + "_delta"] = control[-1][field] - control[0][field]
                assert result['control_' + field + '_delta'] >= 0, 'Resource counter reset requires a fresh comparison'
        else:
            raise AssertionError('At least two live control Pod resource samples are required')
        self.results[backend] = result
        result['business_git_trace_events_delta'] = self.git_trace_events() - git_before
        self.save()
        assert not failures and p95 is not None and p95 < 1000, "local benchmark overview SLO failed"
        assert result["source_download_bytes_delta"]["business"] > 0 if backend == "poll" else result["source_download_bytes_delta"]["business"] == 0
        if backend == "kubernetes":
            assert result['business_git_trace_events_delta'] == 0, 'Kubernetes source processing spawned business Git operations'
            assert result["source_download_bytes_delta"]["sources"] > 0
            assert self.a.sql("business", f"SELECT count(*) FROM planning_events WHERE run_id IN (SELECT id FROM planning_runs WHERE build_id IN ({','.join(repr(b) for b in builds)})) AND kind IN ('model_call','usage','checkpoint')") == "0"
        print(backend, "PASS", {k: v for k, v in result.items() if k in ["elapsed_seconds", "overview_samples", "overview_p95_ms", "source_download_bytes_delta", "control_peak_rss_bytes"]}, flush=True)

    def save(self):
        evidence = {"context": self.a.context, "namespace": self.a.ns, "dataset": self.dataset, "concurrency": self.args.concurrency, "hold_seconds": self.args.hold_seconds, "local_slo": {"overview_p95_ms_less_than": 1000, "errors": 0, "scope": "isolated local acceptance; production target requires deployment-specific review"}, "scenarios": self.results}
        (self.directory / "evidence.json").write_text(json.dumps(evidence, indent=2))

    def run(self):
        # Poll's Git client must trust the same private fixture CA as Source.
        self.a.kubectl('set', 'env', 'deployment/control-plane', 'GIT_SSL_CAINFO=/run/pki/ca.pem', 'GIT_TRACE2_EVENT=/tmp/load-git-trace.json', 'ARTIFACTORY_PUBLIC_URL=http://control-plane:8080')
        self.a.kubectl('rollout', 'status', 'deployment/control-plane', '--timeout=120s')
        self.a.resume()
        original_pool = json.loads(self.a.kubectl("get", "agentpool", "default", "-o", "json"))["spec"]
        try:
            self.prepare_repository()
            self.enroll_poll_workers()
            for backend in ["poll", "kubernetes"]:
                self.scenario(backend)
            print("COMPARISON PASSED", self.directory / "evidence.json", flush=True)
        finally:
            for build in self.created:
                try:
                    if self.a.api("/api/v1/builds/" + build)["status"] not in ["succeeded", "failed", "canceled", "lost"]:
                        self.a.api("/api/v1/builds/" + build + "/cancel", {})
                except (OSError, RuntimeError):
                    pass
            for name in self.workers:
                self.a.kubectl("delete", "deployment,secret", name, "--ignore-not-found")
            self.a.kubectl("delete", "configmap", "load-ca-" + self.suffix, "--ignore-not-found")
            self.a.kubectl("patch", "agentpool", "default", "--type=merge", "-p", json.dumps({"spec": original_pool}))
            self.a.cleanup()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--context", required=True)
    parser.add_argument("--namespace", required=True)
    parser.add_argument("--concurrency", type=int, default=4)
    parser.add_argument("--hold-seconds", type=int, default=60)
    parser.add_argument("--payload-mib", type=int, default=8)
    args = parser.parse_args()
    if not 1 <= args.concurrency <= 32 or not 10 <= args.hold_seconds <= 180 or not 1 <= args.payload_mib <= 64:
        parser.error("use concurrency 1..32, hold 10..180 seconds, and payload 1..64 MiB")
    Comparison(args).run()
