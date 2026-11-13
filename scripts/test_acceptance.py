import importlib.util
import json
import os
import signal
import socket
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch


spec = importlib.util.spec_from_file_location(
    "acceptance", Path(__file__).with_name("acceptance.py")
)
acceptance = importlib.util.module_from_spec(spec)
spec.loader.exec_module(acceptance)


class ControlStartupTests(unittest.TestCase):
    def test_exited_control_reports_status_and_log_without_readiness_timeout(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "target/debug/artifactory-control-plane"
            binary.parent.mkdir(parents=True)
            binary.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "print('fixture: control startup failed', file=sys.stderr, flush=True)\n"
                "sys.exit(23)\n"
            )
            binary.chmod(0o700)
            evidence = root / "evidence"
            evidence.mkdir()
            with socket.socket() as listener:
                listener.bind(("127.0.0.1", 0))
                port = listener.getsockname()[1]
            started = time.monotonic()
            with patch.multiple(
                acceptance,
                ROOT=root,
                DIRECTORY=evidence,
                BASE=f"http://127.0.0.1:{port}",
            ):
                with self.assertRaisesRegex(RuntimeError, r"control plane exited.*23") as failure:
                    acceptance.start_control()
            self.assertIn("fixture: control startup failed", str(failure.exception))
            self.assertLess(time.monotonic() - started, 5)

    def test_startup_timeout_terminates_control(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "target/debug/artifactory-control-plane"
            binary.parent.mkdir(parents=True)
            marker = root / "control.pid"
            binary.write_text(
                "#!/usr/bin/env python3\n"
                "import os, pathlib, time\n"
                f"pathlib.Path({str(marker)!r}).write_text(str(os.getpid()))\n"
                "time.sleep(120)\n"
            )
            binary.chmod(0o700)
            evidence = root / "evidence"
            evidence.mkdir()
            with socket.socket() as listener:
                listener.bind(("127.0.0.1", 0))
                port = listener.getsockname()[1]
            wait_for = acceptance.wait_for

            def wait_after_process_started(probe, timeout):
                wait_for(marker.exists, timeout=5)
                return wait_for(probe, timeout)

            try:
                with patch.multiple(acceptance, ROOT=root, DIRECTORY=evidence, BASE=f"http://127.0.0.1:{port}"), patch.object(acceptance, 'wait_for', side_effect=wait_after_process_started):
                    with self.assertRaises(TimeoutError):
                        acceptance.start_control(timeout=0.5)
                self.assertTrue(marker.exists(), "the control process must actually have started")
                with self.assertRaises(ProcessLookupError):
                    os.kill(int(marker.read_text()), 0)
            finally:
                if marker.exists():
                    try:
                        os.kill(int(marker.read_text()), signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    def test_ready_control_remains_running_until_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "target/debug/artifactory-control-plane"
            binary.parent.mkdir(parents=True)
            with socket.socket() as listener:
                listener.bind(("127.0.0.1", 0))
                port = listener.getsockname()[1]
            binary.write_text(
                "#!/usr/bin/env python3\n"
                "import http.server\n"
                "class Handler(http.server.BaseHTTPRequestHandler):\n"
                "    def do_GET(self):\n"
                "        self.send_response(200 if self.path == '/healthz' else 404)\n"
                "        self.end_headers()\n"
                f"http.server.HTTPServer(('127.0.0.1', {port}), Handler).serve_forever()\n"
            )
            binary.chmod(0o700)
            evidence = root / "evidence"
            evidence.mkdir()
            process = None
            try:
                with patch.multiple(acceptance, ROOT=root, DIRECTORY=evidence, BASE=f"http://127.0.0.1:{port}"):
                    process = acceptance.start_control(timeout=5)
                self.assertIsNone(process.poll())
            finally:
                if process is not None:
                    process.terminate()
                    process.wait(timeout=5)


class BuildFailureDiagnosticsTests(unittest.TestCase):
    def test_unexpected_build_records_errors_without_attempt_credentials(self):
        result = {'id': 'fixture-build', 'status': 'failed', 'error': 'fixture source resolution failed', 'jobs': [{'target': 'native', 'attempts': [{'status': 'failed', 'error': 'checkout failed', 'lease_token': 'must-not-appear'}]}]}
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            with patch.multiple(acceptance, DIRECTORY=directory, EVIDENCE={'checks': []}), patch.object(acceptance, 'api', return_value=result):
                with self.assertRaisesRegex(AssertionError, 'fixture source resolution failed') as failure:
                    acceptance.terminal('fixture-build', 'succeeded')
            recorded = (directory / 'evidence.json').read_text()
            self.assertNotIn('must-not-appear', recorded + str(failure.exception))
            self.assertNotIn('lease_token', recorded + str(failure.exception))
            diagnostic = json.loads(recorded)['unexpected_build']
            self.assertEqual(diagnostic['actual'], 'failed')
            self.assertEqual(diagnostic['attempts'][0]['error'], 'checkout failed')


if __name__ == "__main__":
    unittest.main()
