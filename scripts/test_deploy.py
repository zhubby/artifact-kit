import base64
import json
import os
from pathlib import Path
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[1]


class ProductionComposeTests(unittest.TestCase):
    def test_control_tmpfs_options_remain_one_mount(self):
        environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith(('AK_', 'COMPOSE_'))
        }
        environment.update(
            AK_DOMAIN='console.example.invalid',
            AK_S3_DOMAIN='objects.example.invalid',
            AK_DATABASE_PASSWORD='non-secret-config-fixture',
            AK_MASTER_KEY=base64.b64encode(bytes(32)).decode(),
            AK_S3_ACCESS_KEY='non-secret-config-fixture',
            AK_S3_SECRET_KEY='non-secret-config-fixture',
            AK_BOOTSTRAP_PASSWORD='non-secret-config-fixture',
        )
        result = subprocess.run(
            ['docker', 'compose', '--project-name', 'artifact-kit-config-fixture',
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
