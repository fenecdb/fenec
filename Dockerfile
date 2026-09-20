# fenecdb -- PostgreSQL protocol server (`fenec-pg`)
#
#   docker build -t fenecdb .
#   docker run -d --name fenecdb -p 5433:5433 -v fenecdata:/data \
#     -e FENECPG_PASSWORD=secret --memory 1g fenecdb
#   psql -h 127.0.0.1 -p 5433 -U fenec
#
# Two stages: build statically with musl, then put the result into an *empty*
# image. fenec-pg has no dependencies and runs its own health probe (`--ping`),
# so the runtime image holds nothing but the binary: no shell, no package
# manager, no libc.

# ----------------------------------------------------------------- build
FROM rust:alpine AS build

RUN apk add --no-cache musl-dev

WORKDIR /src
COPY . .

# rust-toolchain.toml also asks for the wasm32 target; that is pointless for
# the server build and makes it download a fresh toolchain instead of using
# the image's own. The channel is already "stable", so dropping it loses no
# version pin.
RUN rm -f rust-toolchain.toml

# On Alpine the host target is already *-linux-musl: a plain `--release`
# gives a static binary, and since we name no target, arm64/amd64 both come
# out of the same Dockerfile. fenec-pg is deliberately on the `release` profile
# (unwind): a panicking connection thread takes down only its own session.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release -p fenec-pg && \
    cp target/release/fenec-pg /fenec-pg

# The empty image has no shell to run `mkdir`: the data directory is prepared
# here and copied over together with its ownership.
RUN mkdir -p /data

# --------------------------------------------------------------- runtime
FROM scratch

COPY --from=build /fenec-pg /usr/local/bin/fenec-pg
# A named volume inherits this directory's ownership, so the non-root process
# can write to it. With a bind mount the ownership comes from the host:
#   docker run -v $PWD/data:/data --user $(id -u):$(id -g) ...
COPY --from=build --chown=65534:65534 /data /data

USER 65534:65534

VOLUME /data
EXPOSE 5433

# The binary runs the probe itself: it connects, authenticates and exits --
# the same depth as `pg_isready`. It deliberately runs no query; a probe
# waiting on the lock during a long `compact` would report a healthy server
# as dead. It reads the password from FENECPG_PASSWORD. If you change the port
# in CMD you have to add `--listen` here as well.
#
# start-period is long: when the file holds no HNSW graph it is rebuilt from
# scratch at startup (~10 s for 100k x 128) and the listener stays closed
# until that finishes.
HEALTHCHECK --interval=10s --timeout=3s --start-period=120s --retries=3 \
  CMD ["/usr/local/bin/fenec-pg", "--ping"]

ENTRYPOINT ["/usr/local/bin/fenec-pg"]

# 0.0.0.0 is outside loopback: fenec-pg refuses to listen on that address
# without a password. Supply FENECPG_PASSWORD (or --password-file), or add
# --insecure deliberately. Overriding CMD drops all of these defaults.
CMD ["--listen", "0.0.0.0:5433", "--file", "/data/data.fenec", "--sync", "250"]
