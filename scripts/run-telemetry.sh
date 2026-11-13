#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
set -a
. .local/env
set +a
export ARTIFACTORY_BUSINESS_DATABASE_URL="$DATABASE_URL"
export DATABASE_URL="${ARTIFACTORY_TELEMETRY_DATABASE_URL:?set the independent telemetry database URL in .local/env}"
export ARTIFACTORY_SERVICE_TLS_CERT="$ARTIFACTORY_PKI_DIR/server.pem"
export ARTIFACTORY_SERVICE_TLS_KEY="$ARTIFACTORY_PKI_DIR/server.key"
exec cargo run --locked --bin artifactory-telemetry-service
