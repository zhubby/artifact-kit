ALTER TABLE attempts ADD COLUMN failure_kind text NOT NULL DEFAULT 'execution'
    CHECK (failure_kind IN ('execution','source','authorization','artifact_transport','agent'));
