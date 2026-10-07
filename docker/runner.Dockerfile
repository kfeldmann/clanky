# Dockerfile for the clanky runner image: Alpine + the compiled release
# binary, with a user matching the host UID/GID. Built by the `build-runner`
# script with `docker/` as the build context (so `clanky` below is the
# release binary staged there by build-runner).
FROM alpine:3.24

ARG EXTRA_PACKAGES=""
ARG EXTRA_COMMANDS=""

RUN apk update && apk upgrade && apk add libgcc curl bash vim

# Optional additional packages (populated by build-runner from the
# `extra-packages` file).
RUN [ -z "$EXTRA_PACKAGES" ] || apk add ${EXTRA_PACKAGES}

# Optional extra commands, e.g. `pip3 install --break-system-packages ...`
# (populated by build-runner from the `extra-commands` file).
RUN [ -z "$EXTRA_COMMANDS" ] || $EXTRA_COMMANDS

# User matching the host UID/GID (so we can read/write mounted volumes).
# On macOS the group id might be a low number and conflict with an existing
# group, hence the `|| true`.
ARG USER_ID
ARG GROUP_ID
RUN addgroup -g ${GROUP_ID} clanky 2>/dev/null || true && \
    GROUP_NAME=$(getent group ${GROUP_ID} | cut -d: -f1) && \
    adduser -u ${USER_ID} -D -h /home/clanky -G ${GROUP_NAME} clanky && \
    mkdir -p /home/clanky/.clanky && chown ${USER_ID}:${GROUP_ID} /home/clanky/.clanky
ENV HOME=/home/clanky
ENV TERM=xterm-256color
ENV EDITOR=/usr/bin/vim

COPY --chown=${USER_ID}:${GROUP_ID} vimrc /home/clanky/.vimrc
COPY clanky /usr/local/bin/clanky
WORKDIR /work
ENTRYPOINT ["/usr/local/bin/clanky"]

USER clanky
