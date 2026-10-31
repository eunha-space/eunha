-- Rows eunha wrote with its own integers for two of Mastodon's enums, moved to
-- Mastodon's.
--
-- `Report#category` is `{ other: 0, spam: 1_000, legal: 1_500, violation:
-- 2_000 }`; eunha wrote spam as 1 and violation as 2. Mastodon never writes
-- either, so every such row is eunha's.
UPDATE public.reports SET category = 1000 WHERE category = 1;
UPDATE public.reports SET category = 2000 WHERE category = 2;

-- `IpBlock#severity` is `{ sign_up_requires_approval: 5000, sign_up_block:
-- 5500, no_access: 9999 }`; eunha wrote 1, 2 and 3 for them, and 0 for a
-- "noop" Mastodon does not have. Nothing ever enforced a noop IP block, and
-- Mastodon cannot load one, so those rows go.
UPDATE public.ip_blocks SET severity = 5000 WHERE severity = 1;
UPDATE public.ip_blocks SET severity = 5500 WHERE severity = 2;
UPDATE public.ip_blocks SET severity = 9999 WHERE severity = 3;
DELETE FROM public.ip_blocks WHERE severity = 0;

-- `DomainBlock#severity` (`{ silence: 0, suspend: 1, noop: 2 }`) cannot be
-- repaired here: eunha used the same three integers in another order, so a row
-- does not say which of the two wrote it. See docs/operating/migrations.md.
