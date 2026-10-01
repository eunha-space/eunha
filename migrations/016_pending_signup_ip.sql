-- The address a sign-up came from, kept until the account is created from it
-- so it can become `users.sign_up_ip`: what Mastodon's IP blocks and the
-- admin API's `ip` filter look at.
ALTER TABLE eunha.pending_signups ADD COLUMN IF NOT EXISTS sign_up_ip inet;
