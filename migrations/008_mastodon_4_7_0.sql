-- Mastodon 4.6.0 -> 4.7.0.
--
-- Reproduces the 27 Rails migrations upstream added between the two releases
-- (`eunha-schema plan --to v4.7.0` lists them). Verify the result with
-- `eunha-schema check`, which diffs the live database against upstream's
-- db/schema.rb for the tracked release.
--
-- Upstream builds its large indexes CONCURRENTLY; eunha's migrations run at
-- startup through sqlx, where an aborted non-transactional migration would
-- leave the schema half-built, so this runs as one transaction instead. The
-- price is that `accounts` and `follow_requests` are locked while their
-- indexes build. On a large instance, run this during a maintenance window.

-- 20260618114230 add_recorded_changes_to_action_logs
ALTER TABLE public.admin_action_logs
    ADD COLUMN IF NOT EXISTS recorded_changes jsonb,
    ADD COLUMN IF NOT EXISTS recorded_changes_format character varying;

-- 20260629124918 change_keypair_uri_non_nullable
-- 20260629125635 add_local_fragment_to_keypair
ALTER TABLE public.keypairs
    ALTER COLUMN uri DROP NOT NULL,
    ADD COLUMN IF NOT EXISTS local_fragment character varying;

-- 20260629125939, 20260701160541, 20260701161457, 20260701161826
-- Upstream first added this index partially (WHERE local_fragment IS NOT NULL),
-- then replaced it with an unconditional one under the same name. Only the end
-- state matters here.
CREATE UNIQUE INDEX IF NOT EXISTS index_keypairs_on_account_id_and_local_fragment
    ON public.keypairs USING btree (account_id, local_fragment);

-- 20260630070531 / 20260630070600 restore index_keypairs_on_uri, which 4.6.0
-- already has under that name, so there is nothing to do for them.

-- 20260812154114 remove_index_keypairs_on_account_id
DROP INDEX IF EXISTS public.index_keypairs_on_account_id;

-- 20260706122112 add_end_of_support_to_software_updates
ALTER TABLE public.software_updates
    ADD COLUMN IF NOT EXISTS end_of_support date;

-- 20260706143100 create_software_deprecations
CREATE TABLE IF NOT EXISTS public.software_deprecations (
    id             bigint NOT NULL,
    branch         character varying NOT NULL,
    end_of_support date NOT NULL,
    warning_issued integer NOT NULL,
    created_at     timestamp(6) without time zone NOT NULL,
    updated_at     timestamp(6) without time zone NOT NULL
);

CREATE SEQUENCE IF NOT EXISTS public.software_deprecations_id_seq
    AS bigint START WITH 1 INCREMENT BY 1 NO MINVALUE NO MAXVALUE CACHE 1;
ALTER SEQUENCE public.software_deprecations_id_seq OWNED BY public.software_deprecations.id;
ALTER TABLE ONLY public.software_deprecations
    ALTER COLUMN id SET DEFAULT nextval('public.software_deprecations_id_seq'::regclass);

DO $$
BEGIN
    ALTER TABLE ONLY public.software_deprecations
        ADD CONSTRAINT software_deprecations_pkey PRIMARY KEY (id);
EXCEPTION
    WHEN duplicate_table THEN NULL;
    WHEN invalid_table_definition THEN NULL;
END
$$;

CREATE UNIQUE INDEX IF NOT EXISTS index_software_deprecations_on_branch
    ON public.software_deprecations USING btree (branch);

-- 20260720090737 change_account_uri_nullable
-- 20260720092724 change_account_uri_default
-- A missing URI is now expressed as NULL rather than the empty string, which is
-- what makes the unique index below possible.
ALTER TABLE public.accounts
    ALTER COLUMN uri DROP NOT NULL,
    ALTER COLUMN uri DROP DEFAULT;

-- 20260720100326 fix_blank_account_uri
UPDATE public.accounts SET uri = NULL WHERE uri = '';

