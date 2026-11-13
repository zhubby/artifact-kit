#!/usr/bin/env python3
"""Exercise real replicated Control processes using a disposable PostgreSQL database."""
import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--nodes", type=int, default=1000)
    parser.add_argument("--active", type=int, default=100)
    parser.add_argument("--seconds", type=int, default=30)
    parser.add_argument("--database-connections", type=int, default=20,
                        help="database pool budget for each Control replica")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--release", action="store_true", help="use the optimized production build")
    parser.add_argument("--cargo-config", action="append", default=[], help="Cargo configuration override (repeatable)")
    args = parser.parse_args()
    if not 10 <= args.nodes <= 10000 or not 1 <= args.active <= args.nodes:
        parser.error("require 10 <= nodes <= 10000 and 1 <= active <= nodes")
    if not 20 <= args.seconds <= 300:
        parser.error("seconds must be between 20 and 300")
    if not 8 <= args.database_connections <= 256:
        parser.error("database-connections must be between 8 and 256")
    if not os.environ.get("ARTIFACTORY_TEST_DATABASE_URL"):
        parser.error("ARTIFACTORY_TEST_DATABASE_URL must identify an isolated test database")
    root = Path(__file__).resolve().parents[2]
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")
    output = (args.output or root / ".local/control-load" / f"{stamp}-{args.nodes}-{args.active}").resolve()
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    env = os.environ.copy()
    env.update(ARTIFACTORY_LOAD_NODES=str(args.nodes), ARTIFACTORY_LOAD_ACTIVE=str(args.active),
               ARTIFACTORY_LOAD_DB_CONNECTIONS=str(args.database_connections),
               ARTIFACTORY_LOAD_SECONDS=str(args.seconds), ARTIFACTORY_LOAD_OUTPUT=str(output / "evidence.json"),
               ARTIFACTORY_LOAD_CARGO_CONFIGS=json.dumps(args.cargo_config))
    command = ["cargo", *[part for value in args.cargo_config for part in ["--config", value]],
               "test", "--locked", "-p", "artifactory-control-plane", "--test", "database",
               *(["--release"] if args.release else []),
               "transport::load::device_fleet_load", "--", "--ignored", "--exact", "--nocapture"]
    with (output / "run.log").open("w") as log:
        result = subprocess.run(command, cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT)
    if result.returncode or not (output / "evidence.json").exists():
        print(f"Load verification failed; inspect {output / 'run.log'}", file=sys.stderr)
        return 1
    print(json.dumps(json.loads((output / "evidence.json").read_text()), indent=2, ensure_ascii=False))
    print(f"Evidence: {output / 'evidence.json'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
