# The kit's three binaries for Linux, and the two images its checks run:
#
#   docker build -f deploy/Dockerfile --target kit -t fenec-kit .       # make drill
#   docker build -f deploy/Dockerfile --target systemd -t fenec-kit-systemd .  # make deploy-check
#
# `kit` is fenec-server, fenec-shard and fenec on Alpine -- a shell for the
# drill's checks, nothing else -- one container a process, as the drill
# runs a deployment's machines. `systemd` is the same binaries where the
# units go, under a real systemd (deploy/test/units.py boots it).
#
# Built as the repository's Dockerfile builds fenec-server: statically with
# musl, so the binaries run on any Linux, glibc or not, and through
# tools/image-cargo.sh on the same cache mount, so a build after it compiles
# only what it did not -- and every binary is held to the sources this build
# was given before it is copied out (the script says why: a branch's image
# once carried main's fenec-shard).

# ----------------------------------------------------------------- build
FROM rust:alpine AS build

RUN apk add --no-cache musl-dev rsync

WORKDIR /src
COPY . .

# rust-toolchain.toml also asks for the wasm32 target, which no binary
# here needs; the channel is "stable" already, so no pin is lost.
RUN rm -f rust-toolchain.toml

# /built, a path no build before the script copied from: BuildKit keyed
# those COPYs by binaries a stale target/ had made, and could hand a
# build of the same sources another tree's layer (the script says how).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=fenec-image-cargo,target=/work,sharing=locked \
    tools/image-cargo.sh /built --release -p fenec-server -p fenec-shard -p fenec-cli

# ------------------------------------------------------------------- kit
FROM alpine:3 AS kit

COPY --from=build /built/ /usr/local/bin/
# The drill's containers run as root inside: their data are named volumes
# the drill removes, and nothing here is a deployment's hardening -- that
# is the units' (deploy/systemd/), which `systemd` below holds.
CMD ["fenec-server", "--help"]

# --------------------------------------------------------------- systemd
# Debian, since systemd: python3 is the checks' HTTP client, procps their
# `ps`, rclone the bucket sync's own (its stand-in passed a run real rclone
# failed: a segment growing as it read it), node_exporter what reads the
# sync's success (a file it could not read passed a check made as root),
# acl the runbook's setfacl.
FROM python:3.13-slim AS systemd

RUN apt-get update && \
    apt-get install -y --no-install-recommends systemd systemd-sysv procps rclone \
        prometheus-node-exporter acl && \
    rm -rf /var/lib/apt/lists/* && \
    # Nothing a container boots needs these, and some wait for hardware.
    systemctl mask systemd-udevd.service systemd-udevd-kernel.socket \
        systemd-udevd-control.socket getty.target console-getty.service \
        systemd-logind.service systemd-modules-load.service

COPY --from=build /built/ /usr/local/bin/
# A machine's systemd makes / a shared mount as it boots; in a container it
# finds it private and leaves it so, and LoadCredential= -- which mounts a
# service's credentials in a namespace of its own and moves them into
# /run/credentials -- then hands the service an empty directory. Made shared
# before systemd starts, the credentials arrive as on a machine.
RUN printf '#!/bin/sh\nmount --make-rshared /\nexec /sbin/init\n' > /usr/local/sbin/boot && \
    chmod 0755 /usr/local/sbin/boot
STOPSIGNAL SIGRTMIN+3
CMD ["/usr/local/sbin/boot"]
