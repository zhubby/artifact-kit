#!/bin/sh
set -eu
umask 077
cd "$(dirname "$0")/.."
worker_node=${ARTIFACTORY_NODE:-node}
worker_config=${ARTIFACTORY_AI_CONFIG:-.local/agent-worker/worker.json}
case ${1:-run} in
  init)
    : "${ARTIFACTORY_AI_ENDPOINT:?set the control plane gRPC endpoint}"
    : "${ARTIFACTORY_AI_CA_FILE:?set the trusted CA path}"
    ARTIFACTORY_AI_CONFIG="$worker_config" python3 - <<'PY'
import json, os
from pathlib import Path
path=Path(os.environ['ARTIFACTORY_AI_CONFIG']).resolve()
path.parent.mkdir(parents=True,exist_ok=True,mode=0o700)
value={'endpoint':os.environ['ARTIFACTORY_AI_ENDPOINT'],
       'ca_file':str(Path(os.environ['ARTIFACTORY_AI_CA_FILE']).resolve()),
       'certificate_file':'worker.pem','key_file':'worker.key',
       'name':os.environ.get('ARTIFACTORY_AI_NAME','artifactory-agent-worker')}
if os.environ.get('ARTIFACTORY_AI_SERVER_NAME'):value['server_name']=os.environ['ARTIFACTORY_AI_SERVER_NAME']
with path.open('x') as output:json.dump(value,output,indent=2)
print('Agent worker configuration created:',path)
PY
    ;;
  enroll) exec "$worker_node" packages/agent-worker/dist/main.js enroll --config "$worker_config" ;;
  run) exec "$worker_node" packages/agent-worker/dist/main.js --config "$worker_config" ;;
  *) printf '%s\n' 'usage: sh scripts/agent-worker.sh init|enroll|run' >&2; exit 2 ;;
esac
