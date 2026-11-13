import base64
import json
import os
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("init-helm.py")


class HelmBootstrapTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which('helm'), 'requires helm')
    def test_replicated_control_requires_source_isolation_and_renders_ha_resources(self):
        chart = SCRIPT.parents[1] / 'charts/artifactory'
        invalid = subprocess.run(['helm', 'template', 'ha-test', str(chart), '--set', 'control.replicas=3'],
                                 capture_output=True, text=True)
        self.assertNotEqual(invalid.returncode, 0)
        self.assertIn('localSource.enabled', invalid.stderr)
        result = subprocess.run(['helm', 'template', 'ha-test', str(chart), '-f', str(chart / 'values-ha.yaml')],
                                check=True, capture_output=True, text=True)
        self.assertIn('replicas: 3', result.stdout)
        self.assertIn('command: [artifactory-control, serve-source]', result.stdout)
        self.assertIn('emptyDir: {sizeLimit: 10Gi}', result.stdout)
        self.assertIn('kind: PodDisruptionBudget', result.stdout)
        self.assertIn('https://ha-test-local-source:8080', result.stdout)
        self.assertIn('tls_trust_pool file /run/pki/ca.pem', result.stdout)
        self.assertIn('ARTIFACTORY_DB_MAX_CONNECTIONS: "40"', result.stdout)
        self.assertIn('path: /readyz', result.stdout)

    @unittest.skipUnless(shutil.which('helm'), 'requires helm')
    def test_external_stateful_services_are_not_created(self):
        chart = SCRIPT.parents[1] / 'charts/artifactory'
        result = subprocess.run(['helm', 'template', 'external', str(chart), '--set',
                                 'postgres.enabled=false,s3.enabled=false,s3.endpoint=https://objects.example.com'],
                                check=True, capture_output=True, text=True)
        self.assertNotIn('name: external-postgres\n', result.stdout)
        self.assertNotIn('name: external-s3\n', result.stdout)
        self.assertIn('ARTIFACTORY_S3_ENDPOINT: "https://objects.example.com"', result.stdout)

    @unittest.skipUnless(shutil.which('helm'), 'requires helm')
    def test_registration_defaults_on_and_can_be_disabled(self):
        chart = SCRIPT.parents[1] / 'charts/artifactory'
        for enabled in ['true', 'false']:
            with self.subTest(enabled=enabled):
                command = ['helm', 'template', 'registration-test', str(chart)]
                if enabled == 'false':
                    command.extend(['--set', 'auth.registrationEnabled=false'])
                result = subprocess.run(command, check=True, capture_output=True, text=True)
                self.assertTrue(
                    f'ARTIFACTORY_REGISTRATION_ENABLED: "{enabled}"' in result.stdout,
                    'rendered registration setting does not match the requested value',
                )

    def test_persistent_secrets_and_certificate_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "private"
            command = [sys.executable, str(SCRIPT), "--output", str(output),
                       "--namespace", "test-builds", "--release", "test-kit",
                       "--grpc-host", "127.0.0.1", "--grpc-host", "agents.example.com"]
            subprocess.run(command, check=True, capture_output=True)
            before = {str(path): path.read_bytes() for path in output.rglob("*") if path.is_file()}
            secret = json.loads((output / "credentials.json").read_text())
            data = secret["stringData"]
            self.assertEqual(secret["metadata"]["namespace"], "test-builds")
            self.assertEqual(len(base64.b64decode(data["master-key"])), 32)
            self.assertIn(data["database-password"] + "@test-kit-postgres:5432/", data["database-url"])
            self.assertEqual(json.loads((output / "access.json").read_text())["password"],
                             data["bootstrap-password"])
            identity = json.loads(data["s3.json"])["identities"][0]["credentials"][0]
            self.assertEqual(identity["secretKey"], data["s3-secret-key"])
            subprocess.run(["openssl", "verify", "-CAfile", str(output / "pki/ca.pem"),
                            "-verify_hostname", "agents.example.com", str(output / "pki/server.pem")],
                           check=True, capture_output=True)
            subprocess.run(["openssl", "verify", "-CAfile", str(output / "pki/ca.pem"),
                            "-verify_ip", "127.0.0.1", str(output / "pki/server.pem")],
                           check=True, capture_output=True)
            subprocess.run(["openssl", "verify", "-CAfile", str(output / "pki/ca.pem"),
                            "-verify_hostname", "test-kit-local-source", str(output / "pki/server.pem")],
                           check=True, capture_output=True)
            if os.name != "nt":
                for path in output.rglob("*"):
                    self.assertEqual(path.stat().st_mode & 0o077, 0, str(path))
            repeated = subprocess.run(command, capture_output=True, text=True)
            self.assertNotEqual(repeated.returncode, 0)
            self.assertIn("refusing to rotate", repeated.stderr)
            self.assertEqual(before, {str(path): path.read_bytes()
                                      for path in output.rglob("*") if path.is_file()})

    def test_rejects_invalid_hosts_before_creating_secrets(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "private"
            result = subprocess.run([sys.executable, str(SCRIPT), "--output", str(output),
                                     "--grpc-host", "bad.example.com,IP:1.2.3.4"], capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
