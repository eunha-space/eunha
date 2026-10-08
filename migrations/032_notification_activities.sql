-- Notifications point at the activity Mastodon's callers hand to
-- `LocalNotificationWorker`. Eunha once pointed a favourite's at the post
-- favourited rather than the `Favourite`, a boost's at the post boosted rather
-- than the boost, and a poll's at the poll's post rather than the `Poll`, and
-- could point a follow's at the recipient's follow of the sender rather than
-- the sender's of the recipient. Each is pointed where Mastodon points it when
-- that activity still exists; one whose activity is gone is left as it is.

-- `favourite`: the sender's `Favourite` of the post.
UPDATE notifications n
SET activity_type = 'Favourite',
    activity_id = f.id
FROM favourites f
WHERE n."type" = 'favourite'
  AND n.activity_type = 'Status'
  AND f.account_id = n.from_account_id
  AND f.status_id = n.activity_id;

-- `reblog`: the sender's boost of the post. A Mastodon-written one already
-- points at a boost, which has a `reblog_of_id`.
UPDATE notifications n
SET activity_id = (
    SELECT s.id FROM statuses s
    WHERE s.account_id = n.from_account_id
      AND s.reblog_of_id = n.activity_id
    ORDER BY s.deleted_at IS NULL DESC, s.id DESC
    LIMIT 1
)
FROM statuses original
WHERE n."type" = 'reblog'
  AND n.activity_type = 'Status'
  AND original.id = n.activity_id
  AND original.reblog_of_id IS NULL
  AND EXISTS (
      SELECT 1 FROM statuses s
      WHERE s.account_id = n.from_account_id
        AND s.reblog_of_id = n.activity_id
  );

-- `poll`: the post's `Poll`.
UPDATE notifications n
SET activity_type = 'Poll',
    activity_id = p.id
FROM polls p
WHERE n."type" = 'poll'
  AND n.activity_type = 'Status'
  AND p.status_id = n.activity_id;

-- `follow`: the sender's `Follow` of the recipient.
UPDATE notifications n
SET activity_id = mine.id
FROM follows wrong, follows mine
WHERE n."type" = 'follow'
  AND n.activity_type = 'Follow'
  AND wrong.id = n.activity_id
  AND wrong.account_id <> n.from_account_id
  AND mine.account_id = n.from_account_id
  AND mine.target_account_id = n.account_id;
