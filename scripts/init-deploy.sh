#!/bin/sh
set -eu
umask 077
cd "$(dirname "$0")/.."
: "${ARTIFACTORY_DOMAIN:?set the console HTTPS hostname}"
: "${ARTIFACTORY_S3_DOMAIN:?set the object-store HTTPS hostname}"
: "${ARTIFACTORY_GRPC_HOST:?set the gRPC hostname, without port}"
case "$ARTIFACTORY_DOMAIN:$ARTIFACTORY_S3_DOMAIN:$ARTIFACTORY_GRPC_HOST" in *[!a-zA-Z0-9.:-]*) printf '%s\n' 'Only DNS hostnames are accepted.' >&2; exit 1;; esac
if [ "$(id -u)" = 0 ]; then printf '%s\n' 'Run as the dedicated non-root deployment account.' >&2; exit 1; fi
if [ -e .deploy/env ]; then printf '%s\n' '.deploy/env exists; refusing to rotate persistent keys.' >&2; exit 1; fi
mkdir -p .deploy/pki .deploy/control .deploy/ai-worker
openssl req -x509 -newkey rsa:3072 -nodes -keyout .deploy/pki/ca.key -out .deploy/pki/ca.pem -sha256 -days 3650 -subj '/CN=artifactory Agent Issuer' >/dev/null 2>&1
openssl req -newkey rsa:3072 -nodes -keyout .deploy/pki/server.key -out .deploy/pki/server.csr -subj "/CN=$ARTIFACTORY_GRPC_HOST" >/dev/null 2>&1
printf '%s\n' "subjectAltName=DNS:$ARTIFACTORY_GRPC_HOST" 'extendedKeyUsage=serverAuth' 'keyUsage=critical,digitalSignature,keyEncipherment' > .deploy/pki/server.ext
openssl x509 -req -in .deploy/pki/server.csr -CA .deploy/pki/ca.pem -CAkey .deploy/pki/ca.key -CAcreateserial -out .deploy/pki/server.pem -days 365 -sha256 -extfile .deploy/pki/server.ext >/dev/null 2>&1
cat > .deploy/env <<EOF
ARTIFACTORY_DOMAIN=$ARTIFACTORY_DOMAIN
ARTIFACTORY_S3_DOMAIN=$ARTIFACTORY_S3_DOMAIN
ARTIFACTORY_GRPC_HOST=$ARTIFACTORY_GRPC_HOST
ARTIFACTORY_UID=$(id -u)
ARTIFACTORY_GID=$(id -g)
ARTIFACTORY_DATABASE_PASSWORD=$(openssl rand -hex 24)
ARTIFACTORY_MASTER_KEY=$(openssl rand -base64 32)
ARTIFACTORY_S3_ACCESS_KEY=artifactory
ARTIFACTORY_S3_SECRET_KEY=$(openssl rand -hex 32)
ARTIFACTORY_BOOTSTRAP_USERNAME=admin
ARTIFACTORY_BOOTSTRAP_PASSWORD=$(openssl rand -hex 24)
EOF
set -a
. .deploy/env
set +a
python3 - <<'PY'
import json, os
with open('.deploy/s3.json', 'w') as output:
    json.dump({'identities': [{'name': 'artifactory', 'credentials': [{'accessKey': os.environ['ARTIFACTORY_S3_ACCESS_KEY'], 'secretKey': os.environ['ARTIFACTORY_S3_SECRET_KEY']}], 'actions': ['Admin', 'Read', 'List', 'Write', 'Tagging']}]}, output)
PY
printf '%s\n' 'Deployment secrets prepared under .deploy/. Back up this directory securely before first use.'
