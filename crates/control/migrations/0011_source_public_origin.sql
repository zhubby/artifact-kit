-- Keep Pi/internal RPC routing separate from device-reachable Git/LFS URLs.
-- Existing snapshots retain their original origin until explicitly migrated.
ALTER TABLE source_snapshots ADD COLUMN public_origin text;
