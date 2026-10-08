-- `media_attachments.type` is Mastodon's enum: image 0, gifv 1, video 2,
-- unknown 3, audio 4. Eunha once wrote audio as 3 and unknown as 4, so an
-- audio attachment it stored read as unknown to Mastodon, and an unknown one
-- as audio. Both numberings use the same integers, so the rows are told apart
-- by what Mastodon never writes:
--
--  -  Mastodon's unknown rows are remote attachments it has not downloaded,
--     with no content type; eunha recorded the remote one's, so a 3 whose
--     content type is audio is eunha's audio. So is an upload of eunha's still
--     waiting in its transcoding queue as audio, which has none yet.
--  -  Mastodon's audio is what it transcoded to MP3, or a copy of one since
--     removed, which has no content type but keeps its `meta.original`. A 4
--     with any other content type is eunha's unknown, as is a remote one with
--     no file, no content type and no `meta.original`, which is how eunha
--     recorded an attachment from a domain blocked with `reject_media`.
--
-- What is left — a 4 with no content type that has a `meta.original`, which
-- eunha wrote only for a remote attachment that named no media type and whose
-- address had no extension — is left as it is.

UPDATE media_attachments m
SET "type" = 4
WHERE m."type" = 3
  AND (m.file_content_type LIKE 'audio/%'
       OR EXISTS (SELECT 1 FROM eunha.media_processing_jobs j
                  WHERE j.media_id = m.id AND j.media_type = 'audio'));

-- `video/x-ms-asf` is in Mastodon's `AUDIO_MIME_TYPES`, so an audio upload
-- still waiting to be processed can carry it; eunha classified it as video.
UPDATE media_attachments
SET "type" = 3
WHERE "type" = 4
  AND (
    (file_content_type IS NOT NULL
     AND file_content_type NOT LIKE 'audio/%'
     AND file_content_type <> 'video/x-ms-asf')
    OR (file_content_type IS NULL
        AND remote_url <> ''
        AND file_file_name IS NULL
        AND (file_meta IS NULL OR file_meta -> 'original' IS NULL))
  );