-- 20260720124731 clean_up_invalid_accounts
-- An old upstream bug could persist a remote account before its URI was known.
-- Such rows cannot be addressed or refetched; upstream deletes them, and the
-- usual foreign keys carry the deletion through to their dependent rows.
DELETE FROM public.accounts WHERE domain IS NOT NULL AND uri IS NULL;

-- 20260720103819 rename_index_accounts_on_uri_to_old
-- 20260720104058 add_unique_index_on_accounts_uri
-- 20260720113713 remove_old_index_on_accounts_uri
--
-- An actor's `id` is now Mastodon's primary identifier for an account, so the
-- URI has to be unique. Where duplicates exist, upstream merges them into the
-- most recently webfingered account; this does the same, discovering what to
-- re-point from the foreign keys that reference accounts(id) rather than from a
-- hand-maintained list of tables.
DO $$
DECLARE
    duplicate  RECORD;
    loser      RECORD;
    reference  RECORD;
    keeper_id  bigint;
    row_to_move RECORD;
BEGIN
    FOR duplicate IN
        SELECT uri FROM public.accounts
        WHERE uri IS NOT NULL
        GROUP BY uri HAVING count(*) > 1
    LOOP
        SELECT id INTO keeper_id
        FROM public.accounts
        WHERE uri = duplicate.uri
        ORDER BY COALESCE(last_webfingered_at, '-infinity'::timestamp) DESC, created_at DESC, id DESC
        LIMIT 1;

        FOR loser IN
            SELECT id FROM public.accounts WHERE uri = duplicate.uri AND id <> keeper_id
        LOOP
            FOR reference IN
                SELECT n.nspname AS schema_name, t.relname AS table_name, a.attname AS column_name
                FROM pg_constraint c
                JOIN pg_class t ON t.oid = c.conrelid
                JOIN pg_namespace n ON n.oid = t.relnamespace
                JOIN pg_class ft ON ft.oid = c.confrelid
                JOIN pg_namespace fn ON fn.oid = ft.relnamespace
                JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = c.conkey[1]
                WHERE c.contype = 'f'
                  AND fn.nspname = 'public'
                  AND ft.relname = 'accounts'
                  AND array_length(c.conkey, 1) = 1
            LOOP
                BEGIN
                    EXECUTE format(
                        'UPDATE %I.%I SET %I = $1 WHERE %I = $2',
                        reference.schema_name, reference.table_name,
                        reference.column_name, reference.column_name
                    ) USING keeper_id, loser.id;
                EXCEPTION WHEN unique_violation THEN
                    -- The keeper already has the equivalent row (the same
                    -- follow, mute, favourite...). Move what can be moved and
                    -- leave the rest to be removed with the losing account.
                    FOR row_to_move IN EXECUTE format(
                        'SELECT ctid FROM %I.%I WHERE %I = $1',
                        reference.schema_name, reference.table_name, reference.column_name
                    ) USING loser.id
                    LOOP
                        BEGIN
                            EXECUTE format(
                                'UPDATE %I.%I SET %I = $1 WHERE ctid = $2',
                                reference.schema_name, reference.table_name, reference.column_name
                            ) USING keeper_id, row_to_move.ctid;
                        EXCEPTION WHEN unique_violation THEN
                            NULL;
                        END;
                    END LOOP;
                END;
            END LOOP;

            DELETE FROM public.accounts WHERE id = loser.id;
        END LOOP;
    END LOOP;
END
$$;

DROP INDEX IF EXISTS public.index_accounts_on_uri;
CREATE UNIQUE INDEX index_accounts_on_uri ON public.accounts USING btree (uri);

-- 20260728124057 add_requested_deletion_at_to_accounts
ALTER TABLE public.accounts
    ADD COLUMN IF NOT EXISTS requested_deletion_at timestamp(6) without time zone;

-- 20260728145507 backfill_account_requested_deletion_at
-- Until 4.7, a local account that deleted itself was recorded as suspended.
-- The two are now distinct: a self-deleted account has requested_deletion_at
-- set and is no longer suspended. An account with a canonical email block was
-- suspended by a moderator, so it stays suspended.
UPDATE public.accounts
SET requested_deletion_at = suspended_at,
    suspended_at = NULL,
    suspension_origin = NULL
