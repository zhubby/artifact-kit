#!/bin/sh
set -eu
role="${1:-operator}"
shift || true
case "$role" in
  operator) exec artifactory-operator "$@" ;;
  runtime) exec artifactory-planner-runtime "$@" ;;
  source) exec artifactory-source-service "$@" ;;
  control) exec artifactory-control "$@" ;;
  telemetry) exec artifactory-telemetry-service "$@" ;;
  *) echo 'unknown service role' >&2; exit 2 ;;
esac
