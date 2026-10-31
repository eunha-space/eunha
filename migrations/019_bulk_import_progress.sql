-- Where eunha keeps the work Mastodon gives Sidekiq for a member's data
-- import (src/portability/import.rs), so that a restart resumes it.
--
-- An import being carried out: whether `BulkImportService`'s first pass is
-- done, and the last `bulk_import_rows` row handled. Rows are handled in id
-- order. The lease keeps two workers off the same import.
CREATE TABLE eunha.bulk_import_progress (
    bulk_import_id BIGINT PRIMARY KEY REFERENCES public.bulk_imports(id) ON DELETE CASCADE,
    prepared       BOOLEAN NOT NULL DEFAULT false,
    last_row_id    BIGINT NOT NULL DEFAULT 0,
    locked_at      TIMESTAMPTZ,
    locked_by      TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ
);
