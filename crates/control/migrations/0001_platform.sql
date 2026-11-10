CREATE TABLE users (
    id uuid PRIMARY KEY,
    username text NOT NULL UNIQUE,
    password_hash text NOT NULL,
    role text NOT NULL CHECK (role IN ('admin','operator','viewer')),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE sessions (
    token_hash text PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id),
    expires_at timestamptz NOT NULL
);
CREATE TABLE api_tokens (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id),
    name text NOT NULL,
    token_hash text NOT NULL UNIQUE,
    project_ids uuid[] NOT NULL,
    scopes text[] NOT NULL,
    expires_at timestamptz NOT NULL,
    revoked boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE node_pools (
    id uuid PRIMARY KEY,
    name text NOT NULL UNIQUE
);
INSERT INTO node_pools VALUES ('00000000-0000-0000-0000-000000000001', 'default');
CREATE TABLE credentials (
    id uuid PRIMARY KEY,
    name text NOT NULL,
    kind text NOT NULL CHECK (kind IN ('ssh','https')),
    ciphertext text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE nodes (
    id uuid PRIMARY KEY,
    name text NOT NULL,
    os text NOT NULL,
    arch text NOT NULL,
    pool_id uuid NOT NULL REFERENCES node_pools(id),
    labels jsonb NOT NULL DEFAULT '{}',
    capabilities jsonb NOT NULL DEFAULT '[]',
    diagnostics jsonb NOT NULL DEFAULT '{}',
    signing_profiles text[] NOT NULL DEFAULT '{}',
    slots integer NOT NULL DEFAULT 1 CHECK (slots BETWEEN 1 AND 32),
    draining boolean NOT NULL DEFAULT false,
    revoked boolean NOT NULL DEFAULT false,
    certificate_hash text NOT NULL UNIQUE,
    certificate_expires_at timestamptz NOT NULL,
    active_session uuid,
    last_seen timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE enrollment_tokens (
    id uuid PRIMARY KEY,
    token_hash text NOT NULL UNIQUE,
    pool_id uuid NOT NULL REFERENCES node_pools(id),
    expires_at timestamptz NOT NULL,
    used_at timestamptz
);
CREATE TABLE projects (
    id uuid PRIMARY KEY,
    name text NOT NULL,
    repository_url text NOT NULL,
    credential_id uuid REFERENCES credentials(id),
    pool_ids uuid[] NOT NULL DEFAULT '{00000000-0000-0000-0000-000000000001}',
    signing_profiles text[] NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE builds (
    id uuid PRIMARY KEY,
    project_id uuid NOT NULL REFERENCES projects(id),
    requested_ref text NOT NULL,
    commit_sha text,
    config_yaml text,
    config_snapshot jsonb,
    parameters jsonb NOT NULL DEFAULT '{}',
    selected_targets text[] NOT NULL DEFAULT '{}',
    status text NOT NULL DEFAULT 'resolving' CHECK (status IN ('resolving','queued','running','succeeded','failed','canceling','canceled','lost')),
    error text,
    release_version text,
    publish_status text NOT NULL DEFAULT 'not_requested' CHECK (publish_status IN ('not_requested','pending','succeeded','failed')),
    publish_error text,
    cancel_requested boolean NOT NULL DEFAULT false,
    requested_by uuid REFERENCES users(id),
    created_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz
);
CREATE TABLE jobs (
    id uuid PRIMARY KEY,
    build_id uuid NOT NULL REFERENCES builds(id),
    target text NOT NULL,
    target_config jsonb NOT NULL,
    status text NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','leased','running','succeeded','failed','canceled','lost')),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE(build_id, target)
);
CREATE TABLE attempts (
    id uuid PRIMARY KEY,
    job_id uuid NOT NULL REFERENCES jobs(id),
    node_id uuid NOT NULL REFERENCES nodes(id),
    lease_token text NOT NULL,
    lease_expires_at timestamptz NOT NULL,
    status text NOT NULL CHECK (status IN ('leased','running','succeeded','failed','canceled','lost')),
    command_id uuid NOT NULL,
    last_sequence bigint NOT NULL DEFAULT 0,
    error text,
    started_at timestamptz,
    finished_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX one_active_attempt ON attempts(job_id) WHERE status IN ('leased','running');
CREATE INDEX scheduling_jobs ON jobs(created_at) WHERE status = 'queued';
CREATE INDEX active_node_attempts ON attempts(node_id, lease_expires_at) WHERE status IN ('leased','running');
CREATE TABLE attempt_events (
    attempt_id uuid NOT NULL REFERENCES attempts(id),
    sequence bigint NOT NULL,
    kind text NOT NULL,
    payload jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(attempt_id, sequence)
);
CREATE TABLE step_runs (
    attempt_id uuid NOT NULL REFERENCES attempts(id),
    step_index integer NOT NULL,
    name text NOT NULL,
    phase text NOT NULL,
    status text NOT NULL,
    exit_code integer,
    started_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    PRIMARY KEY(attempt_id, step_index)
);
CREATE TABLE log_chunks (
    id bigserial PRIMARY KEY,
    attempt_id uuid NOT NULL REFERENCES attempts(id),
    sequence bigint NOT NULL,
    step text NOT NULL,
    object_key text NOT NULL UNIQUE,
    sha256 text NOT NULL,
    size bigint NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE(attempt_id, sequence)
);
CREATE TABLE uploads (
    id uuid PRIMARY KEY,
    attempt_id uuid NOT NULL REFERENCES attempts(id),
    path text NOT NULL,
    object_key text NOT NULL UNIQUE,
    expected_size bigint NOT NULL CHECK(expected_size > 0 AND expected_size <= 21474836480),
    expected_sha256 text NOT NULL,
    expires_at timestamptz NOT NULL,
    UNIQUE(attempt_id, path)
);
CREATE TABLE artifacts (
    id uuid PRIMARY KEY REFERENCES uploads(id),
    attempt_id uuid NOT NULL REFERENCES attempts(id),
    path text NOT NULL,
    object_key text NOT NULL UNIQUE,
    size bigint NOT NULL,
    sha256 text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE(attempt_id, path)
);
CREATE TABLE releases (
    id uuid PRIMARY KEY,
    project_id uuid NOT NULL REFERENCES projects(id),
    build_id uuid NOT NULL REFERENCES builds(id),
    version text NOT NULL,
    idempotency_key text NOT NULL,
    commit_sha text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE(project_id, version),
    UNIQUE(project_id, idempotency_key)
);
CREATE TABLE release_artifacts (
    release_id uuid NOT NULL REFERENCES releases(id),
    artifact_id uuid NOT NULL REFERENCES artifacts(id),
    PRIMARY KEY(release_id, artifact_id)
);
CREATE TABLE publish_attempts (
    id uuid PRIMARY KEY,
    build_id uuid NOT NULL REFERENCES builds(id),
    status text NOT NULL,
    error text,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE audit_events (
    id bigserial PRIMARY KEY,
    actor text NOT NULL,
    action text NOT NULL,
    resource_id text NOT NULL,
    details jsonb NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now()
);
