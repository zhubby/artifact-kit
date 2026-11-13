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
import json, os, tempfile
from pathlib import Path

required = ('ARTIFACTORY_S3_ACCESS_KEY', 'ARTIFACTORY_S3_SECRET_KEY')
missing = [name for name in required if not os.environ.get(name)]
if missing:
    raise SystemExit('Missing required configuration: ' + ', '.join(missing) + '; update .local/env before starting local services.')
payload = json.dumps({'identities': [{'name': 'artifactory', 'credentials': [{'accessKey': os.environ['ARTIFACTORY_S3_ACCESS_KEY'], 'secretKey': os.environ['ARTIFACTORY_S3_SECRET_KEY']}], 'actions': ['Admin', 'Read', 'List', 'Write', 'Tagging']}]})
destination = Path('.local/s3.json')
descriptor, temporary = tempfile.mkstemp(prefix='.s3-', dir=destination.parent)
try:
    with os.fdopen(descriptor, 'w') as output:
        output.write(payload)
    os.replace(temporary, destination)
finally:
    if os.path.exists(temporary):
        os.unlink(temporary)
PY
if ! docker inspect artifactory-timescaledb-test >/dev/null 2>&1; then
  docker run -d --name artifactory-timescaledb-test -p 127.0.0.1:15432:5432 -e POSTGRES_DB=artifactory -e POSTGRES_USER=artifactory -e POSTGRES_PASSWORD=local-test-only -e TIMESCALEDB_TELEMETRY=off -e ARTIFACTORY_TELEMETRY_DATABASE_PASSWORD=local-telemetry-test-only -v artifactory-timescaledb-test:/var/lib/postgresql/data -v "$(pwd)/deploy/timescale-init.sh:/docker-entrypoint-initdb.d/100_artifactory.sh:ro" "${ARTIFACTORY_POSTGRES_IMAGE:-timescale/timescaledb:2.32.0-pg17}" >/dev/null
else
  docker start artifactory-timescaledb-test >/dev/null
fi
if ! docker inspect artifactory-s3-test >/dev/null 2>&1; then
  docker run -d --name artifactory-s3-test -p 127.0.0.1:19000:8333 -v "$(pwd)/.local/s3:/data" -v "$(pwd)/.local/s3.json:/etc/seaweed/s3.json:ro" -v "$(pwd)/deploy/seaweed-entrypoint.sh:/run/artifactory-s3-entrypoint.sh:ro" --entrypoint /bin/sh "${ARTIFACTORY_S3_IMAGE:-chrislusf/seaweedfs:4.53}" /run/artifactory-s3-entrypoint.sh mini -dir=/data -s3.port=8333 -s3.config=/run/artifactory-s3/s3.json -s3.iam.readOnly=true -admin.ui=false -ip.bind=0.0.0.0 >/dev/null
else
  docker start artifactory-s3-test >/dev/null
fi
python3 scripts/wait_local_services.py
printf '%s\n' 'Local TimescaleDB: 127.0.0.1:15432; authenticated S3: 127.0.0.1:19000. Existing PostgreSQL containers/volumes are preserved.'
