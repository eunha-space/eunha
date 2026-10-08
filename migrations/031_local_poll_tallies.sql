-- Local polls keep their tallies as Mastodon keeps them: `cached_tallies`
-- one count per option, raised with each vote, `votes_count` their sum, and
-- `voters_count` raised for each new voter. Eunha once counted `poll_votes`
-- when serving a poll instead, and left `cached_tallies` empty and
-- `voters_count` unset on single-choice polls; those polls get the counts
-- their votes give. A poll Mastodon made already has one tally per option and
-- is left as it is.
UPDATE polls p
SET cached_tallies = t.tallies,
    votes_count = t.votes,
    voters_count = t.voters
FROM (
    SELECT p2.id,
           ARRAY(
               SELECT COUNT(v.id)::bigint
               FROM generate_series(0, cardinality(p2.options) - 1) AS o(n)
               LEFT JOIN poll_votes v ON v.poll_id = p2.id AND v.choice = o.n
               GROUP BY o.n
               ORDER BY o.n
           ) AS tallies,
           (SELECT COUNT(*) FROM poll_votes v WHERE v.poll_id = p2.id) AS votes,
           (SELECT COUNT(DISTINCT v.account_id) FROM poll_votes v WHERE v.poll_id = p2.id) AS voters
    FROM polls p2
    JOIN accounts a ON a.id = p2.account_id
    WHERE a.domain IS NULL
      AND cardinality(p2.cached_tallies) <> cardinality(p2.options)
) t
WHERE p.id = t.id;