WHERE domain IS NULL
  AND suspended_at IS NOT NULL
  AND NOT EXISTS (SELECT 1 FROM public.users WHERE account_id = accounts.id)
  AND NOT EXISTS (
      SELECT 1 FROM public.canonical_email_blocks WHERE reference_account_id = accounts.id
  );

-- 20260803172525 add_target_account_index_to_follow_requests
CREATE INDEX IF NOT EXISTS index_follow_requests_on_target_account_id_and_account_id
    ON public.follow_requests USING btree (target_account_id, account_id);

-- 20260805130216 fix_generated_annual_reports_foreign_key
ALTER TABLE public.generated_annual_reports
    DROP CONSTRAINT IF EXISTS fk_rails_4ca37f035c;
ALTER TABLE public.generated_annual_reports
    ADD CONSTRAINT fk_rails_4ca37f035c FOREIGN KEY (account_id)
        REFERENCES public.accounts(id) ON DELETE CASCADE;

-- 20260728145403 update_account_summaries_to_version_3
-- 20260803140751 add_back_languages_index_to_account_summaries
-- 20260804081821 convert_materialized_views_to_tables
--
-- Follow recommendations stop being materialized views recomputed wholesale and
-- become plain tables the application maintains. Upstream reaches the same end
-- state through a v3 view it then converts; building the tables directly gets
-- there in one step. Any rows already materialized are carried over.
CREATE TABLE public.tmp_account_summaries (
    account_id bigint NOT NULL,
    language   character varying,
    sensitive  boolean DEFAULT false NOT NULL
);

CREATE TABLE public.tmp_global_follow_recommendations (
    account_id bigint NOT NULL,
    rank       numeric NOT NULL,
    reason     character varying[] NOT NULL,
    stale      boolean DEFAULT false NOT NULL
);

-- Only a populated materialized view has rows to keep, and only accounts that
-- still exist survive the copy: the views had no foreign keys, the tables do.
DO $$
BEGIN
    IF (SELECT relispopulated FROM pg_class WHERE oid = 'public.account_summaries'::regclass) THEN
        INSERT INTO public.tmp_account_summaries (account_id, language, sensitive)
        SELECT s.account_id, s.language, s.sensitive
        FROM public.account_summaries s
        JOIN public.accounts a ON a.id = s.account_id;
    END IF;

    IF (SELECT relispopulated FROM pg_class WHERE oid = 'public.global_follow_recommendations'::regclass) THEN
        INSERT INTO public.tmp_global_follow_recommendations (account_id, rank, reason, stale)
        SELECT r.account_id, r.rank, r.reason, false
        FROM public.global_follow_recommendations r
        JOIN public.accounts a ON a.id = r.account_id;
    END IF;
END
$$;

DROP MATERIALIZED VIEW public.global_follow_recommendations;
DROP MATERIALIZED VIEW public.account_summaries;

ALTER TABLE public.tmp_account_summaries RENAME TO account_summaries;
ALTER TABLE public.tmp_global_follow_recommendations RENAME TO global_follow_recommendations;

-- Postgres names NOT NULL constraints after the table they were created on, and
-- renaming the table does not rename them. Left alone, these would keep their
-- `tmp_` names for good, which is a difference a schema comparison sees and a
-- migration that drops a constraint by name would trip over.
ALTER TABLE public.account_summaries
    RENAME CONSTRAINT tmp_account_summaries_account_id_not_null
                   TO account_summaries_account_id_not_null;
ALTER TABLE public.account_summaries
    RENAME CONSTRAINT tmp_account_summaries_sensitive_not_null
                   TO account_summaries_sensitive_not_null;
ALTER TABLE public.global_follow_recommendations
    RENAME CONSTRAINT tmp_global_follow_recommendations_account_id_not_null
                   TO global_follow_recommendations_account_id_not_null;
