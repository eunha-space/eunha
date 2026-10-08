-- A post's edit history holds every version, the current one included, as
-- Mastodon writes it: each edit snapshots the version it made, stamped with
-- the post's new `edited_at`. Eunha once wrote only the version each edit
-- replaced, stamped with that version's own time, and added the current
-- version when serving the history, so the last row of a local post eunha
-- edited is the version before the current one and is older than the post's
-- `edited_at`. Each such post gets the current version as its last row, by
-- its author, stamped with its `edited_at`. A history Mastodon wrote ends
-- with a row stamped with the post's `edited_at`, and is left as it is; so
-- are remote posts, whose history eunha never wrote.
INSERT INTO status_edits
    (status_id, account_id, text, spoiler_text, sensitive,
     ordered_media_attachment_ids, media_descriptions, poll_options, quote_id,
     created_at, updated_at)
SELECT s.id, s.account_id, s.text, s.spoiler_text, s.sensitive,
       COALESCE(s.ordered_media_attachment_ids,
                ARRAY(SELECT m.id FROM media_attachments m
                      WHERE m.status_id = s.id ORDER BY m.id)),
       CASE WHEN s.ordered_media_attachment_ids IS NULL THEN
           ARRAY(SELECT m.description FROM media_attachments m
                 WHERE m.status_id = s.id ORDER BY m.id LIMIT 4)
       ELSE
           ARRAY(SELECT m.description
                 FROM unnest(s.ordered_media_attachment_ids) WITH ORDINALITY AS o(id, n)
                 JOIN media_attachments m ON m.id = o.id AND m.status_id = s.id
                 ORDER BY o.n LIMIT 4)
       END,
       (SELECT p.options FROM polls p WHERE p.id = s.poll_id),
       (SELECT q.id FROM quotes q WHERE q.status_id = s.id LIMIT 1),
       s.edited_at, now()
FROM statuses s
JOIN accounts a ON a.id = s.account_id
JOIN LATERAL (
    SELECT e.created_at FROM status_edits e
    WHERE e.status_id = s.id
    ORDER BY e.id DESC
    LIMIT 1
) last_edit ON true
WHERE a.domain IS NULL
  AND s.edited_at IS NOT NULL
  AND last_edit.created_at < s.edited_at;
