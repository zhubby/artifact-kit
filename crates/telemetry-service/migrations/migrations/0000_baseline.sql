-- Extension installation belongs to the database administrator, before application migrations.
CREATE TABLE telemetry_batches (
    node_id uuid NOT NULL, batch_id uuid NOT NULL, digest text NOT NULL,
    received_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY(node_id,batch_id)
);
CREATE INDEX telemetry_batches_expiry ON telemetry_batches(received_at);

CREATE TABLE host_samples (
    sampled_at timestamptz NOT NULL, node_id uuid NOT NULL, batch_id uuid NOT NULL,
    session_id uuid NOT NULL, monotonic_ms bigint NOT NULL, received_at timestamptz NOT NULL,
    cpu_percent double precision, memory_total_bytes bigint, memory_used_bytes bigint,
    swap_total_bytes bigint, swap_used_bytes bigint, load_one double precision,
    active_attempts integer NOT NULL, dropped_batches bigint NOT NULL,
    devices jsonb NOT NULL, PRIMARY KEY(node_id,sampled_at,batch_id)
);
SELECT create_hypertable('host_samples', by_range('sampled_at', INTERVAL '1 day'));
CREATE INDEX host_samples_node_time ON host_samples(node_id,sampled_at DESC);
ALTER TABLE host_samples SET (timescaledb.enable_columnstore=true, timescaledb.segmentby='node_id', timescaledb.orderby='sampled_at DESC');
CALL add_columnstore_policy('host_samples', after=>INTERVAL '1 day');
SELECT add_retention_policy('host_samples', INTERVAL '7 days');

CREATE TABLE process_samples (
    sampled_at timestamptz NOT NULL, node_id uuid NOT NULL, batch_id uuid NOT NULL,
    attempt_id uuid NOT NULL, pid bigint NOT NULL, started_at bigint NOT NULL,
    parent_pid bigint, step bigint, cpu_percent double precision, rss_bytes bigint,
    read_bytes bigint, written_bytes bigint, status text NOT NULL,
    PRIMARY KEY(node_id,sampled_at,batch_id,pid,started_at)
);
SELECT create_hypertable('process_samples', by_range('sampled_at', INTERVAL '6 hours'));
CREATE INDEX process_samples_attempt_time ON process_samples(attempt_id,sampled_at DESC);
ALTER TABLE process_samples SET (timescaledb.enable_columnstore=true, timescaledb.segmentby='node_id,attempt_id', timescaledb.orderby='sampled_at DESC');
CALL add_columnstore_policy('process_samples', after=>INTERVAL '1 day');
SELECT add_retention_policy('process_samples', INTERVAL '3 days');

-- One row per managed attempt/tick keeps long-term aggregates independent of PID cardinality.
CREATE TABLE build_samples (
    sampled_at timestamptz NOT NULL, node_id uuid NOT NULL, batch_id uuid NOT NULL,
    attempt_id uuid NOT NULL, cpu_percent double precision, rss_bytes bigint,
    read_bytes bigint, written_bytes bigint, process_count integer NOT NULL,
    complete boolean NOT NULL, PRIMARY KEY(node_id,sampled_at,batch_id,attempt_id)
);
SELECT create_hypertable('build_samples', by_range('sampled_at', INTERVAL '6 hours'));
CREATE INDEX build_samples_attempt_time ON build_samples(attempt_id,sampled_at DESC);
ALTER TABLE build_samples SET (timescaledb.enable_columnstore=true, timescaledb.segmentby='node_id,attempt_id', timescaledb.orderby='sampled_at DESC');
CALL add_columnstore_policy('build_samples', after=>INTERVAL '1 day');
SELECT add_retention_policy('build_samples', INTERVAL '3 days');

CREATE TABLE inventory_versions (
    node_id uuid NOT NULL, digest text NOT NULL, first_seen timestamptz NOT NULL,
    inventory jsonb NOT NULL, PRIMARY KEY(node_id,digest)
);
CREATE TABLE inventory_observations (
    node_id uuid NOT NULL, sampled_at timestamptz NOT NULL, digest text NOT NULL,
    PRIMARY KEY(node_id,sampled_at)
);
CREATE TABLE latest_inventories (
    node_id uuid PRIMARY KEY, sampled_at timestamptz NOT NULL, digest text NOT NULL
);
CREATE TABLE latest_hosts (
    node_id uuid PRIMARY KEY, sampled_at timestamptz NOT NULL, received_at timestamptz NOT NULL,
    session_id uuid NOT NULL, monotonic_ms bigint NOT NULL, dropped_batches bigint NOT NULL,
    sample jsonb NOT NULL
);

