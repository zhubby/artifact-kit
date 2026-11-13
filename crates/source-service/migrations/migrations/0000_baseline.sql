CREATE TABLE credentials (id uuid PRIMARY KEY, revision text NOT NULL, ciphertext text NOT NULL);
CREATE TABLE source_repositories (
 id uuid PRIMARY KEY, project_id uuid NOT NULL, repository_url text NOT NULL,
 credential_id uuid REFERENCES credentials(id), cache_key text NOT NULL UNIQUE,
 last_used_at timestamptz NOT NULL DEFAULT now(), fetched_bytes bigint NOT NULL DEFAULT 0,
 cache_hits bigint NOT NULL DEFAULT 0, cache_bytes bigint NOT NULL DEFAULT 0,
 git_download_bytes bigint NOT NULL DEFAULT 0, lfs_download_bytes bigint NOT NULL DEFAULT 0
);
CREATE TABLE source_snapshots (
 id uuid PRIMARY KEY, project_id uuid NOT NULL, root_repository_id uuid NOT NULL REFERENCES source_repositories(id),
 commit_sha text NOT NULL, manifest jsonb NOT NULL, digest text NOT NULL,
 durable boolean NOT NULL DEFAULT false, created_at timestamptz NOT NULL DEFAULT now(),
 last_used_at timestamptz NOT NULL DEFAULT now(), UNIQUE(project_id,digest)
);
CREATE TABLE source_resolutions (
 repository_id uuid NOT NULL REFERENCES source_repositories(id), reference text NOT NULL,
 commit_sha text NOT NULL, fetched_at timestamptz NOT NULL DEFAULT clock_timestamp(), PRIMARY KEY(repository_id,reference)
);
CREATE TABLE source_operations (
 id uuid PRIMARY KEY, input_hash text NOT NULL, root_commit text,
 snapshot_id uuid REFERENCES source_snapshots(id), created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE source_operation_repos (
 operation_id uuid NOT NULL REFERENCES source_operations(id), path text NOT NULL, tags jsonb NOT NULL,
 PRIMARY KEY(operation_id,path)
);
-- No destructive GC in v1: build/release retention is conservative and permanent.
-- Cache objects can be evicted only after durable=true; no CR deletion cascades.
CREATE TABLE source_retentions (
 snapshot_id uuid NOT NULL REFERENCES source_snapshots(id), owner text NOT NULL,
 created_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY(snapshot_id,owner)
);
