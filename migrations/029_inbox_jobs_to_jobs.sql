-- Inbound activities wait in the job queue (eunha.jobs) as
-- `ActivityPub::ProcessingWorker` jobs on the `ingress` queue, retried eight
-- times on Sidekiq's schedule, as Mastodon queues them; eunha.inbox_jobs, the
-- queue of their own they had, goes.
--
-- What is still waiting there is handed over first: a pending activity as a
-- job due when its row was, with the retries it has used; one that failed
-- for good into the dead set, as Sidekiq keeps a job out of retries.
INSERT INTO eunha.jobs
    (queue, kind, args, run_at, attempts, max_retries, keep_dead,
     last_error, failed_at, dead_at, created_at)
SELECT 'ingress',
       'ActivityPub::ProcessingWorker',
       jsonb_build_object('activity', activity),
       run_at,
       attempts,
       8,
       true,
       last_error,
       failed_at,
       failed_at,
       created_at
FROM eunha.inbox_jobs
ORDER BY id;

DROP TABLE eunha.inbox_jobs;
