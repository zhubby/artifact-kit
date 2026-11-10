#!/bin/sh
set -eu
umask 077
cd "$(dirname "$0")/.."
mkdir -p .local/pki .local/control .local/agent
if [ ! -f .local/pki/ca.key ]; then
  openssl req -x509 -newkey rsa:3072 -nodes -keyout .local/pki/ca.key -out .local/pki/ca.pem -sha256 -days 3650 -subj '/CN=Artifact Kit Development CA' >/dev/null 2>&1
fi
if [ ! -f .local/pki/server.key ]; then
  openssl req -newkey rsa:3072 -nodes -keyout .local/pki/server.key -out .local/pki/server.csr -subj '/CN=localhost' >/dev/null 2>&1
  printf '%s\n' 'subjectAltName=DNS:localhost,DNS:control,IP:127.0.0.1' 'extendedKeyUsage=serverAuth' 'keyUsage=critical,digitalSignature,keyEncipherment' > .local/pki/server.ext
  openssl x509 -req -in .local/pki/server.csr -CA .local/pki/ca.pem -CAkey .local/pki/ca.key -CAcreateserial -out .local/pki/server.pem -days 365 -sha256 -extfile .local/pki/server.ext >/dev/null 2>&1
fi
if [ ! -f .local/env ]; then
  cat > .local/env <<EOF
DATABASE_URL=postgres://artifact:local-test-only@127.0.0.1:15432/artifact
AK_HTTP_BIND=127.0.0.1:8080
AK_GRPC_BIND=127.0.0.1:8443
AK_PUBLIC_URL=http://127.0.0.1:3000
AK_INSECURE_DEV=true
AK_MASTER_KEY=$(openssl rand -base64 32)
AK_PKI_DIR=$(pwd)/.local/pki
AK_DATA_DIR=$(pwd)/.local/control
AK_S3_ENDPOINT=http://127.0.0.1:19000
AK_S3_PUBLIC_ENDPOINT=http://127.0.0.1:19000
AK_S3_BUCKET=artifact-kit
AK_S3_REGION=us-east-1
AK_S3_ACCESS_KEY=artifact-local
AK_S3_SECRET_KEY=$(openssl rand -hex 24)
AK_BOOTSTRAP_USERNAME=admin
AK_BOOTSTRAP_PASSWORD=$(openssl rand -hex 16)
EOF
fi
printf '%s\n' 'Development PKI and private environment prepared under .local/.' 'Read .local/env for the one-time bootstrap password; never commit this directory.'
