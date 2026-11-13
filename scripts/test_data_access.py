import pathlib
import tempfile
import unittest

from check_data_access import violations


class DataAccessBoundaries(unittest.TestCase):
    def test_entrypoints_cannot_use_native_models_sql_or_shared_executors(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            service = root / "crates/service/src"
            service.mkdir(parents=True)
            for source in (
                "use toasty::Db;",
                "use toasty_core::driver::Driver;",
                "use artifactory_database::{sql, params};",
                'let query = "SELECT id FROM users";',
                'let query = "INSERT INTO users VALUES(1)";',
                'let query = r#"SELECT\n id\n FROM users"#;',
            ):
                (service / "handlers.rs").write_text(source)
                self.assertTrue(violations(root), source)

    def test_store_infrastructure_and_fixtures_are_the_only_exceptions(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for relative in (
                "crates/service/src/store.rs",
                "crates/service/src/store/auth.rs",
                "crates/database/src/sql.rs",
                "crates/service/tests/database.rs",
            ):
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("use toasty::Db;")
            self.assertEqual(violations(root), [])

    def test_operator_orchestration_obeys_the_same_boundary(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            path = root / "operator/src/reconcile.rs"
            path.parent.mkdir(parents=True)
            path.write_text("use toasty::Db;")
            self.assertTrue(violations(root))
