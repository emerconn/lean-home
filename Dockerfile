# syntax=docker/dockerfile:1
# lean-home in a FROM scratch image: one static binary, the HA frontend, and zoneinfo.
#   docker build --build-arg HA_FRONTEND=<pin from compat/versions.env> -t lean-home .
#   docker run -v /path/to/ha-config:/config -p 8123:8123 lean-home
# The config dir must be one HA has onboarded (lean-home has no onboarding yet; see README).

# Static musl build: Alpine's Rust targets musl. The tag is rust-toolchain.toml's channel (a test in
# main.rs checks they agree; Renovate leaves it alone).
FROM rust:1.100.0-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/lean-home/Cargo.toml crates/lean-home/Cargo.toml
# Pre-build dependencies in their own layer so code changes only recompile lean-home
# (GHA layer caching persists image layers, not BuildKit cache mounts).
RUN mkdir -p crates/lean-home/src \
 && echo 'fn main() {}' > crates/lean-home/src/main.rs \
 && cargo build --release --locked \
 && rm -rf crates/lean-home/src target/release/.fingerprint/lean-home-* target/release/deps/lean_home* target/release/lean-home*
COPY crates ./crates
RUN cargo build --release --locked --offline \
 && strip -o /lean-home target/release/lean-home \
 && rm -rf target

# The frontend from the pinned home-assistant-frontend wheel (pure data, no Python needed at run
# time). Dropped: the source maps and the ES5 build for old browsers (both unused by current
# browsers; trimming the rest is on the roadmap).
# HA's Python: newer frontend releases require 3.14, as HA 2026.11 does.
FROM python:3.14.10-alpine3.24 AS frontend
ARG HA_FRONTEND
RUN test -n "$HA_FRONTEND" || { echo "build with --build-arg HA_FRONTEND=<compat/versions.env>"; exit 1; }
RUN pip install --no-cache-dir --no-deps --root-user-action=ignore --disable-pip-version-check --target /fe "home-assistant-frontend==$HA_FRONTEND" \
 && cd /fe/hass_frontend \
 && rm -rf frontend_es5 \
 && find . -name '*.map' -delete -o -name '*.map.gz' -delete -o -name '*.map.br' -delete

# The system zone database, as Python's zoneinfo reads it: lean-home loads only the configured
# zone, so the rest costs disk, not memory.
FROM alpine:3.24.4 AS zoneinfo
RUN apk add --no-cache tzdata

FROM scratch
COPY --from=build /lean-home /lean-home
COPY --from=frontend /fe/hass_frontend /hass_frontend
COPY --from=zoneinfo /usr/share/zoneinfo /usr/share/zoneinfo
# Runs as root, like HA's own image, so it can write a config dir HA created (.storage, the
# editors' YAML files).
EXPOSE 8123
ENTRYPOINT ["/lean-home", "--config", "/config", "--frontend", "/hass_frontend"]
