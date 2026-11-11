import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import recovery_acceptance


class RecoveryBackupGuards(unittest.TestCase):
    def test_private_backup_rejects_symlink_before_copying_external_content(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            external = root / 'external-secret'
            external.write_text('must not enter the backup')
            source = root / 'source'
            source.mkdir()
            (source / 'regular').write_text('ordinary content')
            (source / 'linked-secret').symlink_to(external)
            destination = root / 'backup'
            with self.assertRaisesRegex(RuntimeError, 'regular files'):
                recovery_acceptance.copy_private(source, destination)
            self.assertFalse(destination.exists())
            self.assertEqual(external.read_text(), 'must not enter the backup')

    def test_private_backup_rejects_a_symlink_as_its_root(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            external = root / 'external'
            external.mkdir()
            source = root / 'source'
            source.symlink_to(external, target_is_directory=True)
            with self.assertRaisesRegex(RuntimeError, 'regular files'):
                recovery_acceptance.copy_private(source, root / 'backup')
            self.assertFalse((root / 'backup').exists())

    def test_missing_backup_settings_cannot_fall_back_to_inherited_secrets(self):
        settings = {
            'DATABASE_URL': 'postgres://backup-only',
            'ARTIFACTORY_MASTER_KEY': 'backup-key',
            'ARTIFACTORY_S3_ACCESS_KEY': 'backup-access',
            'ARTIFACTORY_S3_SECRET_KEY': 'backup-secret',
            'ARTIFACTORY_S3_REGION': 'us-east-1',
            'ARTIFACTORY_S3_BUCKET': 'backup-bucket',
            'ARTIFACTORY_S3_ENDPOINT': 'http://127.0.0.1:19000',
            'ARTIFACTORY_BOOTSTRAP_USERNAME': 'backup-admin',
            'ARTIFACTORY_BOOTSTRAP_PASSWORD': 'backup-password',
        }
        with tempfile.TemporaryDirectory() as temporary:
            environment = Path(temporary) / 'env'
            for missing in ('ARTIFACTORY_MASTER_KEY', 'DATABASE_URL'):
                with self.subTest(setting=missing):
                    environment.write_text('\n'.join(key + '=' + value for key, value in settings.items() if key != missing) + '\n')
                    with patch.dict(os.environ, {missing: 'inherited-live-secret'}):
                        with self.assertRaisesRegex(RuntimeError, missing):
                            recovery_acceptance.load_environment(environment)


if __name__ == '__main__':
    unittest.main()
