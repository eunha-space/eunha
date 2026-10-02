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

At the time of writing there are thirty-one. They cover:

 -  integrity proofs on outgoing activities;
 -  the invite tree, and the two ways eunha's invite API goes beyond
    Mastodon's;
 -  what the update check asks about;
 -  when the local-keypair migration is recorded;
 -  what a mute silences;
 -  the account move API, and when a moved follower leaves the old account;
 -  how command-line accounts are created;
 -  three details of delivery failures;
 -  the admin custom emoji API;
 -  the private metrics listener;
 -  two details of link preview cards;
 -  the terms of service admin API and interstitial, and the configured
    terms and privacy policy eunha still reads;
 -  percent signs in policy texts;
 -  how a delivery from a domain this instance does not federate with is
    answered;
 -  the data export API.

Read the file rather than this paragraph: the file is the one that has to stay
true.
