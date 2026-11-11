"""Seed pre-AI build records for the existing protocol/recovery regression suite.

This is test fixture setup, not a product API or an alternate new-build path.
Real-model planning acceptance lives in ai_acceptance.py.
"""
import json
import os
import subprocess
import urllib.parse
import uuid


def historical_build(project, body, database_url=None):
    project = str(uuid.UUID(project))
    build = str(uuid.uuid4())
    parts = urllib.parse.urlsplit(database_url or os.environ['DATABASE_URL'])
    environment = dict(os.environ, PGHOST=parts.hostname, PGPORT=str(parts.port or 5432),
                       PGUSER=urllib.parse.unquote(parts.username or ''), PGPASSWORD=urllib.parse.unquote(parts.password or ''),
                       PGDATABASE=parts.path.lstrip('/'))
    environment.update({'PG' + key.upper(): value for key, value in urllib.parse.parse_qsl(parts.query)})
    def literal(value):
        return 'NULL' if value is None else "'" + value.replace("'", "''") + "'"
    reference = literal(body.get('ref', 'main'))
    parameters = literal(json.dumps(body.get('parameters', {})))
    targets = literal(json.dumps(body.get('targets', [])))
    release = literal(body.get('release_version'))
    publication = literal('pending' if body.get('release_version') else 'not_requested')
    query = f"""INSERT INTO builds(id,project_id,requested_ref,parameters,selected_targets,release_version,publish_status,source_repository_url,source_credential_id)
        SELECT '{build}',id,{reference},{parameters}::jsonb,ARRAY(SELECT jsonb_array_elements_text({targets}::jsonb)),{release},{publication},repository_url,credential_id
        FROM projects WHERE id='{project}' RETURNING id;"""
    result = subprocess.run(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1'], input=query, text=True, env=environment, capture_output=True, timeout=15)
    if result.returncode or result.stdout.strip() != build:
        raise RuntimeError('historical build fixture setup failed')
    return {'id': build}


if __name__ == '__main__':
    import sys
    payload = json.load(sys.stdin)
    print(json.dumps(historical_build(payload['project'], payload['body'])))
