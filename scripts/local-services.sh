#!/bin/sh
set -eu
umask 077
cd "$(dirname "$0")/.."
sh scripts/init-local.sh
set -a
. .local/env
set +a
mkdir -p .local/s3
python3 - <<'PY'
import json, os
with open('.local/s3.json', 'w') as output:
    json.dump({'identities': [{'name': 'artifact-kit', 'credentials': [{'accessKey': os.environ['AK_S3_ACCESS_KEY'], 'secretKey': os.environ['AK_S3_SECRET_KEY']}], 'actions': ['Admin', 'Read', 'List', 'Write', 'Tagging']}]}, output)
PY
if ! docker inspect artifact-kit-postgres-test >/dev/null 2>&1; then
  docker run -d --name artifact-kit-postgres-test -p 127.0.0.1:15432:5432 -e POSTGRES_DB=artifact -e POSTGRES_USER=artifact -e POSTGRES_PASSWORD=local-test-only -v artifact-kit-postgres-test:/var/lib/postgresql/data "${AK_POSTGRES_IMAGE:-postgres:17-alpine}" >/dev/null
else
  docker start artifact-kit-postgres-test >/dev/null
fi
if ! docker inspect artifact-kit-s3-test >/dev/null 2>&1; then
  docker run -d --name artifact-kit-s3-test -p 127.0.0.1:19000:8333 -v "$(pwd)/.local/s3:/data" -v "$(pwd)/.local/s3.json:/etc/seaweed/s3.json:ro" "${AK_S3_IMAGE:-chrislusf/seaweedfs:4.53}" mini -dir=/data -s3.port=8333 -s3.config=/etc/seaweed/s3.json -s3.iam.readOnly=true -admin.ui=false -ip.bind=0.0.0.0 >/dev/null
else
  docker start artifact-kit-s3-test >/dev/null
fi
python3 scripts/wait_local_services.py
printf '%s\n' 'Local PostgreSQL: 127.0.0.1:15432; authenticated S3: 127.0.0.1:19000.'
