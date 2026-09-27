-- The delivery queue before feder's. Migration 011 moved what was waiting in
-- it to eunha.feder_queue and kept it for one release, so that a rollback
-- past 011 would find its jobs; nothing has read it since.
DROP TABLE IF EXISTS eunha.activity_delivery_jobs;
