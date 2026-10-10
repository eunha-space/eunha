-- When each scheduled task last ran, and which process is running it now.
--
-- Mastodon's sidekiq-scheduler remembers nothing: a cron slot passed while
-- Sidekiq was down is skipped, and an interval starts again at every boot.
-- Eunha restarts on every deploy, so a daily task whose period restarted with
-- the process never ran. One row per task (named by its `config/sidekiq.yml`
-- key) records the last run that finished, so that a missed run is made up
-- after a restart, and holds the lease of the run under way, so that the
-- processes serving an instance run each task once, one at a time.
--
--  -  `last_slot`: the run last finished: a cron entry's time, or the time an
--     interval run started.
--  -  `running_slot`, `leased_until`, `lease_token`: the run under way, and
--     until when its process holds it; a lease that has lapsed belonged to a
--     process that died, and may be taken over.

CREATE TABLE eunha.scheduled_tasks (
    name             TEXT PRIMARY KEY,
    last_slot        TIMESTAMPTZ,
    last_started_at  TIMESTAMPTZ,
    last_finished_at TIMESTAMPTZ,
    running_slot     TIMESTAMPTZ,
    leased_until     TIMESTAMPTZ,
    lease_token      BIGINT
);
