CREATE TABLE agent_releases (
 id uuid PRIMARY KEY,
 version text NOT NULL UNIQUE,
 manifest text NOT NULL,
 signature text NOT NULL,
 manifest_hash text NOT NULL UNIQUE,
 created_at timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE node_pools ADD COLUMN agent_target_release uuid REFERENCES agent_releases(id);
ALTER TABLE node_pools ADD COLUMN agent_update_concurrency integer NOT NULL DEFAULT 1 CHECK(agent_update_concurrency BETWEEN 1 AND 32);
ALTER TABLE node_pools ADD COLUMN agent_updates_paused boolean NOT NULL DEFAULT false;
ALTER TABLE nodes ADD COLUMN agent_target_release uuid REFERENCES agent_releases(id);
ALTER TABLE nodes ADD COLUMN agent_current_version text;
ALTER TABLE nodes ADD COLUMN agent_launcher_version text;
ALTER TABLE nodes ADD COLUMN agent_ota_status jsonb NOT NULL DEFAULT '{}';
ALTER TABLE nodes ADD COLUMN ota_draining boolean NOT NULL DEFAULT false;
CREATE TABLE agent_updates (
 id uuid PRIMARY KEY,
 generation bigserial NOT NULL UNIQUE,
 node_id uuid NOT NULL REFERENCES nodes(id),
 pool_id uuid NOT NULL REFERENCES node_pools(id),
 release_id uuid NOT NULL REFERENCES agent_releases(id),
 previous_version text,
 status text NOT NULL DEFAULT 'queued' CHECK(status IN ('queued','offered','downloading','draining','activating','succeeded','failed','rolled_back','canceled')),
 cancel_requested boolean NOT NULL DEFAULT false,
 error text,
 retry_of uuid REFERENCES agent_updates(id),
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now(),
 finished_at timestamptz,
 last_command_at timestamptz
);
CREATE UNIQUE INDEX agent_updates_one_active_node ON agent_updates(node_id) WHERE finished_at IS NULL;
CREATE INDEX agent_updates_pool_active ON agent_updates(pool_id) WHERE finished_at IS NULL;
