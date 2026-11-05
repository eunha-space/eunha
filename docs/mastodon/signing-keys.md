Signing keys
============

Mastodon keeps local accounts' signing keys in `keypairs`, with the private half
encrypted the way Rails encrypts columns. Since 5.0 there is nowhere else: the
old `accounts.private_key` column is gone. Eunha needs the same secrets that
Mastodon requires, and reads and writes that form:

~~~~
ACTIVE_RECORD_ENCRYPTION__PRIMARY_KEY=...
ACTIVE_RECORD_ENCRYPTION__KEY_DERIVATION_SALT=...
~~~~

Without them an instance cannot sign, so it refuses to start. `eunha migrate`
moves any local keys still in `accounts.private_key` before the migration that
drops the column, which refuses to run while any are left.
