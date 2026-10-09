-- `accounts.protocol` is `ostatus: 0, activitypub: 1`, and defaults to 0.
-- Mastodon stores 1 for every actor it takes from ActivityPub
-- (`ProcessAccountService#set_immediate_protocol_attributes!`), and sends
-- nothing to an account that is not `activitypub?`. Eunha left the column at
-- its default until it stored remote actors as Mastodon does, so a remote
-- account it made earlier and has not refetched since reads as OStatus and
-- would be sent nothing. Each remote account still at 0 that has an inbox,
-- which only an ActivityPub actor gives, is marked ActivityPub; an account
-- left from Mastodon's OStatus days has none, and is left as it is.
UPDATE accounts
SET protocol = 1
WHERE domain IS NOT NULL
  AND protocol = 0
  AND inbox_url <> '';
