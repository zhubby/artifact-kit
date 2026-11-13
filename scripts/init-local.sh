#!/bin/sh
set -eu
umask 077
cd "$(dirname "$0")/.."
mkdir -p .local/pki .local/control-plane .local/emissary
if [ ! -f .local/pki/ca.key ]; then
  openssl req -x509 -newkey rsa:3072 -nodes -keyout .local/pki/ca.key -out .local/pki/ca.pem -sha256 -days 3650 -subj '/CN=artifactory Development CA' >/dev/null 2>&1
fi
if [ ! -f .local/pki/server.key ]; then
  openssl req -newkey rsa:3072 -nodes -keyout .local/pki/server.key -out .local/pki/server.csr -subj '/CN=localhost' >/dev/null 2>&1
  printf '%s\n' 'subjectAltName=DNS:localhost,DNS:control-plane,IP:127.0.0.1' 'extendedKeyUsage=serverAuth' 'keyUsage=critical,digitalSignature,keyEncipherment' > .local/pki/server.ext
  openssl x509 -req -in .local/pki/server.csr -CA .local/pki/ca.pem -CAkey .local/pki/ca.key -CAcreateserial -out .local/pki/server.pem -days 365 -sha256 -extfile .local/pki/server.ext >/dev/null 2>&1
fi
if [ ! -f .local/env ]; then
  cat > .local/env <<EOF
DATABASE_URL=postgres://artifactory:local-test-only@127.0.0.1:15432/artifactory
ARTIFACTORY_TELEMETRY_DATABASE_URL=postgres://artifactory_telemetry:local-telemetry-test-only@127.0.0.1:15432/artifactory_telemetry
ARTIFACTORY_TELEMETRY_ENDPOINT=https://localhost:8444
ARTIFACTORY_TELEMETRY_URL=https://localhost:8093
ARTIFACTORY_TELEMETRY_TOKEN=$(openssl rand -hex 32)
ARTIFACTORY_HTTP_BIND=127.0.0.1:8080
ARTIFACTORY_GRPC_BIND=127.0.0.1:8443
ARTIFACTORY_PUBLIC_URL=http://127.0.0.1:3000
ARTIFACTORY_INSECURE_DEV=true
ARTIFACTORY_REGISTRATION_ENABLED=${ARTIFACTORY_REGISTRATION_ENABLED:-true}
ARTIFACTORY_MASTER_KEY=$(openssl rand -base64 32)
ARTIFACTORY_PKI_DIR=$(pwd)/.local/pki
ARTIFACTORY_DATA_DIR=$(pwd)/.local/control-plane
ARTIFACTORY_S3_ENDPOINT=http://127.0.0.1:19000
ARTIFACTORY_S3_PUBLIC_ENDPOINT=http://127.0.0.1:19000
ARTIFACTORY_S3_BUCKET=artifactory
ARTIFACTORY_S3_REGION=us-east-1
ARTIFACTORY_S3_ACCESS_KEY=artifactory-local
ARTIFACTORY_S3_SECRET_KEY=$(openssl rand -hex 24)
ARTIFACTORY_BOOTSTRAP_USERNAME=admin
ARTIFACTORY_BOOTSTRAP_PASSWORD=$(openssl rand -hex 16)
EOF
fi
printf '%s\n' 'Development PKI and private environment prepared under .local/.' 'Read .local/env for the one-time bootstrap password; never commit this directory.'
