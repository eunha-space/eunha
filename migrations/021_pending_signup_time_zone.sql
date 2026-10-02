-- The time zone a sign-up asked for (`time_zone` on `POST /api/v1/accounts`),
-- kept until the account is created from it so it can become
-- `users.time_zone`, as `AppSignUpService` writes it.
ALTER TABLE eunha.pending_signups ADD COLUMN IF NOT EXISTS time_zone varchar;
