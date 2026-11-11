#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
protoc -I proto --go_out=agent/gen --go_opt=module=github.com/zhubby/artifact-kit/agent/gen --go-grpc_out=agent/gen --go-grpc_opt=module=github.com/zhubby/artifact-kit/agent/gen proto/artifactory/agent/v1/agent.proto
