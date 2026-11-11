CREATE TABLE model_profiles (
    id uuid PRIMARY KEY,
    name text NOT NULL UNIQUE,
    api text NOT NULL CHECK (api IN ('openai-completions','openai-responses','anthropic-messages')),
    base_url text NOT NULL,
    model text NOT NULL,
    key_ciphertext text NOT NULL,
    parameters jsonb NOT NULL DEFAULT '{}',
    revision integer NOT NULL DEFAULT 1,
    enabled boolean NOT NULL DEFAULT true,
    is_default boolean NOT NULL DEFAULT false,
    max_calls integer NOT NULL DEFAULT 20 CHECK (max_calls BETWEEN 1 AND 100),
    timeout_seconds integer NOT NULL DEFAULT 600 CHECK (timeout_seconds BETWEEN 30 AND 3600),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX one_default_model ON model_profiles(is_default) WHERE is_default;
ALTER TABLE projects ADD COLUMN model_profile_id uuid REFERENCES model_profiles(id) ON DELETE SET NULL;
ALTER TABLE projects ADD COLUMN build_context text NOT NULL DEFAULT '';
ALTER TABLE projects ADD COLUMN source_credentials jsonb NOT NULL DEFAULT '[]';

CREATE TABLE source_repositories (
    id uuid PRIMARY KEY,
    project_id uuid NOT NULL REFERENCES projects(id),
    repository_url text NOT NULL,
    credential_id uuid REFERENCES credentials(id),
    cache_key text NOT NULL UNIQUE,
    last_used_at timestamptz NOT NULL DEFAULT now(),
    fetched_bytes bigint NOT NULL DEFAULT 0,
    cache_hits bigint NOT NULL DEFAULT 0
);
CREATE TABLE source_snapshots (
    id uuid PRIMARY KEY,
    project_id uuid NOT NULL REFERENCES projects(id),
    root_repository_id uuid NOT NULL REFERENCES source_repositories(id),
    commit_sha text NOT NULL,
    manifest jsonb NOT NULL,
    digest text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE(project_id,digest)
);
CREATE TABLE source_resolutions (
    repository_id uuid NOT NULL REFERENCES source_repositories(id),
    reference text NOT NULL,
    commit_sha text NOT NULL,
    fetched_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(repository_id,reference)
);
CREATE TABLE ai_workers (
    id uuid PRIMARY KEY,
    name text NOT NULL,
    certificate_hash text NOT NULL UNIQUE,
    certificate_expires_at timestamptz NOT NULL,
    draining boolean NOT NULL DEFAULT false,
    revoked boolean NOT NULL DEFAULT false,
    last_seen timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE ai_enrollment_tokens (
    id uuid PRIMARY KEY,
    token_hash text NOT NULL UNIQUE,
    expires_at timestamptz NOT NULL,
    used_at timestamptz
);
ALTER TABLE builds DROP CONSTRAINT builds_status_check;
ALTER TABLE builds ADD CONSTRAINT builds_status_check CHECK (status IN
    ('resolving','planning','needs_input','replanning','queued','running','succeeded','failed','canceling','canceled','lost'));
ALTER TABLE builds ADD COLUMN goal text;
ALTER TABLE builds ADD COLUMN target_requirements jsonb NOT NULL DEFAULT '[]';
ALTER TABLE builds ADD COLUMN planning_context jsonb NOT NULL DEFAULT '{}';
ALTER TABLE builds ADD COLUMN model_config_snapshot jsonb;
ALTER TABLE builds ADD COLUMN source_snapshot_id uuid REFERENCES source_snapshots(id);
ALTER TABLE builds ADD COLUMN revision_count integer NOT NULL DEFAULT 0 CHECK (revision_count BETWEEN 0 AND 2);
ALTER TABLE builds ADD COLUMN clarification jsonb;
CREATE TABLE planning_runs (
    id uuid PRIMARY KEY,
    build_id uuid NOT NULL REFERENCES builds(id),
    worker_id uuid REFERENCES ai_workers(id),
    purpose text NOT NULL CHECK (purpose IN ('initial','revision','clarification')),
    state text NOT NULL DEFAULT 'queued' CHECK (state IN ('queued','leased','succeeded','needs_input','failed','canceled','lost')),
    failed_targets text[] NOT NULL DEFAULT '{}',
    model_config jsonb NOT NULL,
    lease_hash text,
    lease_expires_at timestamptz,
    deadline_at timestamptz,
    calls integer NOT NULL DEFAULT 0,
    usage jsonb NOT NULL DEFAULT '{}',
    error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz
);
CREATE UNIQUE INDEX one_pending_planning_run ON planning_runs(build_id) WHERE state IN ('queued','leased');
CREATE TABLE plan_revisions (
    id uuid PRIMARY KEY,
    build_id uuid NOT NULL REFERENCES builds(id),
    run_id uuid NOT NULL UNIQUE REFERENCES planning_runs(id),
    revision integer NOT NULL,
    source_snapshot_id uuid NOT NULL REFERENCES source_snapshots(id),
    contents jsonb NOT NULL,
    sha256 text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE(build_id,revision)
);
ALTER TABLE builds ADD COLUMN current_plan_id uuid REFERENCES plan_revisions(id);
ALTER TABLE jobs ADD COLUMN plan_revision_id uuid REFERENCES plan_revisions(id);
ALTER TABLE attempts ADD COLUMN plan_revision_id uuid REFERENCES plan_revisions(id);
ALTER TABLE attempts ADD COLUMN source_snapshot_id uuid REFERENCES source_snapshots(id);
CREATE TABLE planning_events (
    id bigserial PRIMARY KEY,
    run_id uuid NOT NULL REFERENCES planning_runs(id),
    kind text NOT NULL,
    payload jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE source_tokens (
    token_hash text PRIMARY KEY,
    snapshot_id uuid NOT NULL REFERENCES source_snapshots(id),
    attempt_id uuid NOT NULL REFERENCES attempts(id),
    expires_at timestamptz NOT NULL
);
