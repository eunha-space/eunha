-- Annual reports are Mastodon's `AnnualReport::SCHEMA` 2. Eunha once
-- labelled the reports it made schema 1, which Mastodon reads as its 2024
-- reports and fails on (`GeneratedAnnualReport#account_ids` plucks
-- `most_reblogged_accounts`, which they lack), although their data has
-- schema 2's keys. Those reports are labelled 2, and lose the most favourited
-- and most replied posts, which Mastodon's schema 2 leaves empty. A schema 1
-- report Mastodon made has `most_reblogged_accounts` and is left as it is.
UPDATE generated_annual_reports
SET schema_version = 2,
    data = jsonb_set(
        jsonb_set(data, '{top_statuses,by_favourites}', 'null'::jsonb),
        '{top_statuses,by_replies}', 'null'::jsonb
    )
WHERE schema_version = 1
  AND NOT data ? 'most_reblogged_accounts'
  AND data ? 'archetype'
  AND jsonb_typeof(data -> 'top_statuses') = 'object';
