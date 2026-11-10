ALTER TABLE builds ADD COLUMN source_repository_url text;
ALTER TABLE builds ADD COLUMN source_credential_id uuid REFERENCES credentials(id);
UPDATE builds b SET source_repository_url=p.repository_url,source_credential_id=p.credential_id FROM projects p WHERE p.id=b.project_id;
ALTER TABLE builds ALTER COLUMN source_repository_url SET NOT NULL;
