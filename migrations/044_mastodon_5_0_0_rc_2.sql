-- Mastodon 5.0.0-rc.2 (20261112113100 through 20261123145826).
--
-- Whether a strike's mail lists the rules it broke, one index fewer on FASP
-- capabilities, and broadcast relays: a relay now names its actor, says whether
-- it passes posts on to this server, and may need each member to opt in, which
-- `relay_preferences` records.

-- 20261112113100_add_rule_violations_to_account_warning_table
ALTER TABLE public.account_warnings
    ADD COLUMN include_rule_violations boolean DEFAULT true NOT NULL;

-- 20261120134347_remove_duplicate_fasp_capabilities_provider_index: the unique
-- index on (fasp_provider_id, name, version) covers it.
DROP INDEX public.index_fasp_capabilities_on_fasp_provider_id;

-- 20261123103147_add_uri_to_relays
ALTER TABLE public.relays
    ADD COLUMN uri character varying DEFAULT ''::character varying NOT NULL,
    ADD COLUMN direction integer DEFAULT 0 NOT NULL,
    ADD COLUMN requires_opt_in boolean DEFAULT false NOT NULL;

-- 20261123115445_create_relay_preferences
CREATE TABLE public.relay_preferences (
    id bigserial PRIMARY KEY,
    account_id bigint NOT NULL,
    relay_id bigint NOT NULL,
    enabled boolean DEFAULT false NOT NULL,
    created_at timestamp(6) without time zone NOT NULL,
    updated_at timestamp(6) without time zone NOT NULL
);
CREATE UNIQUE INDEX index_relay_preferences_on_account_id_and_relay_id
    ON public.relay_preferences USING btree (account_id, relay_id);
CREATE INDEX index_relay_preferences_on_relay_id
    ON public.relay_preferences USING btree (relay_id);
ALTER TABLE ONLY public.relay_preferences
    ADD CONSTRAINT fk_rails_e71924b6bf FOREIGN KEY (account_id)
        REFERENCES public.accounts(id) ON DELETE CASCADE;
ALTER TABLE ONLY public.relay_preferences
    ADD CONSTRAINT fk_rails_5f785ca5e2 FOREIGN KEY (relay_id)
        REFERENCES public.relays(id) ON DELETE CASCADE;

-- 20261123122202_add_account_id_to_relays
ALTER TABLE public.relays ADD COLUMN account_id bigint;
ALTER TABLE ONLY public.relays
    ADD CONSTRAINT fk_rails_e2475c1454 FOREIGN KEY (account_id)
        REFERENCES public.accounts(id) ON DELETE CASCADE;

-- 20261123145826_backfill_relays_account_id: each relay takes the actor whose
-- inbox it delivers to. Upstream deletes a relay no account has that inbox
-- for, as this does; a relay that has answered a `Follow` has signed it, so its
-- actor is known.
UPDATE public.relays
   SET account_id = a.id, uri = a.uri
  FROM (SELECT DISTINCT ON (inbox_url) id, uri, inbox_url
          FROM public.accounts
         WHERE inbox_url <> ''
         ORDER BY inbox_url, id) a
 WHERE a.inbox_url = public.relays.inbox_url;
DELETE FROM public.relays WHERE account_id IS NULL;
ALTER TABLE public.relays ALTER COLUMN account_id SET NOT NULL;
CREATE UNIQUE INDEX index_relays_on_uri ON public.relays USING btree (uri);

INSERT INTO public.schema_migrations (version) VALUES
    ('20261112113100'),
    ('20261120134347'),
    ('20261123103147'),
    ('20261123115445'),
    ('20261123122202'),
    ('20261123145826')
ON CONFLICT (version) DO NOTHING;
