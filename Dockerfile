FROM rust:1-slim-bookworm AS builder

# cmake is needed by aws-lc-sys, the crypto backend behind reqwest 0.13's rustls
# default. gcc/libc6-dev already ship in rust:slim. Nothing here needs OpenSSL
# anymore; rustls is pure Rust and reads trust roots from /etc/ssl at runtime.
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Dependency-only layer, keyed on Cargo.toml/Cargo.lock alone: editing src/ no
# longer rebuilds the whole tree (aws-lc-sys and chromiumoxide are slow). The
# stub main.rs just gives cargo something to link and is replaced below.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

# COPY preserves the build context's mtimes, which can be older than the stub
# build above, so touch the crate root to force cargo to rebuild it. The binary
# is a single compilation unit, so this invalidates every module in src/.
COPY src ./src
COPY config.example.toml ./
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    chromium \
    ca-certificates \
    fonts-liberation \
    tzdata \
    && rm -rf /var/lib/apt/lists/*

ENV CHROME_EXECUTABLE=/usr/bin/chromium
# schedule.cron is evaluated in this timezone; defaults to UTC unless overridden
# (e.g. `-e TZ=America/Denver` on docker run, or `env: [{name: TZ, value: ...}]` in k8s).
ENV TZ=UTC

# Run unprivileged so a compromised renderer can't write to the binary or the
# config mount. Chromium still needs --no-sandbox here (its setuid sandbox wants
# privileges Docker's default seccomp profile denies), so this reduces blast
# radius rather than restoring the sandbox.
#
# UID/GID are build args so a bind-mounted profile dir can be matched to the host
# user that owns it: `docker build --build-arg UID=$(id -u) --build-arg GID=$(id -g) .`
ARG UID=1000
ARG GID=1000
RUN groupadd --gid "$GID" app \
    && useradd --uid "$UID" --gid "$GID" --create-home --shell /usr/sbin/nologin app

WORKDIR /app

COPY --from=builder /build/target/release/projectionlab-sync-server /app/projectionlab-sync-server

# Pre-create the profile dir owned by `app`: a *fresh* named volume mounted here
# inherits ownership from the image. A volume that already exists from an older
# root-built image will not; see the README for the one-time chown.
RUN mkdir -p /app/.pl-profile && chown -R "$UID:$GID" /app

# config.toml and the browser profile dir are expected to be bind-mounted or
# volume-mounted at runtime; see README for the docker run example.
ENV SYNC_CONFIG=/app/config.toml

USER app

ENTRYPOINT ["/app/projectionlab-sync-server"]
