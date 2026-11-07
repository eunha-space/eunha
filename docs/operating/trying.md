Trying Eunha
============

`eunha try` creates an isolated trial with PostgreSQL, Redis, S3-compatible
media storage and the bundled web interface. Install Docker with Compose and
start its daemon first. Get the `eunha` binary from
[GitHub Releases](./downloads.md), or see [Building the CLI](#building-the-cli)
below.
The first run pulls container images and applies migrations explicitly before
starting Eunha. It creates an `owner` account and prints its random password;
keep that password to sign in. Supply an email with a real mail domain.
No SMTP configuration is needed to sign in with this confirmed account.

~~~~ sh
eunha try --email you@example.com
~~~~

Like [Fedify CLI], this starts an account-free SSH tunnel, discovers its public
HTTPS URL and configures the instance with it. It requires OpenSSH's `ssh`
executable, but no tunnel account or Node.js. The helper tries Serveo and
Pinggy, with a ten-second startup deadline for each provider and pinned SSH
host keys. To select one explicitly:

~~~~ sh
eunha try --email you@example.com --tunnel-service serveo.net
~~~~

The command stays attached. Ctrl-C closes the tunnel and stops containers,
retaining the configuration and Docker volumes. Each automatic trial gets a
fresh directory under `.eunha-try/`, because a new tunnel has a new hostname.
The command prints that directory and the exact stop command. An interrupted
setup retains its data too. These short-lived identities are for trying
federation; use a stable hostname below to keep an instance across sessions.

[Fedify CLI]: https://fedify.dev/cli


Building the CLI
----------------

Build the `eunha` executable from source with [mise]. Install Git and the native
build tools first. On Debian or Ubuntu:

~~~~ sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev cmake
~~~~

On macOS, install the Xcode command-line tools and, with Homebrew, OpenSSL,
pkg-config and CMake:

~~~~ sh
xcode-select --install
brew install openssl@3 pkg-config cmake
~~~~

Then clone the repository and build the CLI:

~~~~ sh
git clone https://github.com/eunha-space/eunha.git
cd eunha
mise trust
mise install rust
SQLX_OFFLINE=true mise exec -- cargo build --release --locked --bin eunha
./target/release/eunha try --email you@example.com
~~~~

`SQLX_OFFLINE=true` uses the checked-in SQL query metadata, so building does
not need PostgreSQL or Redis running. The first release build can take several
minutes. On macOS, if OpenSSL is not found, set
`OPENSSL_DIR="$(brew --prefix openssl@3)"` when running the build command.

The resulting binary is `target/release/eunha`. Run it by that path, or copy it
to a directory on your `PATH`, such as `~/.local/bin`, to use `eunha` directly.
The trial command requires Docker Compose and, for automatic public tunnels,
OpenSSH. It gets the web frontend and ffmpeg from its container image, so you
do not need to build the frontend or install ffmpeg on the host to try it.
Running the server directly instead requires the
[frontend build](../contributing/index.md#building-the-web-frontend) too.

[mise]: https://mise.jdx.dev/


Public HTTPS
------------

ActivityPub peers need to reach the instance over public HTTPS on port 443.
Choose the hostname before creating any accounts: their identities include
that hostname. Keep the tunnel running while using the instance.

For a signed-in Tailscale machine with MagicDNS enabled:

~~~~ sh
eunha try --tailscale --email you@example.com
~~~~

[Tailscale Funnel] is public. Tailscale Serve alone only reaches your tailnet
and cannot receive activities from ordinary public servers. The helper
discovers the machine's DNS name and starts a foreground Funnel after setup.
Approve Funnel access in your Tailscale account if prompted. Ctrl-C stops
Funnel and the trial's containers, retaining data. Use a machine whose HTTPS
port is available for this trial; Funnel uses that machine's forwarding
configuration.

For ngrok, start the tunnel, then use the HTTPS origin it prints:

~~~~ sh
ngrok http 3000
eunha try --url https://your-domain.ngrok.app --email you@example.com
~~~~

For localtunnel:

~~~~ sh
npx localtunnel --port 3000
eunha try --url https://your-name.loca.lt --email you@example.com
~~~~

Any other HTTPS reverse proxy works with `--url`. Forward to
`http://127.0.0.1:3000`. The proxy sets the configured public Host header
before forwarding to Eunha. The helper publishes
only this loopback listener; databases and object storage have no published
ports. Media is served through the same public tunnel.

Tunnel authentication, browser warning pages, provider limits or an unstable
hostname can prevent federation. Other servers cannot click through a tunnel
provider's consent screen or add bypass headers. Check that remote machines
can fetch `/api/v2/instance`, `/.well-known/webfinger` for your account and its
ActivityPub actor without such a screen. A tunnel does not guarantee that
other servers will accept a trial instance.

[Tailscale Funnel]: https://tailscale.com/docs/reference/tailscale-cli/funnel


Keeping and stopping a trial
----------------------------

For `--url` and `--tailscale`, the default `.eunha-try` directory holds
configuration and credentials; keep it private and outside version control.
Docker volumes retain the database, Redis and media. Reuse that directory to
resume without recreating the owner:

~~~~ sh
eunha try --url https://your-domain.ngrok.app
eunha try --stop
~~~~

Stopping removes containers and the private network, retaining the directory
and volumes. For an automatic trial, supply the printed `--directory PATH`
when stopping it from another terminal. Stop your separately running tunnel
too. Failed setup also retains data; rerun with `--email` if owner creation has
not finished. If a password was lost, use the
[account password reset](./first-account.md) command through
`docker compose -f .eunha-try/compose.json run --rm web ./eunha accounts modify owner --reset-password`.

Use `--directory PATH` for another trial, `--port PORT` for another loopback
port (and forward that port), and `--image IMAGE` to choose the initial Eunha
image. The default image is `ghcr.io/eunha-space/eunha:main`, which currently
requires an amd64-capable container runtime. If that published image cannot
be pulled, the helper builds the public repository's `main` using Docker and
tags it `eunha:trial`. This first build can take several minutes and needs
internet access. Later trials reuse that local fallback image; remove it or
build a new one when you want to update it. An explicit `--image` must already
exist locally or be pullable; it never falls back to a different image.
Options for image and port apply
when the directory is first created. A supplied URL must match the stored
origin on later runs; a new tunnel hostname requires a new trial directory.
Repeat `--tailscale` when resuming a Tailscale trial. Automatic SSH tunnels
cannot resume a previous identity after the provider releases its hostname.
This helper is for evaluation; see [container images](./containers.md) for
operating an instance long term.
