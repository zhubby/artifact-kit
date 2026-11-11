import base64
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]


class LocalServiceCredentialTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / 'scripts').mkdir()
        for name in ('init-local.sh', 'local-services.sh'):
            shutil.copyfile(ROOT / 'scripts' / name, self.root / 'scripts' / name)
        pki = self.root / '.local/pki'
        pki.mkdir(parents=True)
        for name in ('ca.key', 'server.key'):
            (pki / name).write_text('existing-test-key')
        self.credentials = self.root / '.local/s3.json'
        self.original = b'{"identities":[{"name":"existing-test-identity"}]}\n'
        tools = self.root / 'tools'
        tools.mkdir()
        docker = tools / 'docker'
        docker.write_text('#!/bin/sh\nexit 79\n')
        docker.chmod(0o700)
        self.environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith('ARTIFACTORY_')
        }
        self.environment['PATH'] = str(tools) + os.pathsep + os.environ['PATH']
        self.settings = {
            'ARTIFACTORY_S3_ACCESS_KEY': 'test-access-key',
            'ARTIFACTORY_S3_SECRET_KEY': 'test-secret-key',
        }

    def start_services(self, settings):
        self.credentials.write_bytes(self.original)
        self.credentials.chmod(0o600)
        environment = self.root / '.local/env'
        environment.write_text(''.join(key + '=' + value + '\n' for key, value in settings.items()))
        result = subprocess.run(
            ['sh', 'scripts/local-services.sh'], cwd=self.root,
            env=self.environment, capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(environment.read_text(), ''.join(key + '=' + value + '\n' for key, value in settings.items()))
        return result

    def test_missing_settings_preserve_existing_credentials(self):
        for name in self.settings:
            with self.subTest(missing=name):
                result = self.start_services({key: value for key, value in self.settings.items() if key != name})
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.credentials.read_bytes(), self.original)
                self.assertEqual(stat.S_IMODE(self.credentials.stat().st_mode), 0o600)
                self.assertIn(name, result.stderr)

    def test_empty_settings_preserve_existing_credentials(self):
        for name in self.settings:
            with self.subTest(empty=name):
                result = self.start_services(dict(self.settings, **{name: ''}))
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.credentials.read_bytes(), self.original)
                self.assertIn(name, result.stderr)

    def test_complete_settings_write_private_s3_credentials(self):
        result = self.start_services(self.settings)
        self.assertEqual(result.returncode, 79, result.stderr)
        self.assertEqual(json.loads(self.credentials.read_bytes()), {
            'identities': [{
                'name': 'artifactory',
                'credentials': [{'accessKey': 'test-access-key', 'secretKey': 'test-secret-key'}],
                'actions': ['Admin', 'Read', 'List', 'Write', 'Tagging'],
            }],
        })
        self.assertEqual(stat.S_IMODE(self.credentials.stat().st_mode), 0o600)


class ProductionComposeTests(unittest.TestCase):
    def test_control_tmpfs_options_remain_one_mount(self):
        environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith(('ARTIFACTORY_', 'COMPOSE_'))
        }
        environment.update(
            ARTIFACTORY_DOMAIN='console.example.invalid',
            ARTIFACTORY_S3_DOMAIN='objects.example.invalid',
            ARTIFACTORY_DATABASE_PASSWORD='non-secret-config-fixture',
            ARTIFACTORY_MASTER_KEY=base64.b64encode(bytes(32)).decode(),
            ARTIFACTORY_S3_ACCESS_KEY='non-secret-config-fixture',
            ARTIFACTORY_S3_SECRET_KEY='non-secret-config-fixture',
            ARTIFACTORY_BOOTSTRAP_PASSWORD='non-secret-config-fixture',
        )
        result = subprocess.run(
            ['docker', 'compose', '--project-name', 'artifactory-config-fixture',
             '--env-file', os.devnull, '-f', str(ROOT / 'compose.yaml'),
             'config', '--format', 'json'],
            cwd=ROOT, env=environment, capture_output=True, text=True,
            check=True, timeout=30,
        )
        manifest = json.loads(result.stdout)
        self.assertEqual(
            manifest['services']['control']['tmpfs'],
            ['/tmp:size=1g,mode=1777'],
            'the size and mode must belong to the same absolute tmpfs mount',
        )


if __name__ == '__main__':
    unittest.main()
