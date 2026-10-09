-- Mastodon stores a local status's `uri` once it is created
-- (`Status#store_uri`), and a boost's is its `Announce`'s id:
-- `<actor>/statuses/<id>/activity`, with the actor `/ap/users/<id>` for an
-- account of the `numeric_ap_id` scheme and `/users/<username>` otherwise.
-- Eunha left a local boost's `uri` NULL. Each such boost gets the URI Mastodon
-- would have stored. The database does not name the instance's domain, so it
-- is taken from the local post nearest the boost by id: the domain the
-- instance had when the boost was made, if it has ever moved. A boost on an
-- instance with no local post that has a URI is left as it is; eunha names it
-- from the account all the same.
UPDATE statuses AS s
SET uri = boost.uri
FROM (
    SELECT b.id,
           near.origin
               || CASE WHEN a.id_scheme = 1 THEN '/ap/users/' || a.id::text
                       ELSE '/users/' || a.username END
               || '/statuses/' || b.id::text || '/activity' AS uri
    FROM statuses AS b
    JOIN accounts AS a ON a.id = b.account_id
    CROSS JOIN LATERAL (
        SELECT substring(n.uri FROM '^https?://[^/]+') AS origin
        FROM (
            (SELECT p.uri, b.id - p.id AS distance
             FROM statuses AS p
             WHERE p.id < b.id AND p.local AND p.reblog_of_id IS NULL
               AND p.uri IS NOT NULL
             ORDER BY p.id DESC LIMIT 1)
            UNION ALL
            (SELECT p.uri, p.id - b.id AS distance
             FROM statuses AS p
             WHERE p.id > b.id AND p.local AND p.reblog_of_id IS NULL
               AND p.uri IS NOT NULL
             ORDER BY p.id LIMIT 1)
        ) AS n
        ORDER BY n.distance
        LIMIT 1
    ) AS near
    WHERE a.domain IS NULL
      AND b.reblog_of_id IS NOT NULL
      AND b.uri IS NULL
      AND near.origin IS NOT NULL
) AS boost
WHERE s.id = boost.id;
