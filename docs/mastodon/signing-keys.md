Signing keys
============

Mastodon 4.7.0 keeps local accounts' signing keys in `keypairs`, with the
private half encrypted the way Rails encrypts columns. Give Eunha the same
secrets that Mastodon requires and it reads and writes that form, moving any
keys still in the old `accounts` columns on startup:

~~~~
ACTIVE_RECORD_ENCRYPTION__PRIMARY_KEY=...
ACTIVE_RECORD_ENCRYPTION__KEY_DERIVATION_SALT=...
~~~~

Without them, keys stay in `accounts.private_key`, which upstream still reads
(`Keypair.from_legacy_account`) — but a database whose keys have already moved
cannot be signed with, and Eunha says so loudly at startup.
