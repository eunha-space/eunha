-- Data imports now run on the job queue, as Mastodon runs them on Sidekiq:
-- `BulkImportWorker`, then an `Import::RowWorker` for each row
-- (src/portability/import.rs). `eunha.bulk_import_progress` kept the place of
-- the import loop they replace, and goes.
--
-- An import still being carried out is handed to the queue where that loop
-- would have taken it up: one whose first pass has not been done gets its
-- `BulkImportWorker`, and one whose first pass has gets a row job for each row
-- after the last one handled. Mastodon's own workers' imports, scheduled or in
-- progress, are taken up the same way, as the loop took them up.
INSERT INTO eunha.jobs (queue, kind, args, max_retries, keep_dead)
SELECT 'pull', 'BulkImportWorker', jsonb_build_object('bulk_import_id', b.id), -1, false
FROM public.bulk_imports b
LEFT JOIN eunha.bulk_import_progress p ON p.bulk_import_id = b.id
WHERE b.state IN (1, 2)
  AND (p.prepared = false OR (p.bulk_import_id IS NULL AND b.state = 1))
ORDER BY b.id;

INSERT INTO eunha.jobs (queue, kind, args, max_retries, keep_dead)
SELECT 'pull', 'Import::RowWorker', jsonb_build_object('bulk_import_row_id', r.id), 6, false
FROM public.bulk_imports b
LEFT JOIN eunha.bulk_import_progress p ON p.bulk_import_id = b.id
JOIN public.bulk_import_rows r ON r.bulk_import_id = b.id
WHERE b.state IN (1, 2)
  AND (p.prepared = true OR (p.bulk_import_id IS NULL AND b.state = 2))
  AND r.id > coalesce(p.last_row_id, 0)
ORDER BY r.id;

DROP TABLE eunha.bulk_import_progress;
