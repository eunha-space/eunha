-- A post that replies to nothing but opens with a mention is addressed to the
-- account it opens with, as X reads it: the timelines treat it like a reply
-- to that account (`crate::leading_mention`). Only posts written, received or
-- edited after this migration have a row; older ones stay as Mastodon reads
-- them.
CREATE TABLE eunha.leading_mentions (
    status_id bigint PRIMARY KEY REFERENCES public.statuses(id) ON DELETE CASCADE,
    account_id bigint NOT NULL REFERENCES public.accounts(id) ON DELETE CASCADE
);

CREATE INDEX leading_mentions_account_id ON eunha.leading_mentions (account_id);
