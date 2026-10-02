Translation
===========

An instance with a translation service configured offers to translate
posts, as Mastodon's `TranslationService` and `TranslateStatusService` do.
The service is DeepL or a LibreTranslate server, named in the instance's
`[instance.translation]` table (see
[several instances in one process](./instances.md#translation)). With none,
`configuration.translation.enabled` in `/api/v2/instance` is `false`,
`/api/v1/instance/translation_languages` is `{}`, and asking for a
translation is a 404.


Languages
---------

`/api/v1/instance/translation_languages` maps each language the service
translates from to those it translates into, and `und` to what a post with
no language translates into. The list is asked of the service once and kept
for a week under the Redis key `translation_service/languages`, so a service
that learns a language is listed within the week.

DeepL is asked for its source and target languages. English and Portuguese
are always targets, since DeepL lists only their regional forms. A
LibreTranslate server is asked for `/languages`, and `und` is every target
it lists.


Translating a post
------------------

`POST /api/v1/statuses/:id/translate` needs a signed-in user and the
`read:statuses` scope, and answers for a post the user can see. It
translates a public or unlisted post, its content warning, its poll options
and its media descriptions, in one request to the service:

 -  The target is the `lang` parameter when it names a language the
    interface is offered in, then the user's locale, then the
    `Accept-Language` header, then English. When the post's language does
    not translate into that locale, its language alone is tried: `pt-BR`
    becomes `pt`.
 -  A followers-only or direct post, or one whose language does not
    translate into the target, is refused with a 403.
 -  Custom emoji shortcodes go to the service inside
    `<span translate="no">`, so they come back as they went.
 -  The translated content is sanitised as Mastodon sanitises remote posts.

The answer is kept for a day, keyed by the post's language, the target and
a digest of the texts, so that an edit is translated afresh and the same
text in another post is not translated twice.

When the service refuses, the answer is a 503: one that is rate limiting
the instance (429) or out of quota (DeepL's 456, LibreTranslate's 403) says
so, and any other answer, or none, says the service is unavailable.

Requests to LibreTranslate are not held to the guard that keeps fetches
from private addresses, because the operator names the server, and it
usually runs next to the instance. DeepL is reached at `api-free.deepl.com`
or `api.deepl.com`, or at `deepl_endpoint` when that is set.


The web client
--------------

A public or unlisted post with text offers *Translate* under its content
when the service translates its language into the browser's. The
translation replaces the content, content warning, poll options and media
descriptions until *Show original* puts them back.
