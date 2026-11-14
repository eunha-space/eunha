-- Staff can take back a handout. Revoking expires the grant's codes the way
-- Mastodon expires any invite (`Expireable#expire!`, a touch of `expires_at`),
-- so `public.invites` keeps every row, use count and lineage edge; only the
-- grant itself records who revoked it and when.
ALTER TABLE eunha.invite_grants
    ADD COLUMN revoked_at timestamp without time zone,
    ADD COLUMN revoked_by_account_id bigint REFERENCES public.accounts(id) ON DELETE SET NULL;

-- `User#functional?`, written once for the queries that filter by it.
-- `crate::user_standing::UserStanding` reads the same facts from here, so the
-- invite pages, staff grants and `Invite#valid_for_use?` cannot drift apart.
CREATE VIEW eunha.user_standings AS
SELECT u.id AS user_id,
       u.account_id,
       u.confirmed_at IS NOT NULL AS confirmed,
       u.approved,
       u.disabled,
       (a.suspended_at IS NOT NULL OR a.requested_deletion_at IS NOT NULL) AS unavailable,
       a.requested_deletion_at IS NOT NULL AS deleted,
       a.memorial,
       a.moved_to_account_id IS NOT NULL AS moved,
       (COALESCE(r.require_2fa, false) AND NOT u.otp_required_for_login
        AND NOT EXISTS (SELECT 1 FROM public.webauthn_credentials w WHERE w.user_id = u.id))
         AS missing_2fa,
       (u.confirmed_at IS NOT NULL AND u.approved AND NOT u.disabled
        AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
        AND NOT a.memorial AND a.moved_to_account_id IS NULL
        AND NOT (COALESCE(r.require_2fa, false) AND NOT u.otp_required_for_login
                 AND NOT EXISTS (SELECT 1 FROM public.webauthn_credentials w WHERE w.user_id = u.id)))
         AS functional
FROM public.users u
JOIN public.accounts a ON a.id = u.account_id
LEFT JOIN public.user_roles r ON r.id = COALESCE(u.role_id, -99);
