CREATE TABLE startup_resources (
    node_id uuid NOT NULL,
    startup_id uuid NOT NULL,
    sampled_at timestamptz NOT NULL,
    received_at timestamptz NOT NULL,
    sample jsonb NOT NULL,
    PRIMARY KEY(node_id,startup_id)
);
