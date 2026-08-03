# syntax=docker/dockerfile:1
# Build stage: static musl binary (tests must pass before the build).
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev build-base cmake perl linux-headers
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
# brand assets: src/runbook.rs embeds the logo with include_bytes!
COPY docs/brand ./docs/brand
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo test --release && \
    cargo build --release && \
    cp target/release/hefesto /hefesto

# Artifact stage: `docker build --target artifact -o dist .` drops the
# bare binary into ./dist — nothing to run, just an export vehicle.
FROM scratch AS artifact
COPY --from=build /hefesto /hefesto
