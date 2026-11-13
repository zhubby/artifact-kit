#!/usr/bin/env python3
"""Keep persistence code inside service stores and shared DB infrastructure."""

import argparse
import json
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
DRIVER = re.compile(r"\b(?:sqlx|toasty|tokio_postgres)\s*::|\b(?:PgPool|PgRow|PgConnection)\b")
SQL = re.compile(
    r"\b(?:select\s+(?:.+?\s+from\b|exists\s*\(|pg_\w+\s*\()"
    r"|insert\s+into\b|update\s+\w+\s+set\b|delete\s+from\b"
    r"|create\s+(?:table|index|extension)\b)",
    re.IGNORECASE,
)


def persistence_path(path):
    parts = path.parts
    return (
        parts[:3] == ("crates", "database", "src")
        or "tests" in parts
        or ("src" in parts and ("store" in parts or path.name == "store.rs"))
    )


def violations(root):
    failures = []
    for path in sorted((root / "crates").rglob("*.rs")):
        relative = path.relative_to(root)
        if persistence_path(relative):
            continue
        for line_number, line in enumerate(path.read_text().splitlines(), 1):
            if line.lstrip().startswith("//"):
                continue
            if DRIVER.search(line) or SQL.search(line):
                failures.append(f"{relative}:{line_number}: persistence belongs in a store")
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dependencies", action="store_true", help="also reject SQLx in the dependency graph")
    args = parser.parse_args()
    failures = violations(ROOT)
    if args.dependencies:
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--locked", "--format-version", "1"], cwd=ROOT
        ))
        for package in metadata["packages"]:
            if package["name"] == "sqlx" or package["name"].startswith("sqlx-"):
                failures.append(f"dependency graph still contains {package['name']}")
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print("Database access boundaries verified.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
