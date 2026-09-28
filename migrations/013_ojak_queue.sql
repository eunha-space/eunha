-- feder is now ojak, and its queue table takes the new name: ojak-postgres
-- describes eunha's queue as `ojak_postgres::PostgresQueue::schema(
-- "eunha.ojak_queue")`. The rows move with the table, so nothing waiting to
-- be sent is lost.
ALTER TABLE IF EXISTS eunha.feder_queue RENAME TO ojak_queue;
ALTER INDEX IF EXISTS eunha.eunha_feder_queue_due RENAME TO eunha_ojak_queue_due;
ALTER SEQUENCE IF EXISTS eunha.feder_queue_id_seq RENAME TO ojak_queue_id_seq;
ALTER INDEX IF EXISTS eunha.feder_queue_pkey RENAME TO ojak_queue_pkey;
