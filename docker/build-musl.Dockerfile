# Dockerfile for the clanky build container: Alpine + Rust toolchain.
# Used by the `build-linux-musl` script.
FROM alpine:3.24

# Rust via apk (no rustup, so no ~/.rustup is needed). Alpine's rust package
# targets x86_64-unknown-linux-musl with dynamic linking by default, which is
# what we want. binutils provides `strip`.
# NOTE: Rust 1.85+ is required for edition 2024; bump the Alpine tag if it
# ships an older toolchain.
RUN apk update && apk upgrade && apk add rust cargo binutils bash

# User matching the host UID/GID (so the cargo caches and build output in the
# mounted project dir end up owned by the host user). The `run` command in
# build-linux-musl passes -u to run as this user.
ARG USER_ID
ARG GROUP_ID
RUN addgroup -g ${GROUP_ID} clanky 2>/dev/null || true && \
    GROUP_NAME=$(getent group ${GROUP_ID} | cut -d: -f1) && \
    adduser -u ${USER_ID} -D -h /home/clanky -G ${GROUP_NAME} clanky
