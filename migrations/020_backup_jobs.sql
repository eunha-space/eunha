-- An archive takeout still to build (`BackupWorker`), with its retries
-- (src/portability/backup.rs), so that a restart loses no request.
CREATE TABLE eunha.backup_jobs (
    backup_id  BIGINT PRIMARY KEY REFERENCES public.backups(id) ON DELETE CASCADE,
    attempts   INTEGER NOT NULL DEFAULT 0,
    run_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    locked_at  TIMESTAMPTZ,
    locked_by  TEXT,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX backup_jobs_due ON eunha.backup_jobs (run_at, backup_id);
