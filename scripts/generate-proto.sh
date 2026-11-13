#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
cargo build --locked -p artifactory-emissary-protocol -p artifactory-runtime-protocol