CREATE MATERIALIZED VIEW host_minutes WITH (timescaledb.continuous, timescaledb.materialized_only=false) AS
SELECT time_bucket(INTERVAL '1 minute',sampled_at) AS bucket, node_id,
    count(*) AS samples, count(cpu_percent) AS cpu_samples,
    sum(cpu_percent) AS cpu_sum, max(cpu_percent) AS cpu_max,
    count(memory_used_bytes) AS memory_samples, sum(memory_used_bytes::double precision) AS memory_sum,
    max(memory_used_bytes) AS memory_max, max(active_attempts) AS active_attempts_max
FROM host_samples GROUP BY bucket,node_id WITH NO DATA;
SELECT add_continuous_aggregate_policy('host_minutes', start_offset=>INTERVAL '6 days', end_offset=>INTERVAL '1 minute', schedule_interval=>INTERVAL '1 minute');
SELECT add_retention_policy('host_minutes', INTERVAL '30 days');

CREATE MATERIALIZED VIEW host_hours WITH (timescaledb.continuous, timescaledb.materialized_only=false) AS
SELECT time_bucket(INTERVAL '1 hour',bucket) AS bucket, node_id,
    sum(samples) AS samples, sum(cpu_samples) AS cpu_samples, sum(cpu_sum) AS cpu_sum,
    max(cpu_max) AS cpu_max, sum(memory_samples) AS memory_samples, sum(memory_sum) AS memory_sum,
    max(memory_max) AS memory_max, max(active_attempts_max) AS active_attempts_max
FROM host_minutes GROUP BY 1,node_id WITH NO DATA;
SELECT add_continuous_aggregate_policy('host_hours', start_offset=>INTERVAL '29 days', end_offset=>INTERVAL '1 hour', schedule_interval=>INTERVAL '1 hour');
SELECT add_retention_policy('host_hours', INTERVAL '180 days');

CREATE MATERIALIZED VIEW build_minutes WITH (timescaledb.continuous, timescaledb.materialized_only=false) AS
SELECT time_bucket(INTERVAL '1 minute',sampled_at) AS bucket, node_id, attempt_id,
    count(*) AS samples, count(cpu_percent) AS cpu_samples, sum(cpu_percent) AS cpu_sum,
    max(cpu_percent) AS cpu_max, count(rss_bytes) AS rss_samples, sum(rss_bytes::double precision) AS rss_sum,
    max(rss_bytes) AS rss_max, max(process_count) AS process_count_max, bool_and(complete) AS complete
FROM build_samples GROUP BY bucket,node_id,attempt_id WITH NO DATA;
SELECT add_continuous_aggregate_policy('build_minutes', start_offset=>INTERVAL '60 hours', end_offset=>INTERVAL '1 minute', schedule_interval=>INTERVAL '1 minute');
SELECT add_retention_policy('build_minutes', INTERVAL '30 days');

CREATE MATERIALIZED VIEW build_hours WITH (timescaledb.continuous, timescaledb.materialized_only=false) AS
SELECT time_bucket(INTERVAL '1 hour',bucket) AS bucket, node_id, attempt_id,
    sum(samples) AS samples, sum(cpu_samples) AS cpu_samples, sum(cpu_sum) AS cpu_sum,
    max(cpu_max) AS cpu_max, sum(rss_samples) AS rss_samples, sum(rss_sum) AS rss_sum,
    max(rss_max) AS rss_max, max(process_count_max) AS process_count_max, bool_and(complete) AS complete
FROM build_minutes GROUP BY 1,node_id,attempt_id WITH NO DATA;
SELECT add_continuous_aggregate_policy('build_hours', start_offset=>INTERVAL '29 days', end_offset=>INTERVAL '1 hour', schedule_interval=>INTERVAL '1 hour');
SELECT add_retention_policy('build_hours', INTERVAL '180 days');
