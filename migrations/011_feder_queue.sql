-- Outgoing deliveries move to feder's queue.
--
-- The deliverer that sends them is feder's (`feder::deliverer`), and it keeps
-- its queue in the table feder-postgres describes, here in the `eunha` schema.
-- These are `feder_postgres::PostgresQueue::schema("eunha.feder_queue")`,
-- written out because eunha changes its schema only through migrations, never
-- from a server starting up; an integration test runs feder's queue checks
-- against the table this builds, so the two cannot drift apart unnoticed.
--
-- One table holds every named queue; deliveries are the `delivery` queue.
CREATE TABLE IF NOT EXISTS eunha.feder_queue (
    id BIGSERIAL PRIMARY KEY,
    queue TEXT NOT NULL,
    payload JSONB NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    run_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_error TEXT,
    failed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS eunha_feder_queue_due
    ON eunha.feder_queue (queue, run_at)
    WHERE failed_at IS NULL;

-- What was still waiting to be sent goes with it, attempts and all, due when
-- it was due. The sender is the key ID, which is how the new deliverer finds
-- the signing account. A claimed row's lease is dropped: nothing will finish
-- it now but this queue.
INSERT INTO eunha.feder_queue (queue, payload, attempts, run_at, last_error, created_at)
SELECT
    'delivery',
    jsonb_build_object('activity', activity, 'inbox', inbox_url, 'sender', key_id),
    attempts,
    run_at,
    last_error,
    created_at
FROM eunha.activity_delivery_jobs
WHERE delivered_at IS NULL AND failed_at IS NULL
ORDER BY id;

-- Marked as finished where they were, so that the release before this one,
-- started again against this database, does not send them a second time.
UPDATE eunha.activity_delivery_jobs
SET failed_at = now(),
    last_error = 'moved to eunha.feder_queue',
    updated_at = now()
WHERE delivered_at IS NULL AND failed_at IS NULL;

-- eunha.activity_delivery_jobs stays until a later migration, so that that
-- release can still start against the database. Nothing writes to it any more.
