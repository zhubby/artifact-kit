ALTER TABLE nodes ADD COLUMN connection_owner uuid;
-- Heartbeats change last_seen continuously. Keep it out of secondary indexes
-- and leave room for HOT updates rather than rewriting every device index.
ALTER TABLE nodes SET (fillfactor=80);
ALTER TABLE jobs ADD COLUMN dispatch_after timestamptz NOT NULL DEFAULT now();
CREATE INDEX dispatch_queue ON jobs(dispatch_after,created_at,id) WHERE status='queued';
CREATE INDEX available_nodes ON nodes(pool_id,os,arch) WHERE NOT revoked AND NOT draining AND NOT ota_draining;
CREATE INDEX owned_connections ON nodes(connection_owner,id) WHERE connection_owner IS NOT NULL;
CREATE INDEX active_builds ON builds(id) WHERE status IN ('queued','running','canceling');

-- Store references, never credentials or assignment payloads. Recreate commands
-- from immutable attempt inputs and current authorization when delivering.
CREATE TABLE device_commands (
    id uuid PRIMARY KEY,
    node_id uuid NOT NULL REFERENCES nodes(id),
    attempt_id uuid NOT NULL REFERENCES attempts(id),
    kind text NOT NULL CHECK(kind IN ('assign','cancel')),
    next_delivery_at timestamptz NOT NULL DEFAULT now(),
    acknowledged_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE(attempt_id,kind)
);
CREATE INDEX pending_device_commands ON device_commands(node_id,next_delivery_at) WHERE acknowledged_at IS NULL;
CREATE INDEX acknowledged_device_commands ON device_commands(acknowledged_at) WHERE acknowledged_at IS NOT NULL;

CREATE FUNCTION signal_control_work() RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    -- NOTIFY serializes commits on a database-wide lock. Only one overlapping
    -- transaction sends a wakeup; durable work and periodic reconciliation
    -- also cover transactions that become visible after that wakeup.
    IF pg_try_advisory_xact_lock(hashtextextended('artifactory:work-notify',12)) THEN
        PERFORM pg_notify('artifactory_work','');
    END IF;
END;
$$;

CREATE FUNCTION notify_control_work() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM signal_control_work();
    RETURN NULL;
END;
$$;
CREATE TRIGGER queued_job_work AFTER INSERT OR UPDATE OF status ON jobs
    FOR EACH STATEMENT EXECUTE FUNCTION notify_control_work();
-- A reconnect wakes its new owner locally. NOTIFY commits serialize on a
-- database-wide lock, so sending one for every session creates a recovery storm.
CREATE TRIGGER node_connection_work AFTER UPDATE OF draining,revoked,slots,pool_id ON nodes
    FOR EACH STATEMENT EXECUTE FUNCTION notify_control_work();

CREATE FUNCTION reschedule_project_work() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE jobs j SET dispatch_after=clock_timestamp() FROM builds b
    WHERE j.build_id=b.id AND b.project_id=NEW.id AND j.status='queued';
    PERFORM signal_control_work();
    RETURN NULL;
END;
$$;
CREATE TRIGGER project_scheduling_changed AFTER UPDATE OF signing_profiles,pool_ids ON projects
    FOR EACH ROW EXECUTE FUNCTION reschedule_project_work();

CREATE FUNCTION persist_device_command() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='INSERT' AND NEW.status='leased' THEN
        INSERT INTO device_commands(id,node_id,attempt_id,kind)
        VALUES(NEW.command_id,NEW.node_id,NEW.id,'assign');
    END IF;
    IF TG_OP='UPDATE' THEN
        UPDATE device_commands SET acknowledged_at=clock_timestamp()
        WHERE attempt_id=NEW.id AND acknowledged_at IS NULL
          AND (NEW.status NOT IN ('leased','running') OR (kind='assign' AND NEW.status='running'));
    END IF;
    PERFORM signal_control_work();
    RETURN NULL;
END;
$$;
CREATE TRIGGER attempt_command AFTER INSERT OR UPDATE OF status ON attempts
    FOR EACH ROW EXECUTE FUNCTION persist_device_command();

CREATE FUNCTION persist_device_cancellation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.cancel_requested AND NOT OLD.cancel_requested THEN
        INSERT INTO device_commands(id,node_id,attempt_id,kind)
        SELECT gen_random_uuid(),a.node_id,a.id,'cancel' FROM attempts a JOIN jobs j ON j.id=a.job_id
        WHERE j.build_id=NEW.id AND a.status IN ('leased','running')
        ON CONFLICT(attempt_id,kind) DO NOTHING;
        PERFORM signal_control_work();
    END IF;
    RETURN NULL;
END;
$$;
CREATE TRIGGER build_cancellation AFTER UPDATE OF cancel_requested ON builds
    FOR EACH ROW EXECUTE FUNCTION persist_device_cancellation();

-- In-flight attempts survive the migration and can be replayed immediately.
INSERT INTO device_commands(id,node_id,attempt_id,kind)
SELECT command_id,node_id,id,'assign' FROM attempts WHERE status='leased';
INSERT INTO device_commands(id,node_id,attempt_id,kind)
SELECT gen_random_uuid(),a.node_id,a.id,'cancel' FROM attempts a JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id
WHERE a.status IN ('leased','running') AND b.cancel_requested;
