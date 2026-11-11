#!/bin/sh
set -eu
cd "$(dirname "$0")/../agent"
mkdir -p bin
for target in linux/amd64 linux/arm64 darwin/amd64 darwin/arm64 windows/amd64; do
  operating_system=${target%/*}
  architecture=${target#*/}
  extension=
  if [ "$operating_system" = windows ]; then extension=.exe; fi
  CGO_ENABLED=0 GOOS="$operating_system" GOARCH="$architecture" go build -trimpath -o "bin/artifactory-agent-$operating_system-$architecture$extension" ./cmd/artifactory-agent
done
