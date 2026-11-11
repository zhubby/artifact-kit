CREATE INDEX registration_attempts ON audit_events(created_at) WHERE action='registration_attempt';
