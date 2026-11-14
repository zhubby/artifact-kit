import argparse
import json
from pathlib import Path
import re
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[1]
STABLE_VERSION = re.compile(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)")


def validate_version(version):
    if not STABLE_VERSION.fullmatch(version):
        raise ValueError(f"{version!r} is not a stable semantic version (major.minor.patch)")
    return version


def inspect_project(root):
    workspace_path = root / "Cargo.toml"
    workspace = tomllib.loads(workspace_path.read_text())["workspace"]
    entries = [(workspace_path, "workspace.package.version", workspace["package"]["version"])]
    package_names = set()
    for pattern in workspace["members"]:
        members = sorted(root.glob(pattern))
        if not members:
            raise ValueError(f"Cargo.toml: workspace member {pattern!r} does not exist")
        for member in members:
            path = member / "Cargo.toml"
            package = tomllib.loads(path.read_text())["package"]
            inheritance = package.get("version")
            if not isinstance(inheritance, dict) or inheritance.get("workspace") is not True:
                raise ValueError(f"{path.relative_to(root)}: require version.workspace = true")
            package_names.add(package["name"])
    lock_path = root / "Cargo.lock"
    lock = tomllib.loads(lock_path.read_text())
    locked_names = set()
    for package in lock["package"]:
        if package["name"] in package_names and "source" not in package:
            locked_names.add(package["name"])
            entries.append((lock_path, package["name"], package["version"]))
    if locked_names != package_names:
        raise ValueError(f"Cargo.lock: missing workspace packages {sorted(package_names - locked_names)}")
    package_paths = [root / "web/package.json", *sorted((root / "packages").glob("*/package.json"))]
    for path in package_paths:
        package = json.loads(path.read_text())
        entries.append((path, "version", package["version"]))
        path = path.with_name("package-lock.json")
        package_lock = json.loads(path.read_text())
        entries.extend([(path, "version", package_lock["version"]),
                        (path, 'packages[""].version', package_lock["packages"][""]["version"])])
    chart_paths = sorted((root / "charts").glob("*/Chart.yaml"))
    if not chart_paths:
        raise ValueError("charts/: no first-party Helm charts found")
    for path in chart_paths:
        text = path.read_text()
        for field in ["version", "appVersion"]:
            match = re.search(rf"(?m)^{field}:\s*([^\n]+)$", text)
            if not match:
                raise ValueError(f"{path.relative_to(root)}: missing {field}")
            entries.append((path, field, match.group(1).split(" #", 1)[0].strip().strip("\"'")))
    path = root / "crates/emissary/Dockerfile"
    match = re.search(r"(?m)^ARG ARTIFACTORY_BUILD_VERSION=([^\n]+)$", path.read_text())
    if not match:
        raise ValueError("crates/emissary/Dockerfile: missing ARTIFACTORY_BUILD_VERSION")
    entries.append((path, "ARTIFACTORY_BUILD_VERSION", match.group(1)))
    return entries, package_names


def update_workspace(text, version):
    lines = text.splitlines(keepends=True)
    selected = False
    for index, line in enumerate(lines):
        if line.startswith("["):
            selected = line.strip() == "[workspace.package]"
        if selected and re.match(r"^version\s*=", line):
            lines[index] = re.sub(r'(^version\s*=\s*")[^"]+(".*)',
                                  lambda match: f"{match[1]}{version}{match[2]}", line)
            return "".join(lines)
    raise ValueError("Cargo.toml: missing workspace.package.version")


def synchronize(root, version, entries, package_names):
    updates = {}
    for path in dict.fromkeys(entry[0] for entry in entries):
        text = path.read_text()
        if path.name == "Cargo.toml":
            updated = update_workspace(text, version)
        elif path.name == "Cargo.lock":
            def update_package(match):
                block = match.group(0)
                package = tomllib.loads(block)["package"][0]
                if package["name"] in package_names and "source" not in package:
                    return re.sub(r'(?m)^(version = ")[^"]+(".*)$',
                                  lambda value: f"{value[1]}{version}{value[2]}", block, count=1)
                return block
            updated = re.sub(r"(?ms)^\[\[package\]\]\n.*?(?=^\[\[package\]\]|\Z)",
                             update_package, text)
        elif path.suffix == ".json":
            package = json.loads(text)
            changed = package["version"] != version
            package["version"] = version
            if path.name == "package-lock.json":
                changed = changed or package["packages"][""]["version"] != version
                package["packages"][""]["version"] = version
            updated = json.dumps(package, indent=2, ensure_ascii=False) + "\n" if changed else text
        elif path.name == "Dockerfile":
            updated = re.sub(r"(?m)^ARG ARTIFACTORY_BUILD_VERSION=[^\n]+$",
                             f"ARG ARTIFACTORY_BUILD_VERSION={version}", text)
        else:
            updated = re.sub(r"(?m)^(version|appVersion):[^\n]*$",
                             lambda match: f'{match[1]}: "{version}"' if match[1] == "appVersion"
                             else f"version: {version}", text)
        updates[path] = updated
    updates[root / "VERSION"] = version + "\n"
    for path, content in updates.items():
        if path.read_text() != content:
            path.write_text(content)


def main():
    parser = argparse.ArgumentParser(description="Check or synchronize first-party release versions.")
    parser.add_argument("--root", type=Path, default=ROOT)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--set", dest="target_version", metavar="VERSION")
    parser.add_argument("--tag", help="Require an exact v<version> release tag")
    arguments = parser.parse_args()
    root = arguments.root.resolve()
    try:
        canonical = validate_version((root / "VERSION").read_text().strip())
        version = validate_version(arguments.target_version) if arguments.target_version else canonical
        if arguments.tag is not None and arguments.tag != f"v{version}":
            raise ValueError(f"release tag must be v{version}, got {arguments.tag!r}")
        entries, package_names = inspect_project(root)
        if arguments.target_version:
            synchronize(root, version, entries, package_names)
            entries, package_names = inspect_project(root)
        errors = [f"{path.relative_to(root)} ({field}): {value!r}; expected {version!r}"
                  for path, field, value in entries if value != version]
        if errors:
            raise ValueError("\n".join(errors))
    except (OSError, ValueError, KeyError) as error:
        print(f"version error: {error}", file=sys.stderr)
        return 1
    print(f"Version {version} is consistent across Rust, Node.js, lockfiles and Helm charts.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
