ALTER TABLE source_repositories ADD COLUMN cache_bytes bigint NOT NULL DEFAULT 0;
ALTER TABLE source_repositories ADD COLUMN git_download_bytes bigint NOT NULL DEFAULT 0;
ALTER TABLE source_repositories ADD COLUMN lfs_download_bytes bigint NOT NULL DEFAULT 0;
