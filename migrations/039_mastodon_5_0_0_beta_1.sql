-- Mastodon 5.0.0-beta.1 (20260820142255 through 20261016161747).
--
-- The tables for what the release adds: account reach filters, a row per FASP
-- capability, moderation list subscriptions and server health checks.
--
-- `20261016161747` drops `accounts.private_key`. This is the one destructive
-- step, and a private key cannot be derived from anything else. Moving a key
-- into `keypairs` means encrypting it, which SQL cannot do, so `eunha migrate`
-- moves them before it applies this file. Refuse rather than lose a key, should
-- one still be here all the same.

DO $$
DECLARE
    remaining bigint;
BEGIN
    SELECT count(*) INTO remaining FROM public.accounts
     WHERE domain IS NULL AND private_key IS NOT NULL AND private_key <> '';
    IF remaining > 0 THEN
        RAISE EXCEPTION '% local accounts still keep a signing key in accounts.private_key, which this migration drops; run eunha migrate with ActiveRecord encryption keys configured', remaining;
    END IF;
END
$$;

ALTER TABLE public.accounts DROP COLUMN private_key;

CREATE TABLE public.account_reach_filters (
    id bigserial PRIMARY KEY,
    account_id bigint NOT NULL,
    bloom_filter bytea,
    created_at timestamp(6) without time zone NOT NULL,
    salt character varying NOT NULL,
    saturated boolean DEFAULT false NOT NULL,
    updated_at timestamp(6) without time zone NOT NULL
);
CREATE UNIQUE INDEX index_account_reach_filters_on_account_id
    ON public.account_reach_filters USING btree (account_id);
ALTER TABLE ONLY public.account_reach_filters
    ADD CONSTRAINT fk_rails_f1ab63c96b FOREIGN KEY (account_id)
        REFERENCES public.accounts(id) ON DELETE CASCADE;

-- `fasp_providers.capabilities` becomes a table. Upstream's migration fails on
-- an entry with no `id` or `version`, or on a repeated one; a provider wrote
-- these, and an instance gets one attempt, so those are skipped or read as
-- Eunha has been reading them.
CREATE TABLE public.fasp_capabilities (
    id bigserial PRIMARY KEY,
    created_at timestamp(6) without time zone NOT NULL,
    enabled boolean DEFAULT false NOT NULL,
    fasp_provider_id bigint NOT NULL,
    name character varying NOT NULL,
    updated_at timestamp(6) without time zone NOT NULL,
    version character varying NOT NULL
);
CREATE UNIQUE INDEX idx_on_fasp_provider_id_name_version_68e2668987
    ON public.fasp_capabilities USING btree (fasp_provider_id, name, version);
CREATE INDEX index_fasp_capabilities_on_fasp_provider_id
    ON public.fasp_capabilities USING btree (fasp_provider_id);
ALTER TABLE ONLY public.fasp_capabilities
    ADD CONSTRAINT fk_rails_a6c03d8c46 FOREIGN KEY (fasp_provider_id)
        REFERENCES public.fasp_providers(id);

INSERT INTO public.fasp_capabilities
    (fasp_provider_id, name, version, enabled, created_at, updated_at)
SELECT p.id, c.value ->> 'id', COALESCE(c.value ->> 'version', ''),
       COALESCE(c.value ->> 'enabled' = 'true', false), p.updated_at, p.updated_at
  FROM public.fasp_providers p,
       jsonb_array_elements(p.capabilities) AS c
 WHERE jsonb_typeof(c.value) = 'object' AND c.value ->> 'id' IS NOT NULL
ON CONFLICT DO NOTHING;

ALTER TABLE public.fasp_providers DROP COLUMN capabilities;

CREATE TABLE public.moderation_subscriptions (
    id bigserial PRIMARY KEY,
    apply_automatically boolean DEFAULT false NOT NULL,
    created_at timestamp(6) without time zone NOT NULL,
    last_synced_at timestamp(6) without time zone,
    list_action integer,
    name character varying,
    override_unattributed boolean DEFAULT false NOT NULL,
    preserve_relationships boolean DEFAULT true NOT NULL,
    priority integer,
    retract_automatically boolean DEFAULT true NOT NULL,
    type integer NOT NULL,
    updated_at timestamp(6) without time zone NOT NULL,
    url character varying NOT NULL
);

