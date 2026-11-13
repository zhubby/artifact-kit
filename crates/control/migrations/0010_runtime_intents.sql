ALTER TABLE projects ADD COLUMN planner_backend text NOT NULL DEFAULT 'poll' CHECK(planner_backend IN ('poll','kubernetes'));
ALTER TABLE builds ADD COLUMN planner_backend text NOT NULL DEFAULT 'poll' CHECK(planner_backend IN ('poll','kubernetes'));
ALTER TABLE planning_runs ADD COLUMN authority text NOT NULL DEFAULT 'poll' CHECK(authority IN ('poll','kubernetes'));
ALTER TABLE source_snapshots ADD COLUMN service_origin text;
CREATE TABLE runtime_intents (
 id bigserial PRIMARY KEY, run_id uuid NOT NULL REFERENCES planning_runs(id), version bigint NOT NULL,
 kind text NOT NULL, payload_ciphertext text, delivered_at timestamptz,
 created_at timestamptz NOT NULL DEFAULT now(), UNIQUE(run_id,version)
);
CREATE TABLE runtime_receipts (
 event_id uuid PRIMARY KEY, input_hash text NOT NULL, result jsonb NOT NULL,
 run_id uuid NOT NULL REFERENCES planning_runs(id), generation bigint, created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE remote_source_requests (
 build_id uuid PRIMARY KEY REFERENCES builds(id), origin text NOT NULL, payload_ciphertext text,
 lease_until timestamptz, created_at timestamptz NOT NULL DEFAULT now(), error text
);
CREATE TABLE source_project_routes (project_id uuid PRIMARY KEY REFERENCES projects(id), origin text NOT NULL);
CREATE FUNCTION pin_build_backend() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN NEW.planner_backend := (SELECT planner_backend FROM projects WHERE id=NEW.project_id); RETURN NEW; END $$;
CREATE TRIGGER pin_build_backend BEFORE INSERT ON builds FOR EACH ROW EXECUTE FUNCTION pin_build_backend();
CREATE FUNCTION planning_runtime_intent() RETURNS trigger LANGUAGE plpgsql AS $$
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
CREATE TRIGGER planning_authority BEFORE INSERT ON planning_runs FOR EACH ROW EXECUTE FUNCTION planning_runtime_intent();
CREATE FUNCTION publish_runtime_intent() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.authority='kubernetes' THEN
  IF TG_OP='INSERT' THEN INSERT INTO runtime_intents(run_id,version,kind) VALUES(NEW.id,1,'create');
  ELSIF NEW.state='canceled' AND OLD.state<>'canceled' THEN INSERT INTO runtime_intents(run_id,version,kind) VALUES(NEW.id,2,'cancel') ON CONFLICT DO NOTHING;
  END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER runtime_outbox AFTER INSERT OR UPDATE OF state ON planning_runs FOR EACH ROW EXECUTE FUNCTION publish_runtime_intent();
