-- A timed mute is lifted by the `DeleteMuteWorker` that `MuteService` queues
-- for when it expires; nothing reads `mutes.expires_at` to ignore a mute that
-- has expired. Eunha used to queue no such job, so every timed mute already in
-- the database gets the job it is owed, due when the mute expires (or at once,
-- for one that already has). A database whose mutes were made by Mastodon is
-- owed the same, since its jobs stayed in Mastodon's Sidekiq.
-- `src/api/mastodon/accounts/mutes_blocks.rs` (`queue_expiries`) does the
-- same after `eunha import-mastodon`.
INSERT INTO eunha.jobs (queue, kind, args, run_at, max_retries, keep_dead)
SELECT 'default', 'DeleteMuteWorker', jsonb_build_object('mute_id', m.id),
       m.expires_at AT TIME ZONE 'UTC', 25, true
FROM public.mutes m
WHERE m.expires_at IS NOT NULL
  AND NOT EXISTS (
    SELECT 1 FROM eunha.jobs j
    WHERE j.kind = 'DeleteMuteWorker' AND j.dead_at IS NULL
      AND j.args = jsonb_build_object('mute_id', m.id)
  );
