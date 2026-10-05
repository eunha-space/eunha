Container images
================

The **Container** GitHub Actions workflow builds the repository's `Dockerfile`
on pull requests, pushes to `main`, `v*` tags and manual runs. Pull requests
build without publishing. Other runs publish to `ghcr.io/<owner>/<repository>`;
for this repository, `ghcr.io/eunha-space/eunha`. The workflow uses the built-in
`GITHUB_TOKEN` with `packages: write`, so no registry secret is needed. If a
package already exists, grant this repository Actions access to it. To allow
anonymous pulls, make the package public in its GitHub package settings.

Images currently target `linux/amd64`. The image includes the Rust server,
built web frontend, migrations, CA certificates and ffmpeg. Compilation uses
the committed Rust and frontend lockfiles and `.sqlx` cache; it does not need a
running database. BuildKit caches intermediate stages in GitHub Actions.


Tags
----

 -  `main` and `latest`: the most recent successful build of `main`.
 -  `v*`: the full Git tag, for example `v0.2.0`.
 -  `sha-<full-commit-sha>`: the source revision for each published build.
 -  Manual runs on other branches publish a normalized branch tag and SHA tag,
    leaving `latest` unchanged.

`latest` tracks development on `main`, rather than the newest release. For a
repeatable deployment, pin the image digest shown in the workflow's build
summary. Publishing an image does not deploy it or apply migrations, and the
container workflow runs independently of the other CI checks.


Running the image
-----------------

Provide the instance's configuration and access to its PostgreSQL, Redis and
media storage. The image's working directory is `/app`; mount configuration as
`/app/config.toml`. Set `bind_address = "0.0.0.0:3000"` so the published port
can reach the server. Database and Redis hosts must be reachable from the
container; `localhost` there refers to the container itself.

Apply [migrations](./migrations.md) explicitly before starting the server:

~~~~ sh
docker pull ghcr.io/eunha-space/eunha:main
docker run --rm \
  -v "$PWD/config.toml:/app/config.toml:ro" \
  ghcr.io/eunha-space/eunha:main ./eunha migrate
docker run -d --name eunha -p 3000:3000 \
  -v "$PWD/config.toml:/app/config.toml:ro" \
  ghcr.io/eunha-space/eunha:main
~~~~

The Dockerfile uses `CMD`, so when supplying a command include `./eunha` before
its arguments. For [several instances](./instances.md), mount the tenants
directory and run `./eunha --tenants /app/tenants migrate`, then
`./eunha --tenants /app/tenants` to serve them. Mount persistent local media
storage too when using it; data kept only in the container is lost on removal.

To build locally from the repository root with a running Docker daemon:

~~~~ sh
mise run container:build
~~~~
