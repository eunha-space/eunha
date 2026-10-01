-- Mastodon keeps the handle a member typed for an alias in
-- `account_aliases.acct` and the actor id it resolved to in `uri`, and adds
-- that id to `accounts.also_known_as`, which is what a local actor serves as
-- `alsoKnownAs`. Eunha stored the handle in `uri`, left `acct` empty, and
-- served `alsoKnownAs` from the aliases instead of the column.
--
-- A handle stored that way becomes the `acct` it should have been. Aliases
-- that do hold an actor id are copied into `also_known_as` for the local
-- accounts whose column is still empty, which a database Mastodon kept never
-- has alongside an alias.
UPDATE public.account_aliases
SET acct = regexp_replace(btrim(uri), '^@', '')
WHERE acct = '' AND uri !~ '^https?://';

UPDATE public.accounts a
SET also_known_as = aliases.uris
FROM (
    SELECT account_id, array_agg(uri ORDER BY id) AS uris
    FROM public.account_aliases
    WHERE uri ~ '^https?://'
    GROUP BY account_id
) aliases
WHERE a.id = aliases.account_id
  AND a.domain IS NULL
  AND coalesce(cardinality(a.also_known_as), 0) = 0;
