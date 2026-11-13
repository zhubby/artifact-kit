#!/usr/bin/env python3
"""Prepare private, persistent Kubernetes Secrets; never overwrite an output directory."""

import argparse
import base64
import ipaddress
import json
import os
from pathlib import Path
import re
import secrets
import subprocess


def dns_label(value):
    if len(value) > 63 or not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]*[a-z0-9])?", value):
        raise argparse.ArgumentTypeError("use a lowercase DNS label, at most 63 characters")
    return value


def san(host):
    try:
        return "IP:" + str(ipaddress.ip_address(host))
    except ValueError:
        if len(host) > 253 or not all(
            re.fullmatch(r"[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?", label)
            for label in host.split(".")
        ):
            raise argparse.ArgumentTypeError("gRPC hosts must be DNS names or IP addresses") from None
        return "DNS:" + host


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--namespace", type=dns_label, default="artifactory")
    parser.add_argument("--release", type=dns_label, default="artifactory")
    parser.add_argument("--resource-prefix", type=dns_label, help="match Helm fullnameOverride, if used")
    parser.add_argument("--grpc-host", type=san, action="append", required=True,
                        help="Emissary-visible host, without port; repeat for multiple SANs")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    prefix = (args.resource_prefix or args.release)[:50].rstrip("-")
    output = args.output.expanduser().absolute()
    os.umask(0o077)
    try:
        output.mkdir(mode=0o700, parents=True, exist_ok=False)
    except FileExistsError:
        parser.exit(1, f"{output} already exists; refusing to rotate persistent credentials or CA.\n")
    pki = output / "pki"
    pki.mkdir(mode=0o700)

    def openssl(*arguments):
        subprocess.run(["openssl", *arguments], cwd=pki, check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)

    openssl("req", "-x509", "-newkey", "rsa:3072", "-nodes", "-keyout", "ca.key",
            "-out", "ca.pem", "-sha256", "-days", "3650", "-subj", "/CN=artifactory Emissary Issuer")
    openssl("req", "-newkey", "rsa:3072", "-nodes", "-keyout", "server.key",
            "-out", "server.csr", "-subj", "/CN=artifactory Control")
    hosts = args.grpc_host + ["DNS:" + prefix + "-grpc",
                             f"DNS:{prefix}-grpc.{args.namespace}.svc",
                             "DNS:" + prefix + "-local-source",
                             f"DNS:{prefix}-local-source.{args.namespace}.svc",
                             "DNS:" + prefix + "-telemetry",
                             f"DNS:{prefix}-telemetry.{args.namespace}.svc"]
    (pki / "server.ext").write_text(
        "subjectAltName=" + ",".join(dict.fromkeys(hosts)) + "\n"
        "extendedKeyUsage=serverAuth\nkeyUsage=critical,digitalSignature,keyEncipherment\n"
    )
    openssl("x509", "-req", "-in", "server.csr", "-CA", "ca.pem", "-CAkey", "ca.key",
            "-CAcreateserial", "-out", "server.pem", "-days", "365", "-sha256",
            "-extfile", "server.ext")

    password = secrets.token_hex(24)
    telemetry_password = secrets.token_hex(24)
    s3_access = "artifactory"
    s3_secret = secrets.token_hex(32)
    credentials = {
        "database-password": password,
        "database-url": f"postgres://artifactory:{password}@{prefix}-postgres:5432/artifactory",
        "telemetry-database-password": telemetry_password,
        "telemetry-database-url": f"postgres://artifactory_telemetry:{telemetry_password}@{prefix}-postgres:5432/artifactory_telemetry",
        "telemetry-token": secrets.token_hex(32),
        "master-key": base64.b64encode(secrets.token_bytes(32)).decode(),
        "s3-access-key": s3_access,
        "s3-secret-key": s3_secret,
        "bootstrap-username": "admin",
        "bootstrap-password": secrets.token_hex(24),
        "s3.json": json.dumps({"identities": [{
            "name": "artifactory",
            "credentials": [{"accessKey": s3_access, "secretKey": s3_secret}],
            "actions": ["Admin", "Read", "List", "Write", "Tagging"],
        }]}),
    }

    def write_secret(suffix, data):
        document = {
            "apiVersion": "v1", "kind": "Secret", "type": "Opaque",
            "metadata": {"name": prefix + "-" + suffix, "namespace": args.namespace,
                         "labels": {"app.kubernetes.io/part-of": "artifactory",
                                    "app.kubernetes.io/instance": args.release}},
            "stringData": data,
        }
        (output / (suffix + ".json")).write_text(json.dumps(document, indent=2) + "\n")

    write_secret("credentials", credentials)
    write_secret("pki", {name: (pki / name).read_text()
                         for name in ("ca.pem", "ca.key", "server.pem", "server.key")})
    (output / "access.json").write_text(json.dumps({
        "namespace": args.namespace, "release": args.release,
        "username": credentials["bootstrap-username"],
        "password": credentials["bootstrap-password"],
        "ca_file": str(pki / "ca.pem"),
    }, indent=2) + "\n")
    print(f"Prepared private Secrets in {output}. Back up this directory; reuse it on upgrades.")
    print("Apply credentials.json and pki.json only after confirming the target namespace is new.")


if __name__ == "__main__":
    main()
