

CREATE FUNCTION pin_build_backend() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN NEW.planner_backend := (SELECT planner_backend FROM projects WHERE id=NEW.project_id); RETURN NEW; END $$;

CREATE FUNCTION planning_runtime_intent() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE profile uuid;
BEGIN
 IF TG_OP='INSERT' THEN
  NEW.authority := (SELECT planner_backend FROM builds WHERE id=NEW.build_id);
  profile := (NEW.model_config->>'id')::uuid;
  PERFORM id FROM model_profiles WHERE id=profile FOR UPDATE;
  IF EXISTS(SELECT 1 FROM planning_runs WHERE model_config->>'id'=profile::text AND state IN ('queued','leased') AND authority<>NEW.authority) THEN
   RAISE EXCEPTION 'drain model profile before transferring planner authority';
  END IF;
 END IF;
 RETURN NEW;
END $$;

CREATE FUNCTION publish_runtime_intent() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
 IF NEW.authority='kubernetes' THEN
  IF TG_OP='INSERT' THEN INSERT INTO runtime_intents(run_id,version,kind) VALUES(NEW.id,1,'create');
  ELSIF NEW.state='canceled' AND OLD.state<>'canceled' THEN INSERT INTO runtime_intents(run_id,version,kind) VALUES(NEW.id,2,'cancel') ON CONFLICT DO NOTHING;
  END IF;
 END IF;
 RETURN NEW;
END $$;

CREATE TABLE ai_enrollment_tokens (
    id uuid NOT NULL,
    token_hash text NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    used_at timestamp with time zone
);

