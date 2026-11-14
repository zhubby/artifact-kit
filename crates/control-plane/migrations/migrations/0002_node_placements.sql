ALTER TABLE nodes ADD COLUMN metadata jsonb NOT NULL DEFAULT '{}',
    ADD COLUMN startup_id uuid,
    ADD COLUMN metadata_digest text,
    ADD COLUMN metadata_received_at timestamptz;
CREATE TABLE node_startups (
    node_id uuid NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    startup_id uuid NOT NULL,
    digest text NOT NULL,
    accepted_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(node_id,startup_id)
);
CREATE TABLE job_placements (
    job_id uuid PRIMARY KEY REFERENCES jobs(id) ON DELETE CASCADE,
    placement jsonb NOT NULL,
    version bigint NOT NULL DEFAULT 1,
    waiting_since timestamptz,
    reason text,
    selected_node uuid,
    selected_reason text
);
CREATE TABLE placement_rounds (
    build_id uuid NOT NULL REFERENCES builds(id) ON DELETE CASCADE,
    round integer NOT NULL CHECK(round BETWEEN 1 AND 2),
    run_id uuid UNIQUE NOT NULL REFERENCES planning_runs(id) ON DELETE CASCADE,
    versions jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(build_id,round)
);
ALTER TABLE planning_runs DROP CONSTRAINT planning_runs_purpose_check;
ALTER TABLE planning_runs ADD CONSTRAINT planning_runs_purpose_check
    CHECK(purpose IN ('initial','revision','clarification','placement'));
CREATE INDEX job_placements_waiting ON job_placements(waiting_since) WHERE waiting_since IS NOT NULL;