ALTER TABLE public.global_follow_recommendations
    RENAME CONSTRAINT tmp_global_follow_recommendations_rank_not_null
                   TO global_follow_recommendations_rank_not_null;
ALTER TABLE public.global_follow_recommendations
    RENAME CONSTRAINT tmp_global_follow_recommendations_reason_not_null
                   TO global_follow_recommendations_reason_not_null;
ALTER TABLE public.global_follow_recommendations
    RENAME CONSTRAINT tmp_global_follow_recommendations_stale_not_null
                   TO global_follow_recommendations_stale_not_null;

CREATE SEQUENCE public.account_summaries_account_id_seq
    AS bigint START WITH 1 INCREMENT BY 1 NO MINVALUE NO MAXVALUE CACHE 1;
ALTER SEQUENCE public.account_summaries_account_id_seq OWNED BY public.account_summaries.account_id;
ALTER TABLE ONLY public.account_summaries
    ALTER COLUMN account_id SET DEFAULT nextval('public.account_summaries_account_id_seq'::regclass);

CREATE SEQUENCE public.global_follow_recommendations_account_id_seq
    AS bigint START WITH 1 INCREMENT BY 1 NO MINVALUE NO MAXVALUE CACHE 1;
ALTER SEQUENCE public.global_follow_recommendations_account_id_seq
    OWNED BY public.global_follow_recommendations.account_id;
ALTER TABLE ONLY public.global_follow_recommendations
    ALTER COLUMN account_id SET DEFAULT nextval('public.global_follow_recommendations_account_id_seq'::regclass);

ALTER TABLE ONLY public.account_summaries
    ADD CONSTRAINT account_summaries_pkey PRIMARY KEY (account_id);
ALTER TABLE ONLY public.global_follow_recommendations
    ADD CONSTRAINT global_follow_recommendations_pkey PRIMARY KEY (account_id);

CREATE INDEX idx_on_account_id_language_sensitive_250461e1eb
    ON public.account_summaries USING btree (account_id, language, sensitive);
CREATE INDEX index_global_follow_recommendations_on_rank
    ON public.global_follow_recommendations USING btree (rank);

-- Rails derives a foreign key's name from a hash of the table and column, and
-- its own migrations drop constraints by that name, so the names have to match
-- rather than merely describe the same relationship.
ALTER TABLE ONLY public.account_summaries
    ADD CONSTRAINT fk_rails_4ea43ef304 FOREIGN KEY (account_id)
        REFERENCES public.accounts(id) ON DELETE CASCADE;
ALTER TABLE ONLY public.global_follow_recommendations
    ADD CONSTRAINT fk_rails_bacefde68a FOREIGN KEY (account_id)
        REFERENCES public.accounts(id) ON DELETE CASCADE;

-- Record what upstream would have recorded.
--
-- 20260702144128 (migrate_local_account_keypairs) is deliberately absent: it
-- moves local private keys into `keypairs` and blanks `accounts.private_key`,
-- and eunha still signs from `accounts`. Claiming it were applied would tell a
-- Mastodon booted on this database that the move had happened. It stays
-- outstanding until eunha reads its keys from `keypairs`.
INSERT INTO public.schema_migrations (version) VALUES
    ('20260618114230'),
    ('20260629124918'),
    ('20260629125635'),
    ('20260629125939'),
    ('20260630070531'),
    ('20260630070600'),
    ('20260701160541'),
    ('20260701161457'),
    ('20260701161826'),
    ('20260706122112'),
    ('20260706143100'),
    ('20260720090737'),
    ('20260720092724'),
    ('20260720100326'),
    ('20260720103819'),
    ('20260720104058'),
    ('20260720113713'),
    ('20260720124731'),
    ('20260728124057'),
    ('20260728145403'),
    ('20260728145507'),
    ('20260803140751'),
    ('20260803172525'),
    ('20260804081821'),
    ('20260805130216'),
    ('20260812154114')
ON CONFLICT (version) DO NOTHING;
