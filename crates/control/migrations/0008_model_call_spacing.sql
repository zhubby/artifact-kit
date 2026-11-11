-- Reserve model calls across tasks and worker restarts without storing keys.
ALTER TABLE model_profiles ADD COLUMN next_call_at timestamptz;
