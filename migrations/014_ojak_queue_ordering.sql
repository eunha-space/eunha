-- ojak's queue keeps an ordering key with a job queued in order, and a claim
-- passes over a job while one before it with the same key is still there.
-- These are the two statements `ojak_postgres::PostgresQueue::schema(
-- "eunha.ojak_queue")` adds for it, written out because eunha changes its
-- schema only through migrations; the queue checks in
-- tests/integration/federation/delivery_queue.rs run against the table this
-- builds. Eunha queues nothing in order, as Mastodon delivers nothing in
-- order, but every claim reads the column.
ALTER TABLE eunha.ojak_queue ADD COLUMN IF NOT EXISTS ordering_key TEXT;

CREATE INDEX IF NOT EXISTS eunha_ojak_queue_ordering
    ON eunha.ojak_queue (ordering_key, id)
    WHERE ordering_key IS NOT NULL AND failed_at IS NULL;
