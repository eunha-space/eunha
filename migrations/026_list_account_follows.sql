-- `POST /api/v1/lists/:id/accounts` used to add a member without the follow
-- (or follow request) Mastodon's `ListAccount#set_follow` hangs it on. The
-- list feeds now distribute as Mastodon does, to `list_accounts` rows with a
-- `follow_id` (`Account#lists_for_local_distribution`, `ListAccount.active`),
-- so those members are linked to the follow, or else the follow request,
-- they were added under. The owner's own membership has neither.
UPDATE public.list_accounts la
SET follow_id = f.id
FROM public.lists l, public.follows f
WHERE l.id = la.list_id
  AND la.follow_id IS NULL
  AND la.follow_request_id IS NULL
  AND la.account_id <> l.account_id
  AND f.account_id = l.account_id
  AND f.target_account_id = la.account_id;

UPDATE public.list_accounts la
SET follow_request_id = fr.id
FROM public.lists l, public.follow_requests fr
WHERE l.id = la.list_id
  AND la.follow_id IS NULL
  AND la.follow_request_id IS NULL
  AND la.account_id <> l.account_id
  AND fr.account_id = l.account_id
  AND fr.target_account_id = la.account_id;
