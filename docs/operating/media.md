Media
=====

What a local account uploads — an avatar, a header, a post's attachments —
is stored where and how Mastodon stores it, in the same columns and under
the same object keys, so that a Mastodon booted on the database and the
bucket serves the same files. Remote accounts' media is not downloaded; see
the recorded divergence `remote-account-images-not-downloaded`.


Avatars and headers
-------------------

`PATCH /api/v1/accounts/update_credentials` and `PATCH /api/v1/profile`
take an `avatar` and a `header` as `Account::Avatar` and `Account::Header`
do:

 -  Only `image/jpeg`, `image/png`, `image/gif` and `image/webp`, each under
    8 MB. Anything else is refused with Mastodon's messages, for example
    “Validation failed: Avatar content type is invalid, Avatar is invalid”,
    and nothing in the request is saved.
 -  Bytes that are no image of those types are refused too, as Mastodon
    refuses a file its geometry parser cannot read, rather than stored under
    the type the client claimed for them. What is stored is re-encoded, and
    its content type is the one read from the bytes.
 -  An avatar is cropped to 400×400, and a header shrunk to at most
    750,000 pixels (1500×500), turned upright and without its metadata.
 -  A GIF keeps its animation: ffmpeg shrinks it as Mastodon's
    `LazyThumbnail` does, never enlarging it, an avatar cropped square, at
    most 60 frames a second and 3,000 frames, in a 32-colour palette. Its
    first frame is kept as a PNG in the `static` style beside it
    (`accounts/avatars/…/static/<name>.png`), and `avatar_static` and
    `header_static` name that; any other image is its own static one.
 -  `avatar_description` and `header_description` are saved, up to 150
    characters each.

The body of these two requests may be up to 17 MB, room for both images.


Media uploads
-------------

`POST /api/v1/media` and `POST /api/v2/media` take a `file`, a
`description` (up to 10,000 characters), a `focus` (`"x,y"`) and, for audio
and video only, a `thumbnail`. `PUT /api/v1/media/:id` takes the last three.

### What is taken

Mastodon's `IMAGE_MIME_TYPES`, `VIDEO_MIME_TYPES` and `AUDIO_MIME_TYPES`,
the ones the instance API lists, and nothing else: “Validation failed: File
content type is invalid, File is invalid”. An image must be under 16 MB, and
a video or audio file under 99 MB; a GIF counts as an image. A thumbnail must
be one of the image types, under 16 MB, and is refused for anything but
audio and video (“Thumbnail must be blank”). The body of an upload may be up
to 116 MB, room for both at their limits, and of an update 17 MB.

Before anything is processed, as Mastodon's callbacks raise:

 -  an image over 33,177,600 pixels, or a GIF over 921,600, is refused
    (“1920x1080 GIF files are not supported”);
 -  a video over 3840×2160 pixels or 120 frames a second is refused
    (“150fps videos are not supported”), and one with no video stream too.

Bytes that are no image the build can read are refused (“File
Paperclip::Errors::NotIdentifiedByImageMagickError”) rather than stored
under the type the client claimed. A HEIC, HEIF or AVIF image fails
processing, as it does on Mastodon 4.7.2, answered with a 500 and “Error
processing thumbnail for uploaded media”, as is a video or audio file ffmpeg
cannot read.

### How it is processed

 -  An image is turned upright, shrunk to at most 8,294,400 pixels and
    re-encoded without its metadata; its small style is the same image at
    230,400 pixels, in the same format, and the blurhash is made from it.
 -  A still GIF stays an image and is kept as it came, for its small style
    too, as `GifTranscoder` leaves it. An animated GIF becomes a `gifv`: an
    MP4, with a PNG of its first frame as its small style.
 -  A video is transcoded to H.264 MP4, or only remuxed when it already is
    H.264 with AAC or no sound in 4:2:0, at `Transcoder`'s bitrate. Its small
    style is a PNG of its first frame, fitted within 640×640. A video with no
    sound is a `gifv`.
 -  Audio is transcoded to MP3. Its cover art, when it has one, becomes its
    thumbnail.
 -  A thumbnail, uploaded or cover art, is shrunk to 230,400 pixels and gives
    the attachment its blurhash, `meta.small`, and `meta.colors`: the
    background, foreground and accent colours of `ColorExtractor`.

`meta.original` is `image_geometry` for an image and `video_metadata` for
the rest — width, height, the frame rate as Ruby writes a rational
(`"30/1"`), duration and bitrate — read from the uploaded file for a video
and from the transcoded one for a gifv or audio, as Mastodon's memoised
reads leave them.

### When it is processed

`/api/v1/media` processes everything before it answers, 200, and records the
attachment `complete` (`processing` 2). `/api/v2/media` does the same for
images and GIFs, but leaves video and audio to the media queue
(`eunha.media_processing_jobs`): the original is kept as it came, under the
name it will keep, a video's small style is made at once, and the
attachment is recorded `queued` (0) and answered with 202 and no `url`. The
queue marks it `in_progress` (1) while it transcodes, then `complete`, or
`failed` (3) once its attempts are spent.

`GET` and `PUT /api/v1/media/:id` answer 206 while an attachment is not
complete, and 422 with “Error processing thumbnail for uploaded media” once
its processing has failed. A status cannot attach one that is not complete.
`DELETE /api/v1/media/:id` answers `{}`.

### Where the files are

As Paperclip names them: `media_attachments/files/<id>/original/<name>`,
where the name is sixteen hex digits and the type's extension, and the small
style at `…/small/` under the same name, or with `.png` for a GIF or a video.
A thumbnail is kept at `media_attachments/thumbnails/<id>/original/<name>`,
and `preview_url` is the thumbnail when there is one, else the small style;
audio has no small style. Deleting an attachment, or the post it belongs to,
deletes all of them.

Uploads eunha stored before it named files this way kept the small style at
`…/small/small.<ext>` beside an `original.<ext>`, and recorded that name as
the thumbnail. Eunha still finds those previews there; Mastodon looks for a
thumbnail under `thumbnails/` and finds none.
