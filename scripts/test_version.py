import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("version.py")


class ProjectVersionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.write("VERSION", "0.1.0\n")
        self.write("Cargo.toml", '[workspace]\nmembers = ["crates/control-plane"]\n'
                   '[workspace.package]\nversion = "0.1.0"\n')
        self.write("crates/control-plane/Cargo.toml", '[package]\n'
                   'name = "artifactory-control-plane"\nversion.workspace = true\n')
        self.write("Cargo.lock", 'version = 4\n\n[[package]]\n'
                   'name = "artifactory-control-plane"\nversion = "0.1.0"\n\n'
                   '[[package]]\nname = "third-party"\nversion = "0.1.0"\n')
        for directory, name in [("web", "artifactory-console"),
                                ("packages/agent-worker", "artifactory-agent-worker")]:
            self.write_json(f"{directory}/package.json", {
                "name": name, "version": "0.1.0", "dependencies": {"third-party": "0.1.0"},
            })
            self.write_json(f"{directory}/package-lock.json", {
                "name": name, "version": "0.1.0", "lockfileVersion": 3,
                "packages": {"": {"name": name, "version": "0.1.0"},
                             "node_modules/third-party": {"version": "0.1.0"}},
            })
        for chart in ["artifactory", "artifactory-operator"]:
            self.write(f"charts/{chart}/Chart.yaml", f"apiVersion: v2\nname: {chart}\n"
                       'version: 0.1.0\nappVersion: "0.1.0"\n')
        self.write("crates/emissary/Dockerfile", "FROM rust:1.95-bookworm\n"
                   "ARG ARTIFACTORY_BUILD_VERSION=0.1.0\n")

    def write(self, relative, content):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)

    def write_json(self, relative, value):
        self.write(relative, json.dumps(value, indent=2) + "\n")

    def run_version(self, *arguments):
        return subprocess.run([sys.executable, str(SCRIPT), "--root", str(self.root),
                               *arguments], capture_output=True, text=True, timeout=10)

    def test_check_accepts_a_synchronized_project(self):
        result = self.run_version("--check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("0.1.0", result.stdout)

    def test_check_reports_node_package_drift(self):
        self.write_json("web/package.json", {"name": "artifactory-console", "version": "0.9.0"})
        result = self.run_version("--check")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("web/package.json", result.stderr)

    def test_check_reports_npm_lock_root_drift(self):
        path = self.root / "web/package-lock.json"
        lock = json.loads(path.read_text())
        lock["packages"][""]["version"] = "0.9.0"
        self.write_json("web/package-lock.json", lock)
        result = self.run_version("--check")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("web/package-lock.json", result.stderr)

    def test_check_reports_cargo_lock_drift(self):
        path = self.root / "Cargo.lock"
        path.write_text(path.read_text().replace('version = "0.1.0"', 'version = "0.9.0"', 1))
        result = self.run_version("--check")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Cargo.lock", result.stderr)

    def test_check_reports_chart_app_version_drift(self):
        self.write("charts/artifactory/Chart.yaml", "version: 0.1.0\nappVersion: 0.9.0\n")
        result = self.run_version("--check")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("charts/artifactory/Chart.yaml", result.stderr)

    def test_check_reports_container_binary_version_drift(self):
        self.write("crates/emissary/Dockerfile", "ARG ARTIFACTORY_BUILD_VERSION=0.9.0\n")
        result = self.run_version("--check")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("crates/emissary/Dockerfile", result.stderr)

    def test_check_requires_workspace_version_inheritance(self):
        self.write("crates/control-plane/Cargo.toml", '[package]\n'
                   'name = "artifactory-control-plane"\nversion = "0.1.0"\n')
        result = self.run_version("--check")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("version.workspace", result.stderr)

    def test_set_synchronizes_first_party_versions_without_changing_dependencies(self):
        result = self.run_version("--set", "0.2.0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / "VERSION").read_text(), "0.2.0\n")
        self.assertIn("ARG ARTIFACTORY_BUILD_VERSION=0.2.0\n",
                      (self.root / "crates/emissary/Dockerfile").read_text())
        self.assertEqual(self.run_version("--check", "--tag", "v0.2.0").returncode, 0)
        package = json.loads((self.root / "web/package.json").read_text())
        lock = json.loads((self.root / "web/package-lock.json").read_text())
        self.assertEqual(package["dependencies"]["third-party"], "0.1.0")
        self.assertEqual(lock["packages"]["node_modules/third-party"]["version"], "0.1.0")
        self.assertIn('name = "third-party"\nversion = "0.1.0"',
                      (self.root / "Cargo.lock").read_text())
        before = {path: path.read_bytes() for path in self.root.rglob("*") if path.is_file()}
        self.assertEqual(self.run_version("--set", "0.2.0").returncode, 0)
        self.assertEqual(before, {path: path.read_bytes() for path in before})

    def test_release_tag_must_match_the_canonical_version(self):
        result = self.run_version("--check", "--tag", "v0.2.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("v0.1.0", result.stderr)

    def test_invalid_target_is_rejected_without_writes(self):
        before = {path: path.read_bytes() for path in self.root.rglob("*") if path.is_file()}
        result = self.run_version("--set", "0.02.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("stable semantic version", result.stderr)
        self.assertEqual(before, {path: path.read_bytes() for path in before})


if __name__ == "__main__":
    unittest.main()