CREATE TABLE ai_workers (
    id uuid NOT NULL,
    name text NOT NULL,
    certificate_hash text NOT NULL,
    certificate_expires_at timestamp with time zone NOT NULL,
    draining boolean DEFAULT false NOT NULL,
    revoked boolean DEFAULT false NOT NULL,
    last_seen timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE api_tokens (
    id uuid NOT NULL,
    user_id uuid NOT NULL,
    name text NOT NULL,
    token_hash text NOT NULL,
    project_ids uuid[] NOT NULL,
    scopes text[] NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    revoked boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE artifacts (
    id uuid NOT NULL,
    attempt_id uuid NOT NULL,
    path text NOT NULL,
    object_key text NOT NULL,
    size bigint NOT NULL,
    sha256 text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE attempt_events (
    attempt_id uuid NOT NULL,
    sequence bigint NOT NULL,
    kind text NOT NULL,
    payload jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE attempts (
    id uuid NOT NULL,
    job_id uuid NOT NULL,
    node_id uuid NOT NULL,
    lease_token text NOT NULL,
    lease_expires_at timestamp with time zone NOT NULL,
    status text NOT NULL,
    command_id uuid NOT NULL,
    last_sequence bigint DEFAULT 0 NOT NULL,
    error text,
    started_at timestamp with time zone,
    finished_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    plan_revision_id uuid,
    source_snapshot_id uuid,
    failure_kind text DEFAULT 'execution'::text NOT NULL,
    CONSTRAINT attempts_failure_kind_check CHECK ((failure_kind = ANY (ARRAY['execution'::text, 'source'::text, 'authorization'::text, 'artifact_transport'::text, 'emissary'::text]))),
    CONSTRAINT attempts_status_check CHECK ((status = ANY (ARRAY['leased'::text, 'running'::text, 'succeeded'::text, 'failed'::text, 'canceled'::text, 'lost'::text])))
);

CREATE TABLE audit_events (
    id bigint NOT NULL,
    actor text NOT NULL,
    action text NOT NULL,
    resource_id text NOT NULL,
    details jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE SEQUENCE audit_events_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE audit_events_id_seq OWNED BY audit_events.id;

CREATE TABLE builds (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    requested_ref text NOT NULL,
    commit_sha text,
    config_yaml text,
    config_snapshot jsonb,
    parameters jsonb DEFAULT '{}'::jsonb NOT NULL,
    selected_targets text[] DEFAULT '{}'::text[] NOT NULL,
    status text DEFAULT 'resolving'::text NOT NULL,
    error text,
    release_version text,
    publish_status text DEFAULT 'not_requested'::text NOT NULL,
    publish_error text,
    cancel_requested boolean DEFAULT false NOT NULL,
    requested_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    finished_at timestamp with time zone,
    source_repository_url text NOT NULL,
    source_credential_id uuid,
    goal text,
    target_requirements jsonb DEFAULT '[]'::jsonb NOT NULL,
    planning_context jsonb DEFAULT '{}'::jsonb NOT NULL,
    model_config_snapshot jsonb,
    source_snapshot_id uuid,
    revision_count integer DEFAULT 0 NOT NULL,
    clarification jsonb,
    current_plan_id uuid,
    planner_backend text DEFAULT 'poll'::text NOT NULL,
    CONSTRAINT builds_planner_backend_check CHECK ((planner_backend = ANY (ARRAY['poll'::text, 'kubernetes'::text]))),
    CONSTRAINT builds_publish_status_check CHECK ((publish_status = ANY (ARRAY['not_requested'::text, 'pending'::text, 'succeeded'::text, 'failed'::text]))),
    CONSTRAINT builds_revision_count_check CHECK (((revision_count >= 0) AND (revision_count <= 2))),
    CONSTRAINT builds_status_check CHECK ((status = ANY (ARRAY['resolving'::text, 'planning'::text, 'needs_input'::text, 'replanning'::text, 'queued'::text, 'running'::text, 'succeeded'::text, 'failed'::text, 'canceling'::text, 'canceled'::text, 'lost'::text])))
);

CREATE TABLE credentials (
    id uuid NOT NULL,
    name text NOT NULL,
    kind text NOT NULL,
    ciphertext text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT credentials_kind_check CHECK ((kind = ANY (ARRAY['ssh'::text, 'https'::text])))
);

CREATE TABLE emissary_releases (
    id uuid NOT NULL,
    version text NOT NULL,
    manifest text NOT NULL,
    signature text NOT NULL,
    manifest_hash text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE emissary_updates (
    id uuid NOT NULL,
    generation bigint NOT NULL,
    node_id uuid NOT NULL,
    pool_id uuid NOT NULL,
    release_id uuid NOT NULL,
    previous_version text,
    status text DEFAULT 'queued'::text NOT NULL,
    cancel_requested boolean DEFAULT false NOT NULL,
    error text,
    retry_of uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    finished_at timestamp with time zone,
    last_command_at timestamp with time zone,
    CONSTRAINT emissary_updates_status_check CHECK ((status = ANY (ARRAY['queued'::text, 'offered'::text, 'downloading'::text, 'draining'::text, 'activating'::text, 'succeeded'::text, 'failed'::text, 'rolled_back'::text, 'canceled'::text])))
);

CREATE SEQUENCE emissary_updates_generation_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE emissary_updates_generation_seq OWNED BY emissary_updates.generation;

CREATE TABLE enrollment_tokens (
    id uuid NOT NULL,
    token_hash text NOT NULL,
    pool_id uuid NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    used_at timestamp with time zone
);

CREATE TABLE jobs (
    id uuid NOT NULL,
    build_id uuid NOT NULL,
    target text NOT NULL,
    target_config jsonb NOT NULL,
    status text DEFAULT 'queued'::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    plan_revision_id uuid,
    CONSTRAINT jobs_status_check CHECK ((status = ANY (ARRAY['queued'::text, 'leased'::text, 'running'::text, 'succeeded'::text, 'failed'::text, 'canceled'::text, 'lost'::text])))
);

CREATE TABLE log_chunks (
    id bigint NOT NULL,
    attempt_id uuid NOT NULL,
    sequence bigint NOT NULL,
    step text NOT NULL,
    object_key text NOT NULL,
    sha256 text NOT NULL,
    size bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE SEQUENCE log_chunks_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE log_chunks_id_seq OWNED BY log_chunks.id;

CREATE TABLE model_profiles (
    id uuid NOT NULL,
    name text NOT NULL,
    api text NOT NULL,
    base_url text NOT NULL,
    model text NOT NULL,
    key_ciphertext text NOT NULL,
    parameters jsonb DEFAULT '{}'::jsonb NOT NULL,
    revision integer DEFAULT 1 NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    is_default boolean DEFAULT false NOT NULL,
    max_calls integer DEFAULT 20 NOT NULL,
    timeout_seconds integer DEFAULT 600 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    next_call_at timestamp with time zone,
    CONSTRAINT model_profiles_api_check CHECK ((api = ANY (ARRAY['openai-completions'::text, 'openai-responses'::text, 'anthropic-messages'::text]))),
    CONSTRAINT model_profiles_max_calls_check CHECK (((max_calls >= 1) AND (max_calls <= 100))),
    CONSTRAINT model_profiles_timeout_seconds_check CHECK (((timeout_seconds >= 30) AND (timeout_seconds <= 3600)))
);

CREATE TABLE node_pools (
    id uuid NOT NULL,
    name text NOT NULL,
    emissary_target_release uuid,
    emissary_update_concurrency integer DEFAULT 1 NOT NULL,
    emissary_updates_paused boolean DEFAULT false NOT NULL,
    CONSTRAINT node_pools_emissary_update_concurrency_check CHECK (((emissary_update_concurrency >= 1) AND (emissary_update_concurrency <= 32)))
);

CREATE TABLE nodes (
    id uuid NOT NULL,
    name text NOT NULL,
    os text NOT NULL,
    arch text NOT NULL,
    pool_id uuid NOT NULL,
    labels jsonb DEFAULT '{}'::jsonb NOT NULL,
    capabilities jsonb DEFAULT '[]'::jsonb NOT NULL,
    diagnostics jsonb DEFAULT '{}'::jsonb NOT NULL,
    signing_profiles text[] DEFAULT '{}'::text[] NOT NULL,
    slots integer DEFAULT 1 NOT NULL,
    draining boolean DEFAULT false NOT NULL,
    revoked boolean DEFAULT false NOT NULL,
    certificate_hash text NOT NULL,
    certificate_expires_at timestamp with time zone NOT NULL,
    active_session uuid,
    last_seen timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    emissary_target_release uuid,
    emissary_current_version text,
    emissary_launcher_version text,
    emissary_ota_status jsonb DEFAULT '{}'::jsonb NOT NULL,
    ota_draining boolean DEFAULT false NOT NULL,
    CONSTRAINT nodes_slots_check CHECK (((slots >= 1) AND (slots <= 32)))
);

CREATE TABLE plan_revisions (
    id uuid NOT NULL,
    build_id uuid NOT NULL,
    run_id uuid NOT NULL,
    revision integer NOT NULL,
    source_snapshot_id uuid NOT NULL,
    contents jsonb NOT NULL,
    sha256 text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE planning_events (
    id bigint NOT NULL,
    run_id uuid NOT NULL,
    kind text NOT NULL,
    payload jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE SEQUENCE planning_events_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE planning_events_id_seq OWNED BY planning_events.id;

CREATE TABLE planning_runs (
    id uuid NOT NULL,
    build_id uuid NOT NULL,
    worker_id uuid,
    purpose text NOT NULL,
    state text DEFAULT 'queued'::text NOT NULL,
    failed_targets text[] DEFAULT '{}'::text[] NOT NULL,
    model_config jsonb NOT NULL,
    lease_hash text,
    lease_expires_at timestamp with time zone,
    deadline_at timestamp with time zone,
    calls integer DEFAULT 0 NOT NULL,
    usage jsonb DEFAULT '{}'::jsonb NOT NULL,
    error text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    finished_at timestamp with time zone,
    authority text DEFAULT 'poll'::text NOT NULL,
    CONSTRAINT planning_runs_authority_check CHECK ((authority = ANY (ARRAY['poll'::text, 'kubernetes'::text]))),
    CONSTRAINT planning_runs_purpose_check CHECK ((purpose = ANY (ARRAY['initial'::text, 'revision'::text, 'clarification'::text]))),
    CONSTRAINT planning_runs_state_check CHECK ((state = ANY (ARRAY['queued'::text, 'leased'::text, 'succeeded'::text, 'needs_input'::text, 'failed'::text, 'canceled'::text, 'lost'::text])))
);

CREATE TABLE projects (
    id uuid NOT NULL,
    name text NOT NULL,
    repository_url text NOT NULL,
    credential_id uuid,
    pool_ids uuid[] DEFAULT '{00000000-0000-0000-0000-000000000001}'::uuid[] NOT NULL,
    signing_profiles text[] DEFAULT '{}'::text[] NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    model_profile_id uuid,
    build_context text DEFAULT ''::text NOT NULL,
    source_credentials jsonb DEFAULT '[]'::jsonb NOT NULL,
    planner_backend text DEFAULT 'poll'::text NOT NULL,
    CONSTRAINT projects_planner_backend_check CHECK ((planner_backend = ANY (ARRAY['poll'::text, 'kubernetes'::text])))
);

CREATE TABLE publish_attempts (
    id uuid NOT NULL,
    build_id uuid NOT NULL,
    status text NOT NULL,
    error text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE release_artifacts (
    release_id uuid NOT NULL,
    artifact_id uuid NOT NULL
);

CREATE TABLE releases (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    build_id uuid NOT NULL,
    version text NOT NULL,
    idempotency_key text NOT NULL,
    commit_sha text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE remote_source_requests (
    build_id uuid NOT NULL,
    origin text NOT NULL,
    payload_ciphertext text,
    lease_until timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    error text
);

CREATE TABLE runtime_intents (
    id bigint NOT NULL,
    run_id uuid NOT NULL,
    version bigint NOT NULL,
    kind text NOT NULL,
    payload_ciphertext text,
    delivered_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE SEQUENCE runtime_intents_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE runtime_intents_id_seq OWNED BY runtime_intents.id;

CREATE TABLE runtime_receipts (
    event_id uuid NOT NULL,
    input_hash text NOT NULL,
    result jsonb NOT NULL,
    run_id uuid NOT NULL,
    generation bigint,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE sessions (
    token_hash text NOT NULL,
    user_id uuid NOT NULL,
    expires_at timestamp with time zone NOT NULL
);

CREATE TABLE source_project_routes (
    project_id uuid NOT NULL,
    origin text NOT NULL
);

CREATE TABLE source_repositories (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    repository_url text NOT NULL,
    credential_id uuid,
    cache_key text NOT NULL,
    last_used_at timestamp with time zone DEFAULT now() NOT NULL,
    fetched_bytes bigint DEFAULT 0 NOT NULL,
    cache_hits bigint DEFAULT 0 NOT NULL,
    cache_bytes bigint DEFAULT 0 NOT NULL,
    git_download_bytes bigint DEFAULT 0 NOT NULL,
    lfs_download_bytes bigint DEFAULT 0 NOT NULL,
    emissary_cache_hits bigint DEFAULT 0 NOT NULL,
    emissary_cache_misses bigint DEFAULT 0 NOT NULL
);

CREATE TABLE source_resolutions (
    repository_id uuid NOT NULL,
    reference text NOT NULL,
    commit_sha text NOT NULL,
    fetched_at timestamp with time zone DEFAULT clock_timestamp() NOT NULL
);

CREATE TABLE source_snapshots (
    id uuid NOT NULL,
    project_id uuid NOT NULL,
    root_repository_id uuid NOT NULL,
    commit_sha text NOT NULL,
    manifest jsonb NOT NULL,
    digest text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    last_used_at timestamp with time zone DEFAULT now() NOT NULL,
    service_origin text,
    public_origin text
);

CREATE TABLE source_tokens (
    token_hash text NOT NULL,
    snapshot_id uuid NOT NULL,
    attempt_id uuid NOT NULL,
    expires_at timestamp with time zone NOT NULL
);

CREATE TABLE step_runs (
    attempt_id uuid NOT NULL,
    step_index integer NOT NULL,
    name text NOT NULL,
    phase text NOT NULL,
    status text NOT NULL,
    exit_code integer,
    started_at timestamp with time zone DEFAULT now() NOT NULL,
    finished_at timestamp with time zone
);

CREATE TABLE uploads (
    id uuid NOT NULL,
    attempt_id uuid NOT NULL,
    path text NOT NULL,
    object_key text NOT NULL,
    expected_size bigint NOT NULL,
    expected_sha256 text NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    CONSTRAINT uploads_expected_size_check CHECK (((expected_size > 0) AND (expected_size <= '21474836480'::bigint)))
);

CREATE TABLE users (
    id uuid NOT NULL,
    username text NOT NULL,
    password_hash text NOT NULL,
    role text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT users_role_check CHECK ((role = ANY (ARRAY['admin'::text, 'operator'::text, 'viewer'::text])))
);

ALTER TABLE ONLY audit_events ALTER COLUMN id SET DEFAULT nextval('audit_events_id_seq'::regclass);

ALTER TABLE ONLY emissary_updates ALTER COLUMN generation SET DEFAULT nextval('emissary_updates_generation_seq'::regclass);

ALTER TABLE ONLY log_chunks ALTER COLUMN id SET DEFAULT nextval('log_chunks_id_seq'::regclass);

ALTER TABLE ONLY planning_events ALTER COLUMN id SET DEFAULT nextval('planning_events_id_seq'::regclass);

ALTER TABLE ONLY runtime_intents ALTER COLUMN id SET DEFAULT nextval('runtime_intents_id_seq'::regclass);

ALTER TABLE ONLY ai_enrollment_tokens
    ADD CONSTRAINT ai_enrollment_tokens_pkey PRIMARY KEY (id);

ALTER TABLE ONLY ai_enrollment_tokens
    ADD CONSTRAINT ai_enrollment_tokens_token_hash_key UNIQUE (token_hash);

ALTER TABLE ONLY ai_workers
    ADD CONSTRAINT ai_workers_certificate_hash_key UNIQUE (certificate_hash);

ALTER TABLE ONLY ai_workers
    ADD CONSTRAINT ai_workers_pkey PRIMARY KEY (id);

ALTER TABLE ONLY api_tokens
    ADD CONSTRAINT api_tokens_pkey PRIMARY KEY (id);

ALTER TABLE ONLY api_tokens
    ADD CONSTRAINT api_tokens_token_hash_key UNIQUE (token_hash);

ALTER TABLE ONLY artifacts
    ADD CONSTRAINT artifacts_attempt_id_path_key UNIQUE (attempt_id, path);

ALTER TABLE ONLY artifacts
    ADD CONSTRAINT artifacts_object_key_key UNIQUE (object_key);

ALTER TABLE ONLY artifacts
    ADD CONSTRAINT artifacts_pkey PRIMARY KEY (id);

ALTER TABLE ONLY attempt_events
    ADD CONSTRAINT attempt_events_pkey PRIMARY KEY (attempt_id, sequence);

ALTER TABLE ONLY attempts
    ADD CONSTRAINT attempts_pkey PRIMARY KEY (id);

ALTER TABLE ONLY audit_events
    ADD CONSTRAINT audit_events_pkey PRIMARY KEY (id);

ALTER TABLE ONLY builds
    ADD CONSTRAINT builds_pkey PRIMARY KEY (id);

ALTER TABLE ONLY credentials
    ADD CONSTRAINT credentials_pkey PRIMARY KEY (id);

ALTER TABLE ONLY emissary_releases
    ADD CONSTRAINT emissary_releases_manifest_hash_key UNIQUE (manifest_hash);

ALTER TABLE ONLY emissary_releases
    ADD CONSTRAINT emissary_releases_pkey PRIMARY KEY (id);

ALTER TABLE ONLY emissary_releases
    ADD CONSTRAINT emissary_releases_version_key UNIQUE (version);

ALTER TABLE ONLY emissary_updates
    ADD CONSTRAINT emissary_updates_generation_key UNIQUE (generation);

ALTER TABLE ONLY emissary_updates
    ADD CONSTRAINT emissary_updates_pkey PRIMARY KEY (id);

ALTER TABLE ONLY enrollment_tokens
    ADD CONSTRAINT enrollment_tokens_pkey PRIMARY KEY (id);

ALTER TABLE ONLY enrollment_tokens
    ADD CONSTRAINT enrollment_tokens_token_hash_key UNIQUE (token_hash);

ALTER TABLE ONLY jobs
    ADD CONSTRAINT jobs_build_id_target_key UNIQUE (build_id, target);

ALTER TABLE ONLY jobs
    ADD CONSTRAINT jobs_pkey PRIMARY KEY (id);

ALTER TABLE ONLY log_chunks
    ADD CONSTRAINT log_chunks_attempt_id_sequence_key UNIQUE (attempt_id, sequence);

ALTER TABLE ONLY log_chunks
    ADD CONSTRAINT log_chunks_object_key_key UNIQUE (object_key);

ALTER TABLE ONLY log_chunks
    ADD CONSTRAINT log_chunks_pkey PRIMARY KEY (id);

ALTER TABLE ONLY model_profiles
    ADD CONSTRAINT model_profiles_name_key UNIQUE (name);

ALTER TABLE ONLY model_profiles
    ADD CONSTRAINT model_profiles_pkey PRIMARY KEY (id);

ALTER TABLE ONLY node_pools
    ADD CONSTRAINT node_pools_name_key UNIQUE (name);

ALTER TABLE ONLY node_pools
    ADD CONSTRAINT node_pools_pkey PRIMARY KEY (id);

ALTER TABLE ONLY nodes
    ADD CONSTRAINT nodes_certificate_hash_key UNIQUE (certificate_hash);

ALTER TABLE ONLY nodes
    ADD CONSTRAINT nodes_pkey PRIMARY KEY (id);

ALTER TABLE ONLY plan_revisions
    ADD CONSTRAINT plan_revisions_build_id_revision_key UNIQUE (build_id, revision);

ALTER TABLE ONLY plan_revisions
    ADD CONSTRAINT plan_revisions_pkey PRIMARY KEY (id);

ALTER TABLE ONLY plan_revisions
    ADD CONSTRAINT plan_revisions_run_id_key UNIQUE (run_id);

ALTER TABLE ONLY planning_events
    ADD CONSTRAINT planning_events_pkey PRIMARY KEY (id);

ALTER TABLE ONLY planning_runs
    ADD CONSTRAINT planning_runs_pkey PRIMARY KEY (id);

ALTER TABLE ONLY projects
    ADD CONSTRAINT projects_pkey PRIMARY KEY (id);

ALTER TABLE ONLY publish_attempts
    ADD CONSTRAINT publish_attempts_pkey PRIMARY KEY (id);

ALTER TABLE ONLY release_artifacts
    ADD CONSTRAINT release_artifacts_pkey PRIMARY KEY (release_id, artifact_id);

ALTER TABLE ONLY releases
    ADD CONSTRAINT releases_pkey PRIMARY KEY (id);

ALTER TABLE ONLY releases
    ADD CONSTRAINT releases_project_id_idempotency_key_key UNIQUE (project_id, idempotency_key);

ALTER TABLE ONLY releases
    ADD CONSTRAINT releases_project_id_version_key UNIQUE (project_id, version);

ALTER TABLE ONLY remote_source_requests
    ADD CONSTRAINT remote_source_requests_pkey PRIMARY KEY (build_id);

ALTER TABLE ONLY runtime_intents
    ADD CONSTRAINT runtime_intents_pkey PRIMARY KEY (id);

ALTER TABLE ONLY runtime_intents
    ADD CONSTRAINT runtime_intents_run_id_version_key UNIQUE (run_id, version);

ALTER TABLE ONLY runtime_receipts
    ADD CONSTRAINT runtime_receipts_pkey PRIMARY KEY (event_id);

ALTER TABLE ONLY sessions
    ADD CONSTRAINT sessions_pkey PRIMARY KEY (token_hash);

ALTER TABLE ONLY source_project_routes
    ADD CONSTRAINT source_project_routes_pkey PRIMARY KEY (project_id);

ALTER TABLE ONLY source_repositories
    ADD CONSTRAINT source_repositories_cache_key_key UNIQUE (cache_key);

ALTER TABLE ONLY source_repositories
    ADD CONSTRAINT source_repositories_pkey PRIMARY KEY (id);

ALTER TABLE ONLY source_resolutions
    ADD CONSTRAINT source_resolutions_pkey PRIMARY KEY (repository_id, reference);

ALTER TABLE ONLY source_snapshots
    ADD CONSTRAINT source_snapshots_pkey PRIMARY KEY (id);

ALTER TABLE ONLY source_snapshots
    ADD CONSTRAINT source_snapshots_project_id_digest_key UNIQUE (project_id, digest);

ALTER TABLE ONLY source_tokens
    ADD CONSTRAINT source_tokens_pkey PRIMARY KEY (token_hash);

ALTER TABLE ONLY step_runs
    ADD CONSTRAINT step_runs_pkey PRIMARY KEY (attempt_id, step_index);

ALTER TABLE ONLY uploads
    ADD CONSTRAINT uploads_attempt_id_path_key UNIQUE (attempt_id, path);

ALTER TABLE ONLY uploads
    ADD CONSTRAINT uploads_object_key_key UNIQUE (object_key);

ALTER TABLE ONLY uploads
    ADD CONSTRAINT uploads_pkey PRIMARY KEY (id);

ALTER TABLE ONLY users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);

ALTER TABLE ONLY users
    ADD CONSTRAINT users_username_key UNIQUE (username);

CREATE INDEX active_node_attempts ON attempts USING btree (node_id, lease_expires_at) WHERE (status = ANY (ARRAY['leased'::text, 'running'::text]));

CREATE UNIQUE INDEX emissary_updates_one_active_node ON emissary_updates USING btree (node_id) WHERE (finished_at IS NULL);

CREATE INDEX emissary_updates_pool_active ON emissary_updates USING btree (pool_id) WHERE (finished_at IS NULL);

CREATE UNIQUE INDEX one_active_attempt ON attempts USING btree (job_id) WHERE (status = ANY (ARRAY['leased'::text, 'running'::text]));

CREATE UNIQUE INDEX one_default_model ON model_profiles USING btree (is_default) WHERE is_default;

CREATE UNIQUE INDEX one_pending_planning_run ON planning_runs USING btree (build_id) WHERE (state = ANY (ARRAY['queued'::text, 'leased'::text]));

CREATE INDEX registration_attempts ON audit_events USING btree (created_at) WHERE (action = 'registration_attempt'::text);

CREATE INDEX scheduling_jobs ON jobs USING btree (created_at) WHERE (status = 'queued'::text);

CREATE TRIGGER pin_build_backend BEFORE INSERT ON builds FOR EACH ROW EXECUTE FUNCTION pin_build_backend();

CREATE TRIGGER planning_authority BEFORE INSERT ON planning_runs FOR EACH ROW EXECUTE FUNCTION planning_runtime_intent();

CREATE TRIGGER runtime_outbox AFTER INSERT OR UPDATE OF state ON planning_runs FOR EACH ROW EXECUTE FUNCTION publish_runtime_intent();

ALTER TABLE ONLY api_tokens
    ADD CONSTRAINT api_tokens_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id);

ALTER TABLE ONLY artifacts
    ADD CONSTRAINT artifacts_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES attempts(id);

ALTER TABLE ONLY artifacts
    ADD CONSTRAINT artifacts_id_fkey FOREIGN KEY (id) REFERENCES uploads(id);

ALTER TABLE ONLY attempt_events
    ADD CONSTRAINT attempt_events_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES attempts(id);

ALTER TABLE ONLY attempts
    ADD CONSTRAINT attempts_job_id_fkey FOREIGN KEY (job_id) REFERENCES jobs(id);

ALTER TABLE ONLY attempts
    ADD CONSTRAINT attempts_node_id_fkey FOREIGN KEY (node_id) REFERENCES nodes(id);

ALTER TABLE ONLY attempts
    ADD CONSTRAINT attempts_plan_revision_id_fkey FOREIGN KEY (plan_revision_id) REFERENCES plan_revisions(id);

ALTER TABLE ONLY attempts
    ADD CONSTRAINT attempts_source_snapshot_id_fkey FOREIGN KEY (source_snapshot_id) REFERENCES source_snapshots(id);

ALTER TABLE ONLY builds
    ADD CONSTRAINT builds_current_plan_id_fkey FOREIGN KEY (current_plan_id) REFERENCES plan_revisions(id);

ALTER TABLE ONLY builds
    ADD CONSTRAINT builds_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects(id);

ALTER TABLE ONLY builds
    ADD CONSTRAINT builds_requested_by_fkey FOREIGN KEY (requested_by) REFERENCES users(id);

ALTER TABLE ONLY builds
    ADD CONSTRAINT builds_source_credential_id_fkey FOREIGN KEY (source_credential_id) REFERENCES credentials(id);

ALTER TABLE ONLY builds
    ADD CONSTRAINT builds_source_snapshot_id_fkey FOREIGN KEY (source_snapshot_id) REFERENCES source_snapshots(id);

ALTER TABLE ONLY emissary_updates
    ADD CONSTRAINT emissary_updates_node_id_fkey FOREIGN KEY (node_id) REFERENCES nodes(id);

ALTER TABLE ONLY emissary_updates
    ADD CONSTRAINT emissary_updates_pool_id_fkey FOREIGN KEY (pool_id) REFERENCES node_pools(id);

ALTER TABLE ONLY emissary_updates
    ADD CONSTRAINT emissary_updates_release_id_fkey FOREIGN KEY (release_id) REFERENCES emissary_releases(id);

ALTER TABLE ONLY emissary_updates
    ADD CONSTRAINT emissary_updates_retry_of_fkey FOREIGN KEY (retry_of) REFERENCES emissary_updates(id);

ALTER TABLE ONLY enrollment_tokens
    ADD CONSTRAINT enrollment_tokens_pool_id_fkey FOREIGN KEY (pool_id) REFERENCES node_pools(id);

ALTER TABLE ONLY jobs
    ADD CONSTRAINT jobs_build_id_fkey FOREIGN KEY (build_id) REFERENCES builds(id);

ALTER TABLE ONLY jobs
    ADD CONSTRAINT jobs_plan_revision_id_fkey FOREIGN KEY (plan_revision_id) REFERENCES plan_revisions(id);

ALTER TABLE ONLY log_chunks
    ADD CONSTRAINT log_chunks_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES attempts(id);

ALTER TABLE ONLY node_pools
    ADD CONSTRAINT node_pools_emissary_target_release_fkey FOREIGN KEY (emissary_target_release) REFERENCES emissary_releases(id);

ALTER TABLE ONLY nodes
    ADD CONSTRAINT nodes_emissary_target_release_fkey FOREIGN KEY (emissary_target_release) REFERENCES emissary_releases(id);

ALTER TABLE ONLY nodes
    ADD CONSTRAINT nodes_pool_id_fkey FOREIGN KEY (pool_id) REFERENCES node_pools(id);

ALTER TABLE ONLY plan_revisions
    ADD CONSTRAINT plan_revisions_build_id_fkey FOREIGN KEY (build_id) REFERENCES builds(id);

ALTER TABLE ONLY plan_revisions
    ADD CONSTRAINT plan_revisions_run_id_fkey FOREIGN KEY (run_id) REFERENCES planning_runs(id);

ALTER TABLE ONLY plan_revisions
    ADD CONSTRAINT plan_revisions_source_snapshot_id_fkey FOREIGN KEY (source_snapshot_id) REFERENCES source_snapshots(id);

ALTER TABLE ONLY planning_events
    ADD CONSTRAINT planning_events_run_id_fkey FOREIGN KEY (run_id) REFERENCES planning_runs(id);

ALTER TABLE ONLY planning_runs
    ADD CONSTRAINT planning_runs_build_id_fkey FOREIGN KEY (build_id) REFERENCES builds(id);

ALTER TABLE ONLY planning_runs
    ADD CONSTRAINT planning_runs_worker_id_fkey FOREIGN KEY (worker_id) REFERENCES ai_workers(id);

ALTER TABLE ONLY projects
    ADD CONSTRAINT projects_credential_id_fkey FOREIGN KEY (credential_id) REFERENCES credentials(id);

ALTER TABLE ONLY projects
    ADD CONSTRAINT projects_model_profile_id_fkey FOREIGN KEY (model_profile_id) REFERENCES model_profiles(id) ON DELETE SET NULL;

ALTER TABLE ONLY publish_attempts
    ADD CONSTRAINT publish_attempts_build_id_fkey FOREIGN KEY (build_id) REFERENCES builds(id);

ALTER TABLE ONLY release_artifacts
    ADD CONSTRAINT release_artifacts_artifact_id_fkey FOREIGN KEY (artifact_id) REFERENCES artifacts(id);

ALTER TABLE ONLY release_artifacts
    ADD CONSTRAINT release_artifacts_release_id_fkey FOREIGN KEY (release_id) REFERENCES releases(id);

ALTER TABLE ONLY releases
    ADD CONSTRAINT releases_build_id_fkey FOREIGN KEY (build_id) REFERENCES builds(id);

ALTER TABLE ONLY releases
    ADD CONSTRAINT releases_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects(id);

ALTER TABLE ONLY remote_source_requests
    ADD CONSTRAINT remote_source_requests_build_id_fkey FOREIGN KEY (build_id) REFERENCES builds(id);

ALTER TABLE ONLY runtime_intents
    ADD CONSTRAINT runtime_intents_run_id_fkey FOREIGN KEY (run_id) REFERENCES planning_runs(id);

ALTER TABLE ONLY runtime_receipts
    ADD CONSTRAINT runtime_receipts_run_id_fkey FOREIGN KEY (run_id) REFERENCES planning_runs(id);

ALTER TABLE ONLY sessions
    ADD CONSTRAINT sessions_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id);

ALTER TABLE ONLY source_project_routes
    ADD CONSTRAINT source_project_routes_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects(id);

ALTER TABLE ONLY source_repositories
    ADD CONSTRAINT source_repositories_credential_id_fkey FOREIGN KEY (credential_id) REFERENCES credentials(id);

ALTER TABLE ONLY source_repositories
    ADD CONSTRAINT source_repositories_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects(id);

ALTER TABLE ONLY source_resolutions
    ADD CONSTRAINT source_resolutions_repository_id_fkey FOREIGN KEY (repository_id) REFERENCES source_repositories(id);

ALTER TABLE ONLY source_snapshots
    ADD CONSTRAINT source_snapshots_project_id_fkey FOREIGN KEY (project_id) REFERENCES projects(id);

ALTER TABLE ONLY source_snapshots
    ADD CONSTRAINT source_snapshots_root_repository_id_fkey FOREIGN KEY (root_repository_id) REFERENCES source_repositories(id);

ALTER TABLE ONLY source_tokens
    ADD CONSTRAINT source_tokens_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES attempts(id);

ALTER TABLE ONLY source_tokens
    ADD CONSTRAINT source_tokens_snapshot_id_fkey FOREIGN KEY (snapshot_id) REFERENCES source_snapshots(id);

ALTER TABLE ONLY step_runs
    ADD CONSTRAINT step_runs_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES attempts(id);

ALTER TABLE ONLY uploads
    ADD CONSTRAINT uploads_attempt_id_fkey FOREIGN KEY (attempt_id) REFERENCES attempts(id);

INSERT INTO node_pools (id,name) VALUES ('00000000-0000-0000-0000-000000000001','default');