-- `statuses.quote_approval_policy` is Mastodon's `InteractionPolicy` bitmap:
-- the automatic sub-policy in the high 16 bits, the manual one in the low 16,
-- each a set of flags (public 2, followers 4, following 8). Eunha wrote its
-- own enum to local posts instead: public 0, followers 1, nobody 2, manual 3.
--
-- Followers (1) and manual (3) are eunha's alone, and become the automatic
-- followers policy and the manual public one. A local post holding 2 is
-- eunha's nobody too, since Mastodon writes manual policies only for remote
-- posts. Eunha's public, 0, cannot be told apart from Mastodon's 0, which
-- lets no one quote, and stays as it is: the stricter of the two readings.
UPDATE public.statuses SET quote_approval_policy = 262144
WHERE local AND quote_approval_policy = 1;
UPDATE public.statuses SET quote_approval_policy = 2
WHERE local AND quote_approval_policy = 3;
UPDATE public.statuses SET quote_approval_policy = 0
WHERE local AND quote_approval_policy = 2;
