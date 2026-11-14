Hosted instances
================

An instance a hosting service runs, as eunha space runs its customers', has
administrators who look after its members and its moderation, while the host
looks after its software, its machines and how long it lives. The host says so
in the instance's configuration:

~~~~ toml
[instance]
hosted = true
~~~~

The administrators are then shown nothing about the server itself, which they
could not act on:

 -  No update check runs, even if `software_update_url` is set, so no update
    or end-of-support notice is recorded, mailed or shown on the dashboard
    (see [Update notices](./update-notices.md)).
 -  The *Software updates* and *Server health* pages are gone: their API
    endpoints answer 404, and the web client leaves both out of the admin
    navigation. The scheduled health checks are not run.
 -  The dashboard warns only of what the administrators can fix, that the
    instance has no server rules yet; not of the database schema, the media
    bucket's privacy or a pending release. Its *Software* and *Space usage*
    cards, the PostgreSQL, Redis, Elasticsearch and FFmpeg versions and the
    sizes of the database, Redis and media, are left out.
 -  A `self_destruct` value is ignored, and `eunha self-destruct` refuses the
    instance: closing it is the host's to do, with its own deprovisioning
    (see [Self-destruct](./self-destruct.md)).

Everything else is as on any instance: the server settings, rules, roles,
federation, moderation and the rest of the admin pages.


Crediting the host
------------------

A host can be credited on the instance's About page, under its title, as
*Hosted by* its name, linking to its address:

~~~~ toml
[instance.hosting_provider]
name = "eunha space"
url = "https://eunha.space/"
~~~~

The credit is independent of `hosted`, and either can be set alone. The `url`
is optional, and is shown only when it is an `https://` or `http://` address.
The web client reads the credit from `GET /api/eunha/v1/instance`, which
answers `{"hosting_provider": {"name": …, "url": …}}`, or `null` for no credit,
to anyone, as `/api/v2/instance` does; Mastodon's own instance entity is left
as it is.
