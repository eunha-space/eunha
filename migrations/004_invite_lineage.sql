-- Durable record of "who invited whom", kept in the `eunha` schema so `public`
-- stays a pure Mastodon mirror.
--
-- Mastodon derives invite lineage from `users.invite_id -> invites.user_id`.
-- Account deletion destroys the `users` row (that is where the email and the
-- rest of the personal data live), which cascades away that account's `invites`
-- and nulls `users.invite_id` for everyone it invited — so the lineage powering
-- eunha's invite tree would be lost along with the PII.
--
-- `DeleteAccountService` therefore snapshots the lineage here immediately
-- before destroying the user rows: one row per account, naming the account that
-- invited it. Only account ids are stored, never email or any other user data.

CREATE TABLE eunha.invite_lineage (
    account_id         BIGINT PRIMARY KEY REFERENCES public.accounts(id) ON DELETE CASCADE,
    -- The inviter's account. NULL once the inviter's own account row is gone
    -- (a full purge, e.g. an unconfirmed signup or a remote Delete(actor)).
    inviter_account_id BIGINT REFERENCES public.accounts(id) ON DELETE SET NULL,
    -- When the invited account signed up (the `users.created_at` we snapshot).
    invited_at         TIMESTAMPTZ,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX invite_lineage_inviter_idx
    ON eunha.invite_lineage (inviter_account_id)
    WHERE inviter_account_id IS NOT NULL;