CREATE TABLE public.moderation_subscription_advisories (
    id bigserial PRIMARY KEY,
    action integer NOT NULL,
    created_at timestamp(6) without time zone NOT NULL,
    moderation_subscription_id bigint NOT NULL,
    target_key character varying NOT NULL,
    target_type integer NOT NULL,
    updated_at timestamp(6) without time zone NOT NULL
);
CREATE INDEX index_moderation_advisories_on_moderation_subscription_id
    ON public.moderation_subscription_advisories USING btree (moderation_subscription_id);
CREATE UNIQUE INDEX idx_on_target_type_target_key_moderation_subscripti_a3f09c29b9
    ON public.moderation_subscription_advisories
    USING btree (target_type, target_key, moderation_subscription_id);
ALTER TABLE ONLY public.moderation_subscription_advisories
    ADD CONSTRAINT fk_rails_373d44cfb0 FOREIGN KEY (moderation_subscription_id)
        REFERENCES public.moderation_subscriptions(id);

CREATE TABLE public.moderation_suggestions (
    id bigserial PRIMARY KEY,
    moderation_subscription_id bigint,
    state integer DEFAULT 0 NOT NULL,
    action integer NOT NULL,
    target_type integer NOT NULL,
    target_key character varying NOT NULL,
    created_at timestamp(6) without time zone NOT NULL,
    updated_at timestamp(6) without time zone NOT NULL
);
CREATE INDEX index_moderation_suggestions_on_moderation_subscription_id
    ON public.moderation_suggestions USING btree (moderation_subscription_id);
CREATE UNIQUE INDEX idx_on_target_type_target_key_action_4f6ab01ebc
    ON public.moderation_suggestions USING btree (target_type, target_key, action);
ALTER TABLE ONLY public.moderation_suggestions
    ADD CONSTRAINT fk_rails_189e624c8b FOREIGN KEY (moderation_subscription_id)
        REFERENCES public.moderation_subscriptions(id) ON DELETE SET NULL;

ALTER TABLE public.domain_blocks ADD COLUMN moderation_subscription_id bigint;
CREATE INDEX index_domain_blocks_on_moderation_subscription_id
    ON public.domain_blocks USING btree (moderation_subscription_id);
ALTER TABLE ONLY public.domain_blocks
    ADD CONSTRAINT fk_rails_0d4a303c6d FOREIGN KEY (moderation_subscription_id)
        REFERENCES public.moderation_subscriptions(id) ON DELETE SET NULL;

ALTER TABLE public.domain_allows ADD COLUMN moderation_subscription_id bigint;
CREATE INDEX index_domain_allows_on_moderation_subscription_id
    ON public.domain_allows USING btree (moderation_subscription_id);
ALTER TABLE ONLY public.domain_allows
    ADD CONSTRAINT fk_rails_1be18c8bad FOREIGN KEY (moderation_subscription_id)
        REFERENCES public.moderation_subscriptions(id) ON DELETE SET NULL;

ALTER TABLE public.admin_action_logs ADD COLUMN moderation_subscription_id bigint;
CREATE INDEX index_admin_action_logs_on_moderation_subscription_id
    ON public.admin_action_logs USING btree (moderation_subscription_id)
    WHERE (moderation_subscription_id IS NOT NULL);

CREATE TABLE public.admin_server_checks (
    id bigserial PRIMARY KEY,
    key character varying NOT NULL,
    status smallint DEFAULT 0 NOT NULL,
    severity smallint DEFAULT 0 NOT NULL,
    ignored boolean DEFAULT false NOT NULL,
    data jsonb,
    checked_at timestamp(6) without time zone,
    created_at timestamp(6) without time zone NOT NULL,
    updated_at timestamp(6) without time zone NOT NULL
);
CREATE UNIQUE INDEX index_admin_server_checks_on_key
    ON public.admin_server_checks USING btree (key);

ALTER TABLE public.account_warning_presets
    ADD COLUMN action_category character varying DEFAULT 'all_actions'::character varying NOT NULL;

-- `20260702144128` too: it moved local keys into `keypairs`, and with the column
-- gone there is nowhere left for it to move them from.
INSERT INTO public.schema_migrations (version) VALUES
    ('20260702144128'),
    ('20260820142255'),
    ('20260925094651'),
    ('20260929112507'),
    ('20260929120312'),
    ('20260929123810'),
    ('20260929124143'),
    ('20260929124153'),
    ('20260929124323'),
    ('20260929125012'),
    ('20260930093209'),
    ('20260930093734'),
    ('20261013133732'),
    ('20261013134454'),
    ('20261016161747')
ON CONFLICT (version) DO NOTHING;
