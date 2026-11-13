#!/usr/bin/env python3
"""Keep persistence code inside service stores and shared DB infrastructure."""

import argparse
import pathlib
import re
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]
DRIVER = re.compile(r"\b(?:sqlx|toasty(?:_core|_driver_\w+)?|tokio_postgres|postgres|artifactory_database)\s*::|\b(?:PgPool|PgRow|PgConnection)\b")
SQL = re.compile(
    r"\b(?:select\s+(?:.+?\s+from\b|exists\s*\(|pg_\w+\s*\()"
    r"|insert\s+into\b|update\s+\w+\s+set\b|delete\s+from\b"
    r"|create\s+(?:table|index|extension)\b)",
    re.IGNORECASE | re.DOTALL,
)
RUST_STRING = re.compile(r'r(?P<hashes>\#*)"(?P<raw>.*?)"(?P=hashes)|"(?:\\.|[^"\\])*"', re.DOTALL)


def persistence_path(path):
    parts = path.parts
    return (
        parts[:3] == ("crates", "database", "src")
        or "tests" in parts
        or ("src" in parts and ("store" in parts or path.name == "store.rs"))
    )


def violations(root):
    failures = []
    sources = list((root / "crates").rglob("*.rs")) + list((root / "operator").rglob("*.rs"))
    for path in sorted(sources):
        relative = path.relative_to(root)
        if persistence_path(relative):
            continue
        source = path.read_text()
        for line_number, line in enumerate(source.splitlines(), 1):
            if line.lstrip().startswith("//"):
                continue
            if DRIVER.search(line):
                failures.append(f"{relative}:{line_number}: persistence belongs in a store")
        for literal in RUST_STRING.finditer(source):
            if SQL.search(literal.group()):
                line_number = source.count("\n", 0, literal.start()) + 1
                failures.append(f"{relative}:{line_number}: persistence belongs in a store")
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dependencies", action="store_true", help="also reject SQLx in the dependency graph")
    args = parser.parse_args()
    failures = violations(ROOT)
    if args.dependencies:
        # Resolve every supported target as well as inspecting the lockfile.
        graph = subprocess.check_output(
            ["cargo", "tree", "--locked", "--workspace", "--target", "all",
             "--edges", "normal,build,dev", "--prefix", "none", "--format", "{p}"],
            cwd=ROOT, text=True,
        )
        for package in sorted(set(re.findall(r"(?m)^(sqlx(?:-[\w-]+)?) v", graph))):
            failures.append(f"dependency graph still contains {package}")
        for package in tomllib.loads((ROOT / "Cargo.lock").read_text())["package"]:
            if package["name"] == "sqlx" or package["name"].startswith("sqlx-"):
                failures.append(f"lockfile still contains {package['name']}")
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print("Database access boundaries verified.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
