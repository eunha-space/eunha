-- Keep Mastodon's public.invites unchanged. A staff grant is an admission
-- decision; its links bypass review without changing the holder's role.
CREATE TABLE eunha.invite_grants (
    id bigserial PRIMARY KEY,
    granted_by_account_id bigint REFERENCES public.accounts(id) ON DELETE SET NULL,
    created_at timestamp without time zone NOT NULL DEFAULT now()
);
CREATE TABLE eunha.granted_invites (
    invite_id bigint PRIMARY KEY REFERENCES public.invites(id) ON DELETE CASCADE,
    grant_id bigint NOT NULL REFERENCES eunha.invite_grants(id) ON DELETE CASCADE
);
CREATE INDEX granted_invites_grant_id ON eunha.granted_invites(grant_id);
