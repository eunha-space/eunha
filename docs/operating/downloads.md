Downloading Eunha
=================

Download a ready-made executable from [GitHub Releases]. The **Downloads**
workflow builds macOS and Linux packages for both Apple Silicon/ARM64 and
Intel/AMD64. Each archive includes `eunha`, the web frontend, the license and a
`REVISION` file identifying its source commit.

| Platform             | Archive                    |
| -------------------- | -------------------------- |
| macOS, Apple Silicon | `eunha-macos-arm64.tar.gz` |
| macOS, Intel         | `eunha-macos-amd64.tar.gz` |
| Linux, Intel/AMD     | `eunha-linux-amd64.tar.gz` |
| Linux, ARM64         | `eunha-linux-arm64.tar.gz` |

The macOS packages are built and tested on macOS 15. Linux packages are built
on Ubuntu 22.04 and require glibc 2.35 or newer; Alpine's musl environment is
not supported by these downloads. OpenSSL is linked statically, so a separate
OpenSSL installation is not needed to run them. Windows users can use Linux
through WSL2 with Docker integration.

[GitHub Releases]: https://github.com/eunha-space/eunha/releases


Download and try
----------------

Tagged releases get versioned downloads. The `nightly` prerelease tracks
successful builds of `main`; it is a development build, not a stable release.
The first run of the Downloads workflow must finish before its assets appear.

For example, on an Apple Silicon Mac:

~~~~ sh
curl -fLO https://github.com/eunha-space/eunha/releases/download/nightly/eunha-macos-arm64.tar.gz
curl -fLO https://github.com/eunha-space/eunha/releases/download/nightly/SHA256SUMS
awk '$2 == "eunha-macos-arm64.tar.gz"' SHA256SUMS | shasum -a 256 --check
mkdir eunha
tar -xzf eunha-macos-arm64.tar.gz -C eunha
cd eunha
./eunha try --email you@example.com
~~~~

Choose the matching filename from the table for your system. On Linux, use
`sha256sum --ignore-missing --check SHA256SUMS` to check the downloaded archive.
Stop if checksum verification fails. Nightly assets can change while you are
downloading them; download the archive and checksums again if they do not match.

The [trial command](./trying.md) needs Docker Compose and OpenSSH, but no Rust,
Node.js, PostgreSQL or Redis installed on the host. It supplies those services
and the frontend through containers. For a server running directly, keep the
extracted `frontend` directory beside `eunha`, run from that directory, and
install ffmpeg for media processing.

To build the executable yourself, see
[Building the CLI](./trying.md#building-the-cli).
