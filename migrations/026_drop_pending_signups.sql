-- Sign-ups used to wait in eunha.pending_signups until their link was
-- followed; they are now `users` rows with no `confirmed_at`, as Mastodon
-- writes them. `eunha migrate` turns the sign-ups still within their day into
-- such users before applying this, keeping the token their mail carries so
-- the link still works (`accounts::convert_pending_signups`); what is left is
-- expired, or could not be converted, and goes with the table.
DROP TABLE IF EXISTS eunha.pending_signups;
