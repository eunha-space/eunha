Deliberate divergences
======================

Eunha aims for behavioural parity, so a difference from Mastodon is either a bug
or a decision. The decisions live in `divergences.toml`, one entry each, saying
what Mastodon does, what eunha does instead, why, and which test would fail if
that stopped being true.

They are recorded as data rather than prose because prose is not checked and so
stops being true. `cargo test` reads that file: an entry whose evidence has gone
missing fails, and — the part that matters over time — every entry carries the
Mastodon release it was last judged against, so **adopting a newer release fails
the suite until each divergence has been re-examined**. A divergence that made
sense against one release is not automatically right against the next; upstream
may have adopted the same idea, changed what is being diverged from, or ruled it
out. `mise run mastodon:plan` prints them when adopting, so the question is
asked at the moment it can be answered.

The recorded decisions cover:

 -  integrity proofs on outgoing activities;
 -  the invite tree, invite management and grants, and public signup invite
    resolution and the admission approval carried by staff grants;
 -  what the update check asks about;
 -  when the local-keypair migration is recorded;
 -  the moderation tools API, and the server administration API;
 -  the account move API;
 -  what `eunha accounts create --force` checks before it deletes;
 -  the admin custom emoji API;
 -  the private metrics listener;
 -  how a link preview card reads a page's character set;
 -  which private addresses are refused;
 -  the email subscription API, and unsubscribe links;
 -  how async refresh ids are signed, and who an embedded note that names
    another author is taken to be by;
 -  the terms of service admin API and interstitial;
 -  the data export, import and archive takeout API;
 -  the two-factor authentication API, and where a sign-in waits for its
    second factor;
 -  the sessions, authorized apps and sign-in history API, and how a password
    reset token is stored;
 -  deleting one's own account and changing its email address over the
    API, and the preferences Mastodon sets only through web forms;
 -  how notification emails are unsubscribed from;
 -  the DeepL endpoint setting;
 -  the auxiliary service provider admin API;
 -  remote media linked rather than downloaded, which the operator chose;
 -  the web push title of a collection notification, and the locales push
    titles are written in;
 -  the collection addresses eunha named before, still answered;
 -  the followers digests, cached in Redis entries of eunha's own;
 -  deliveries to a suspended domain, dropped as they are queued;
 -  the date forms a remote poll's end is read in;
 -  Mastodon's web UI endpoints under `/api/web`, which eunha does not serve;
 -  self-destruct mode in eunha's terms;
 -  where an OAuth denial is sent;
 -  the tooling for changing an instance's domain;
 -  where a signed-out browser signs in to authorize an OAuth client;
 -  the language of a notification's fallback, and how it names a
    collection's owner;
 -  the pictures the instance API names when none were uploaded;
 -  the range of a dashboard measure or dimension asked for without one;
 -  what the dashboard's software versions and space usage report;
 -  the shared Wrapstodon page, drawn by eunha's web app;
 -  the rate limits counted at eunha's own sign-in and account pages, and
    the switch that turns them off, and the locales the rate limit message
    is written in;
 -  how a profile link's redirect back to the account is asked about;
 -  `tootctl`'s feeds, cache, statuses, media and preview card commands in
    eunha's terms;
 -  mail written in eunha's own words, its text part made from its HTML;
 -  the keys `eunha accounts rotate` replaces, and the instance actor that it
    and `eunha accounts follow` leave out;
 -  what `eunha accounts refresh` fetches, and how two rows for one remote
    actor are told apart;
 -  the schema and the running processes `eunha maintenance fix-duplicates`
    works with;
 -  the audit log entry for a hashtag reviewed through the admin API;
 -  the `Remove` of a collection a moderator deletes.

Read the file rather than this paragraph: the file is the one that has to stay
true.
