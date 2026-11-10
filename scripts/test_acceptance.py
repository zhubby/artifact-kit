import importlib.util
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
            binary = root / "target/debug/artifact-control"
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
            binary = root / "target/debug/artifact-control"
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
            try:
                with patch.multiple(acceptance, ROOT=root, DIRECTORY=evidence, BASE=f"http://127.0.0.1:{port}"):
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
            binary = root / "target/debug/artifact-control"
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


if __name__ == "__main__":
    unittest.main()
