The job queue
=============

Mastodon hands its background work to Sidekiq: a mail to send in two
minutes, a remote account to refresh, a provider to tell about a new post.
Eunha keeps the same work in a table of its own, `eunha.jobs`, so that a job
queued by one process is run by whichever process gets to it first, and a
restart loses none of it.


What a job is
-------------

A job is a Mastodon worker class and its arguments, stored under the
class's name: `EmailDistributionWorker`, `FetchReplyWorker`,
`ActionMailer::MailDeliveryJob`. Each worker keeps its `sidekiq_options`:

 -  the queue it waits in: `default`, `push`, `ingress`, `mailers`, `pull`,
    `scheduler` or `fasp`;
 -  how many times it is retried, and whether it is kept in the dead set
    once it has run out of retries;
 -  its sidekiq-unique-jobs lock, if it has one.

A job queued for later (`perform_in`, `perform_at`) waits until it is due.


How jobs are run
----------------

Every instance runs `[workers] job_workers` job loops, one by default, and
each runs up to `job_concurrency` jobs at once, five by default, as
Sidekiq's `concurrency` does:

~~~~ toml
[workers]
job_workers = 1
job_concurrency = 5
~~~~

A loop claims due jobs with `FOR UPDATE SKIP LOCKED`, so loops in one
process and loops in other processes sharing the database never take the
same job. Each claim looks at the queues in Sidekiq's weighted order, the
weights of Mastodon's `config/sidekiq.yml` (`default` 8, `push` 6,
`ingress` 4, `mailers` 2, the rest 1): a busy queue goes first more often,
but never starves the others.

A job queued by the process that runs it wakes its loop at once. One queued
by another process — `eunha accounts`, say — and a retry that has come due
are found by polling, at most `[workers] queue_idle_poll_seconds` apart.

A claimed job is held by a lease of five minutes, which its loop renews
every minute while the job runs. A job whose process died is taken up again
once its lease lapses. A stopping instance gives its running jobs fifteen
seconds to finish, as Sidekiq's shutdown timeout does, and then hands the
rest back to the queue.


Retries and the dead set
------------------------

A job that fails is retried as Sidekiq retries it: after its `n`th retry
(counting from nought) it waits `n⁴ + 15` seconds, or what its worker's
`sidekiq_retry_in` says, plus up to `9 × (n + 1)` seconds of jitter. Sidekiq
retries a job 25 times unless its worker says otherwise.

A job that has failed as many times as its worker allows is dead. It is kept,
with `dead_at` and its last error, unless its worker says `dead: false`, in
which case it is deleted; a worker with `retry: false` is deleted after its
first failure. Like Sidekiq's dead set, dead jobs are kept for six months, and
no more than the newest ten thousand.

~~~~ sql
SELECT kind, attempts, last_error, dead_at FROM eunha.jobs
WHERE dead_at IS NOT NULL ORDER BY dead_at DESC;
~~~~


Unique jobs
-----------

A worker with a sidekiq-unique-jobs lock is queued at most once for the same
arguments. `until_executed` holds the lock from queueing until the job has run,
or failed for good; `until_executing` until it starts. Each lock has its
worker's `lock_ttl`, counted from when the job is due, or Mastodon's default
of fifty days; once it has passed, a new job takes the lock over.


Mail
----

Mail Mastodon sends with `deliver_later` is queued as
`ActionMailer::MailDeliveryJob` on the `mailers` queue, rendered when it is
queued. A notification email is queued two minutes ahead and rendered when
it is sent, so that nothing is mailed about a notification or post that has
gone in the meantime, or to a member who can no longer sign in. The welcome
mail a new user is sent goes an hour after the account is ready, and is
rendered then, so that its checklist shows what the user has done since.


Deliveries
----------

ActivityPub deliveries have a queue of their own, `eunha.ojak_queue`, run by
the `[workers] delivery_workers` loops, with `ActivityPub::DeliveryWorker`'s
seventeen attempts and backoff. What `ActivityPub::Forwarder` passes on goes
as `ActivityPub::LowPriorityDeliveryWorker` sends it, nine attempts on a lane
taken only when nothing else is due, as Mastodon's `pull` queue is. The
`Follow` of a follower moving to a remote account goes as
`ActivityPub::MigratedFollowDeliveryWorker` sends it: once it has been
delivered, or refused for good, the old account is unfollowed by a job queued
here.

Each inbox's circuit breaker is Mastodon's Stoplight: ten failures in a row
open it, and its deliveries are held, each counted as a failed attempt and
retried, for a minute. Then one delivery at a time is let through as a probe,
the others held while it is under way; a probe that succeeds closes the
breaker, and one that fails opens it for another minute. A success that is not
a probe only starts the count of failures again. The breaker is kept in Redis
under the instance's prefix (`stoplight:<inbox>:failures`,
`stoplight:<inbox>:recovery_after` and the probe's lock,
`stoplight:<inbox>:probe`), so every process delivering for the instance counts
the same failures and lets one probe through between them, as Mastodon's
Stoplights are shared across Sidekiq processes.


Seeing whether the queue moves
------------------------------

`Eunha::ProbeWorker` does nothing but count its runs, under
`jobs:probe:<name>` in Redis, failing as many of its first runs as it is
asked to. The tests queue it to watch retries happen, and so can an operator
who wants to know whether an instance is running its jobs:

~~~~ sql
INSERT INTO eunha.jobs (queue, kind, args, max_retries, keep_dead)
VALUES ('default', 'Eunha::ProbeWorker', '{"name": "check"}', 2, true);
~~~~
