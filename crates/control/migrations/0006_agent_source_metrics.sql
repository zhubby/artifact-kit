ALTER TABLE source_repositories ADD COLUMN agent_cache_hits bigint NOT NULL DEFAULT 0;
ALTER TABLE source_repositories ADD COLUMN agent_cache_misses bigint NOT NULL DEFAULT 0;
