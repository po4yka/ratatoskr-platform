# The deployment artifact: three binaries, built for the machine they run on.
#
# `ratatoskr-workspace/docs/DEPLOYMENT_TARGET.md` fixes the target as one Raspberry Pi 5 —
# `aarch64`, Debian 12, glibc 2.36. That last number is why this file exists at all: a binary linked
# against a newer glibc does not start against an older one, and the failure is a loader error that
# says nothing about what is wrong. `debian:12-slim` IS glibc 2.36, so the runtime stage is an exact
# match rather than an approximation of one.
#
# Build it for the target explicitly; on an Apple Silicon developer machine that is native, not
# emulated, because the Docker VM is already `linux/arm64`:
#
#     docker buildx build --platform linux/arm64 \
#       --build-arg RATATOSKR_GIT_SHA="$(git rev-parse HEAD)" -t ratatoskr-platform:dev .
#
# `aarch64-unknown-linux-musl` was rejected: it swaps in musl's allocator, which is a throughput
# regression for a multi-threaded Tokio server, and it breaks the `tls-roots` feature that reads the
# system trust store.

# ---------------------------------------------------------------------------------------------
# builder
# ---------------------------------------------------------------------------------------------
# Pinned to the same version as `rust-toolchain.toml`, so the image does not download a second
# toolchain on the first cargo invocation and then build with it instead.
FROM rust:1.97.0-slim-bookworm AS builder

# `pkg-config` and a C compiler for the crates that build C: `ring` assembles its own primitives.
# `git` because the workspace resolves `ratatoskr-contracts` as a git dependency pinned to a SHA.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git pkg-config \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# The commit this artifact was built from. `crates/telemetry/src/identity.rs` reads it through
# `option_env!` at COMPILE time, so it has to be set before cargo runs; a build that omits it
# produces binaries that report `git_sha: unknown`, which is the first thing anyone looks at when a
# deployment misbehaves. `--build-arg RATATOSKR_GIT_SHA="$(git rev-parse HEAD)"`.
ARG RATATOSKR_GIT_SHA=unknown
ENV RATATOSKR_GIT_SHA=${RATATOSKR_GIT_SHA}

COPY . .

# `--locked` for the same reason the gate uses it: the lockfile is the artifact's dependency set, and
# a build that may silently resolve a different one is not the thing CI checked.
#
# `openapic` is deliberately not built. It is a generator for a checked-in document, it never runs in
# a deployment, and shipping it would put a tool that writes files into the runtime image.
RUN cargo build --release --locked \
      -p ratatoskr-edge -p ratatoskr-ingest -p ratatoskr-scheduler

# ---------------------------------------------------------------------------------------------
# runtime
# ---------------------------------------------------------------------------------------------
FROM debian:12-slim AS runtime

# The trust store the OTLP exporter's `tls-roots` feature reads. Without it an `https://` collector
# endpoint fails at handshake time, which surfaces as spans that silently never arrive.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --no-create-home --shell /usr/sbin/nologin ratatoskr

# All three in ONE directory, and this is load-bearing rather than tidy:
# `services/edge/tests/boot.rs` locates its siblings with
# `Path::new(env!("CARGO_BIN_EXE_ratatoskr-edge")).with_file_name(binary)`, so an image that split
# one binary per image would make three of its boot tests unrunnable against the artifact.
COPY --from=builder /build/target/release/ratatoskr-edge \
                    /build/target/release/ratatoskr-ingest \
                    /build/target/release/ratatoskr-scheduler \
                    /usr/local/bin/

# Carried through so a running container can report what it is. `platform_build_info` exposes it.
ARG RATATOSKR_GIT_SHA=unknown
ENV RATATOSKR_GIT_SHA=${RATATOSKR_GIT_SHA}

# Never root. The process binds ports above 1024, reads its configuration from the environment and
# writes nothing to the filesystem, so it needs no privilege at all.
USER ratatoskr

# No default CMD: the image carries three deployables and choosing between them silently would make
# a misconfigured unit start the wrong one. Every caller names its binary.
