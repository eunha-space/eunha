-- Rails supplies application timestamps when creating an OAuth application.
-- Older Eunha inserts omitted them. The exact creation time was not recorded;
-- recover the earliest known authorization when available, preserving any
-- existing timestamp. Applications with no dated token use their update time
-- or the repair time as a final fallback.
UPDATE public.oauth_applications a
SET created_at = COALESCE(a.created_at,
        (SELECT min(t.created_at) FROM public.oauth_access_tokens t WHERE t.application_id = a.id),
        a.updated_at, now()),
    updated_at = COALESCE(a.updated_at, a.created_at,
        (SELECT min(t.created_at) FROM public.oauth_access_tokens t WHERE t.application_id = a.id), now())
WHERE a.created_at IS NULL OR a.updated_at IS NULL;
