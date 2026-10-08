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
 -  the web push title of a collection notification;
 -  the collection addresses eunha named before, still answered;
 -  the followers digests, cached in Redis entries of eunha's own;
 -  deliveries to a suspended domain, dropped as they are queued;
 -  the date forms a remote poll's end is read in;
 -  Mastodon's web UI endpoints under `/api/web`, which eunha does not serve;
 -  self-destruct mode in eunha's terms;
 -  where an OAuth denial is sent;
 -  the tooling for changing an instance's domain.

Read the file rather than this paragraph: the file is the one that has to stay
true.
