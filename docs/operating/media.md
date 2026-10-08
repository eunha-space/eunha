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
