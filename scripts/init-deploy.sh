#!/bin/sh
set -eu
umask 077
cd "$(dirname "$0")/.."
: "${AK_DOMAIN:?set the console HTTPS hostname}"
: "${AK_S3_DOMAIN:?set the object-store HTTPS hostname}"
: "${AK_GRPC_HOST:?set the gRPC hostname, without port}"
case "$AK_DOMAIN:$AK_S3_DOMAIN:$AK_GRPC_HOST" in *[!a-zA-Z0-9.:-]*) printf '%s\n' 'Only DNS hostnames are accepted.' >&2; exit 1;; esac
if [ "$(id -u)" = 0 ]; then printf '%s\n' 'Run as the dedicated non-root deployment account.' >&2; exit 1; fi
if [ -e .deploy/env ]; then printf '%s\n' '.deploy/env exists; refusing to rotate persistent keys.' >&2; exit 1; fi
mkdir -p .deploy/pki .deploy/control
openssl req -x509 -newkey rsa:3072 -nodes -keyout .deploy/pki/ca.key -out .deploy/pki/ca.pem -sha256 -days 3650 -subj '/CN=Artifact Kit Agent Issuer' >/dev/null 2>&1
openssl req -newkey rsa:3072 -nodes -keyout .deploy/pki/server.key -out .deploy/pki/server.csr -subj "/CN=$AK_GRPC_HOST" >/dev/null 2>&1
printf '%s\n' "subjectAltName=DNS:$AK_GRPC_HOST" 'extendedKeyUsage=serverAuth' 'keyUsage=critical,digitalSignature,keyEncipherment' > .deploy/pki/server.ext
openssl x509 -req -in .deploy/pki/server.csr -CA .deploy/pki/ca.pem -CAkey .deploy/pki/ca.key -CAcreateserial -out .deploy/pki/server.pem -days 365 -sha256 -extfile .deploy/pki/server.ext >/dev/null 2>&1
cat > .deploy/env <<EOF
AK_DOMAIN=$AK_DOMAIN
AK_S3_DOMAIN=$AK_S3_DOMAIN
AK_GRPC_HOST=$AK_GRPC_HOST
AK_UID=$(id -u)
AK_GID=$(id -g)
AK_DATABASE_PASSWORD=$(openssl rand -hex 24)
AK_MASTER_KEY=$(openssl rand -base64 32)
AK_S3_ACCESS_KEY=artifact-kit
AK_S3_SECRET_KEY=$(openssl rand -hex 32)
AK_BOOTSTRAP_USERNAME=admin
AK_BOOTSTRAP_PASSWORD=$(openssl rand -hex 24)
EOF
set -a
. .deploy/env
set +a
python3 - <<'PY'
import json, os
with open('.deploy/s3.json', 'w') as output:
    json.dump({'identities': [{'name': 'artifact-kit', 'credentials': [{'accessKey': os.environ['AK_S3_ACCESS_KEY'], 'secretKey': os.environ['AK_S3_SECRET_KEY']}], 'actions': ['Admin', 'Read', 'List', 'Write', 'Tagging']}]}, output)
PY
printf '%s\n' 'Deployment secrets prepared under .deploy/. Back up this directory securely before first use.'
