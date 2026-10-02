-- Eunha used to serve the site's title, descriptions, contact address,
-- registrations, privacy policy and terms of service from the instance
-- configuration until an administrator saved the settings; it now reads them
-- from Mastodon's tables alone. An instance that was already serving gets
-- what its configuration said copied into the settings nobody has saved, once,
-- by the next `eunha migrate` (`eunha settings import-config`). This row says
-- that copy is still owed. A database with no users yet was not serving, so it
-- is owed nothing: it starts from Mastodon's defaults, or from the Mastodon
-- data `eunha import-mastodon` restores into it.
CREATE TABLE eunha.site_settings_import (
    owed_since TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO eunha.site_settings_import (owed_since)
SELECT now() WHERE EXISTS (SELECT 1 FROM public.users);
