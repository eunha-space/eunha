-- The work Mastodon hands to Sidekiq, kept where a restart cannot lose it
-- (src/jobs/mod.rs, docs/operating/jobs.md).
--
-- A row is one job: a worker class (`kind`) and its arguments, the queue it
-- waits in, when it is next due, and how many times it has failed. A job that
-- succeeds is deleted. One that has failed more times than its worker allows
-- is deleted, or kept with `dead_at` set when the worker keeps its dead jobs,
-- as Sidekiq's dead set does.
--
-- `unique_key` is sidekiq-unique-jobs' lock: while a row holds it, no other
-- job with the same key is queued. `until_executed` holds it until the job
-- has run; `until_executing` gives it up as the job is claimed
-- (`unlock_on_claim`). `unique_until` is the lock's TTL, past which a new job
-- may take the key from a row that still holds it.
CREATE TABLE eunha.jobs (
    id              BIGSERIAL PRIMARY KEY,
    queue           TEXT NOT NULL,
    kind            TEXT NOT NULL,
    args            JSONB NOT NULL,
    run_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    attempts        INTEGER NOT NULL DEFAULT 0,
    max_retries     INTEGER NOT NULL,
    keep_dead       BOOLEAN NOT NULL,
    unique_key      TEXT,
    unique_until    TIMESTAMPTZ,
    unlock_on_claim BOOLEAN NOT NULL DEFAULT false,
    locked_at       TIMESTAMPTZ,
    locked_by       TEXT,
    last_error      TEXT,
    failed_at       TIMESTAMPTZ,
    dead_at         TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX jobs_due ON eunha.jobs (run_at, id) WHERE dead_at IS NULL;
CREATE INDEX jobs_dead ON eunha.jobs (dead_at) WHERE dead_at IS NOT NULL;
CREATE UNIQUE INDEX jobs_unique ON eunha.jobs (unique_key) WHERE unique_key IS NOT NULL;
