-- Mastodon's `UserRole.everyone`: the role every account has unless given
-- another.
--
-- Mastodon creates it from `db/seeds`, and again on demand if it is missing, so
-- every Mastodon database has it and a database eunha created fresh did not.
-- Without it `verify_credentials` carries no `role`, and a client reading its
-- permissions to decide what to offer sees nothing rather than the defaults.
--
-- `permissions` is `UserRole::Flags::DEFAULT`, which is `invite_users` (1 << 16).
-- The id is `UserRole::EVERYONE_ROLE_ID`, which Mastodon hard-codes.
INSERT INTO public.user_roles (id, name, color, permissions, highlighted, position, created_at, updated_at)
VALUES (-99, '', '', 65536, false, 0, now(), now())
ON CONFLICT (id) DO NOTHING;
