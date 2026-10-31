-- Mastodon's default staff roles: Moderator, Admin and Owner.
--
-- Mastodon creates them from `db/seeds/03_roles.rb`, which reads
-- `config/roles.yml` and runs `find_or_create_by(name:)` for each, so every
-- Mastodon database has them and a database eunha created fresh did not. Without
-- them there is no role to give an instance's first account — `tootctl accounts
-- create --role Owner` and `mastodon:setup` both look the Owner up by name.
--
-- Matched by name, as the seed is, so a database Mastodon already seeded keeps
-- the roles it has, including any an administrator has since edited.
--
-- `permissions` are the `UserRole::FLAGS` bits `roles.yml` lists:
--   Moderator  view_dashboard, view_audit_log, view_feeds, manage_users,
--              manage_reports, manage_taxonomies
--   Admin      the above, and manage_user_access, delete_user_data,
--              manage_federation, manage_settings, manage_blocks,
--              manage_appeals, manage_rules, manage_invites,
--              manage_announcements, manage_custom_emojis, manage_webhooks,
--              manage_roles
--   Owner      administrator
INSERT INTO public.user_roles (name, color, permissions, highlighted, position, created_at, updated_at)
SELECT seed.name, '', seed.permissions, true, seed.position, now(), now()
FROM (VALUES
    ('Moderator', 10, 1049884::bigint),
    ('Admin', 100, 2031612::bigint),
    ('Owner', 1000, 1::bigint)
) AS seed (name, position, permissions)
WHERE NOT EXISTS (SELECT 1 FROM public.user_roles r WHERE r.name = seed.name);
