CREATE TABLE runtime_runs (
 id uuid PRIMARY KEY, version bigint NOT NULL, intent_hash text, intent_ciphertext text,
 project_id uuid, pool text, snapshot_id uuid, snapshot_digest text,
 lifecycle jsonb NOT NULL, status text NOT NULL, execution jsonb,
 grant_hash text, grant_ciphertext text, checkpoint jsonb, usage jsonb NOT NULL DEFAULT '{}',
 report_sequence bigint NOT NULL DEFAULT 0, created_at timestamptz NOT NULL DEFAULT now(),
 dispatched_at timestamptz, updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX runtime_queue ON runtime_runs(status,created_at);
CREATE TABLE runtime_profiles (id uuid PRIMARY KEY, next_call_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE runtime_model_requests (
 run_id uuid NOT NULL REFERENCES runtime_runs(id), request_id uuid NOT NULL,
 input_hash text NOT NULL, generation bigint NOT NULL, state text NOT NULL DEFAULT 'reserved',
 created_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY(run_id,request_id)
);
CREATE TABLE runtime_reports (
 run_id uuid NOT NULL REFERENCES runtime_runs(id), event_id uuid NOT NULL, input_hash text NOT NULL,
 PRIMARY KEY(run_id,event_id)
);
CREATE TABLE runtime_proposals (
 id uuid PRIMARY KEY, run_id uuid NOT NULL REFERENCES runtime_runs(id), generation bigint NOT NULL,
 input_hash text NOT NULL, kind text NOT NULL, payload jsonb NOT NULL,
 deadline_ms bigint NOT NULL, result jsonb, created_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX one_pending_proposal ON runtime_proposals(run_id) WHERE result IS NULL;
CREATE TABLE runtime_outbox (
 id uuid PRIMARY KEY, run_id uuid NOT NULL REFERENCES runtime_runs(id), kind text NOT NULL,
 payload jsonb NOT NULL, delivered boolean NOT NULL DEFAULT false,
 created_at timestamptz NOT NULL DEFAULT now()
);
