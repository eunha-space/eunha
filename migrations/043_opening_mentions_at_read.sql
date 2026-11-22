-- Whom a post opens by mentioning is read from its text when a timeline is
-- read (`crate::leading_mention`), so that posts from before eunha read them,
-- and imported ones, are read the same way; nothing is recorded any more.
DROP TABLE eunha.leading_mentions;

-- Whether a post's text opens with a mention: a local post's source text with
-- `@handle`, a remote post's HTML with a link inside at most a paragraph and
-- a span, such as Mastodon's `h-card`. Anchored, so it reads only the start.
-- `crate::leading_mention` reads the same openings.
CREATE FUNCTION eunha.opens_with_mention(text text, local boolean) RETURNS boolean
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT CASE WHEN local
        THEN text ~ '^\s*[@＠][[:alnum:]_]'
        ELSE text ~ '^\s*(<p[^>]*>\s*)?(<span[^>]*>\s*)?<a\s[^>]*class="[^"]*\mmention\M'
    END
$$;
