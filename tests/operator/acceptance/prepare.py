#!/usr/bin/env python3
"""Build test images for the target cluster architecture; never deploy them."""
import argparse
from pathlib import Path
import os
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[3]

def run(args, cwd=ROOT, env=None):
    subprocess.run(args, cwd=cwd, env=env, check=True)

if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--platform', default='linux/arm64', choices=['linux/arm64', 'linux/amd64'])
    parser.add_argument('--registry', default='localhost:5001/artifact-kit')
    parser.add_argument('--tag', default='rust-native')
    parser.add_argument('--rust-image', default='rust:1.95-bookworm')
    parser.add_argument('--base-image', default='debian:bookworm-slim')
    parser.add_argument('--pi-base', help='Optional existing image with the identical pinned Pi dependencies')
    parser.add_argument('--skip-runtime-build', action='store_true')
    parser.add_argument('--kind', help='Load into this kind cluster instead of pushing')
    args = parser.parse_args()
    directory = ROOT / '.local/operator-test'; directory.mkdir(parents=True, exist_ok=True)
    runtime = f'{args.registry}/operator:{args.tag}'
    fixture = f'{args.registry}/operator-fixture:{args.tag}'
    pi = f'{args.registry}/operator-pi:{args.tag}'
    if not args.skip_runtime_build:
        run(['docker', 'build', '--platform', args.platform, '-f', 'operator/Dockerfile', '--build-arg', 'RUST_IMAGE='+args.rust_image, '--build-arg', 'BASE_IMAGE='+args.base_image, '-t', runtime, '.'])
    environment = dict(os.environ, CGO_ENABLED='0', GOOS='linux', GOARCH=args.platform.split('/')[1])
    run(['go', 'build', '-o', str(directory/'fixture'), str(ROOT/'tests/operator/acceptance/fixture.go'), str(ROOT/'tests/operator/acceptance/live_provider.go')], env=environment)
    run(['docker', 'build', '--platform', args.platform, '-f', str(ROOT/'tests/operator/acceptance/Dockerfile'), '--build-arg', 'BASE_IMAGE='+runtime, '-t', fixture, str(directory)])
    if args.pi_base:
        run(['npm', 'run', 'build'], cwd=ROOT/'ai-worker')
        dockerfile = directory/'pi.Dockerfile'
        shutil.copytree(ROOT/'ai-worker/dist', directory/'dist', dirs_exist_ok=True)
        shutil.copytree(ROOT/'proto', directory/'proto', dirs_exist_ok=True)
        dockerfile.write_text('ARG BASE_IMAGE=node:24-bookworm-slim\nFROM ${BASE_IMAGE}\nCOPY dist /app/ai-worker/dist\nCOPY --chown=10001:10001 proto /app/proto\n')
        run(['docker', 'build', '--platform', args.platform, '-f', str(dockerfile), '--build-arg', 'BASE_IMAGE='+args.pi_base, '-t', pi, str(directory)])
    else:
        run(['docker', 'build', '--platform', args.platform, '-f', 'ai-worker/Dockerfile', '-t', pi, '.'])
    dependencies = [('timescale/timescaledb:2.32.0-pg17', f'{args.registry}/timescaledb:2.32.0-pg17'), ('chrislusf/seaweedfs:4.53', f'{args.registry}/seaweedfs:4.53')]
    for upstream, local in dependencies:
        existing = subprocess.run(['docker','image','inspect','--format','{{.Os}}/{{.Architecture}}',local],text=True,capture_output=True)
        if existing.returncode == 0 and existing.stdout.strip() == args.platform:
            continue
        run(['docker', 'pull', '--platform', args.platform, upstream])
        run(['docker', 'tag', upstream, local])
    images = [runtime, fixture, pi, *[local for _, local in dependencies]]
    for image in images:
        actual = subprocess.check_output(['docker','image','inspect','--format','{{.Os}}/{{.Architecture}}',image], text=True).strip()
        if actual != args.platform:
            raise RuntimeError(f'Wrong architecture for {image}: {actual}')
        if args.kind:
            run(['kind', 'load', 'docker-image', '--name', args.kind, image])
        else:
            run(['docker', 'push', image])
