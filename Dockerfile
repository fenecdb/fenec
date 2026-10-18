# fenecdb -- the server (`fenec-server`), over HTTP
#
#   docker build -t fenecdb .
#   docker run -d --name fenecdb -p 127.0.0.1:8080:8080 -v fenecdata:/data \
#     -e FENEC_HTTP_TOKEN=secret --memory 1g fenecdb
#   curl -H 'Authorization: Bearer secret' -d '{"query": "collections"}' \
#     http://127.0.0.1:8080/query
#
# Two stages: build statically with musl, then put the result into an *empty*
# image. fenec-server has no dependencies and runs its own health probe (`--ping`),
# so the runtime image holds nothing but the binary: no shell, no package
# manager, no libc.

# ----------------------------------------------------------------- build
FROM rust:alpine AS build

RUN apk add --no-cache musl-dev rsync

WORKDIR /src
COPY . .

# rust-toolchain.toml also asks for the wasm32 target; that is pointless for
# the server build and makes it download a fresh toolchain instead of using
# the image's own. The channel is already "stable", so dropping it loses no
# version pin.
RUN rm -f rust-toolchain.toml

# On Alpine the host target is already *-linux-musl: a plain `--release`
# gives a static binary, and since we name no target, arm64/amd64 both come
# out of the same Dockerfile. fenec-server is deliberately on the `release` profile
# (unwind): a panicking connection thread takes down only its own session.
# FEATURES: fenec-server's features, e.g. `timing` for `make roundtrip-bench`.
# tools/image-cargo.sh builds in a tree it keeps in the cache mount, by
# content, and holds the binary to this build's sources before it copies it
# out: with target/ alone in the cache, a tree whose files were older than
# the last build's compiled nothing and the image kept the binary before.
# It lands in /built, a path no build before the script copied from: BuildKit
# keyed those COPYs by binaries a stale target/ had made, and could hand a
# build of the same sources another tree's layer (the script says how).
ARG FEATURES=""
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=fenec-image-cargo,target=/work,sharing=locked \
    tools/image-cargo.sh /built --release -p fenec-server ${FEATURES:+--features $FEATURES}

# The empty image has no shell to run `mkdir`: the data directory is prepared
# here and copied over together with its ownership.
RUN mkdir -p /data

# --------------------------------------------------------------- runtime
FROM scratch

COPY --from=build /built/fenec-server /usr/local/bin/fenec-server
# A named volume inherits this directory's ownership, so the non-root process
# can write to it. With a bind mount the ownership comes from the host:
#   docker run -v $PWD/data:/data --user $(id -u):$(id -g) ...
COPY --from=build --chown=65534:65534 /data /data

USER 65534:65534

VOLUME /data
EXPOSE 8080

# The binary runs the probe itself: `GET /_health` on 127.0.0.1:8080, which
# answers with no token and takes no lock -- a probe waiting on the lock
# during a long `compact` would report a healthy server as dead. If you
# change the port in CMD you have to add `--http` here as well.
#
# start-period is long: the documents are read before the listener opens
# (vectors the graph lacks are linked beside the queries after it).
HEALTHCHECK --interval=10s --timeout=3s --start-period=120s --retries=3 \
  CMD ["/usr/local/bin/fenec-server", "--ping"]

ENTRYPOINT ["/usr/local/bin/fenec-server"]

# 0.0.0.0 is outside loopback: fenec-server refuses to listen on that address
# without a token. Supply FENEC_HTTP_TOKEN (or --jwt-secret and --policy),
# or add --insecure deliberately. Overriding CMD drops all of these defaults.
CMD ["--http", "0.0.0.0:8080", "--file", "/data/data.fenec", "--sync", "250"]
