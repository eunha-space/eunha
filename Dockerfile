# ── Stage 1: Build the web frontend (SPA) ───────────────────────────────────
FROM node:24-alpine AS frontend-builder
# The build never runs Playwright, so skip its browser download during install.
ENV PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1
RUN corepack enable
WORKDIR /frontend
# `pnpm-workspace.yaml` as well: it carries `allowBuilds`, and pnpm 10+ refuses
# to run a dependency's build script that nothing has allowed — so without it
# the install fails on esbuild and this image cannot be built at all. It works
# outside Docker because the file is right there; only the copy list hid it.
COPY frontend/package.json frontend/pnpm-lock.yaml frontend/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY frontend/ .
RUN pnpm run build

# ── Stage 2a: Install cargo-chef ────────────────────────────────────────────
FROM rust:1-slim-bookworm AS chef
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
WORKDIR /app

# ── Stage 2b: Generate dependency recipe ────────────────────────────────────
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 2c: Cache dependency compilation ──────────────────────────────────
FROM chef AS cacher
COPY --from=planner /app/recipe.json recipe.json
# Dependencies don't contain sqlx queries so offline mode is fine here.
ENV SQLX_OFFLINE=true
RUN cargo chef cook --release --locked --recipe-path recipe.json

# ── Stage 2d: Build application ─────────────────────────────────────────────
FROM chef AS rust-builder
# Use the committed .sqlx cache so the build doesn't depend on a live database.
ENV SQLX_OFFLINE=true
COPY --from=cacher /app/target target
COPY --from=cacher $CARGO_HOME $CARGO_HOME
COPY . .
RUN cargo build --release --locked --bin eunha

# ── Stage 4: Runtime ────────────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates ffmpeg && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=rust-builder /app/target/release/eunha .
COPY --from=frontend-builder /frontend/dist/ frontend/dist/
COPY migrations/ migrations/
EXPOSE 3000
CMD ["./eunha"]
