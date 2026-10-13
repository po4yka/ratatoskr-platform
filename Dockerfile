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
# Three stages, in this order: `builder`, `export` and `runtime`. A plain `docker build` builds the
# LAST stage, so `runtime` stays last and every existing caller keeps the image it had. `export` is
# what `ratatoskr-workspace`'s reusable-release-artifact workflow publishes for each commit on main:
#
#     docker buildx build --platform linux/arm64 --target export --output type=local,dest=out .
#
# Both base images are pinned by digest as well as by tag. The tag says what was meant; the digest
# is what was built, and a tag that is re-pushed under it would otherwise change the artifact
# without a commit here.
#
# `aarch64-unknown-linux-musl` was rejected: it swaps in musl's allocator, which is a throughput
# regression for a multi-threaded Tokio server, and it breaks the `tls-roots` feature that reads the
# system trust store.

# ---------------------------------------------------------------------------------------------
# builder
# ---------------------------------------------------------------------------------------------
# Pinned to the same version as `rust-toolchain.toml`, so the image does not download a second
# toolchain on the first cargo invocation and then build with it instead.
FROM rust:1.97.0-slim-bookworm@sha256:6d220bf85c74e842a79da63997af8d2e74455c0b8847d8bb3a5888572334991d AS builder

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
#
# The registry, the git checkouts and `target/` are BuildKit cache mounts, so a rebuild on the same
# runner compiles only what changed. A cache mount is not part of the layer, which is why the three
# binaries are copied out to `/out` in the same step: a later `COPY --from=builder` cannot see into
# the cache.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked \
      -p ratatoskr-edge -p ratatoskr-ingest -p ratatoskr-scheduler \
 && mkdir /out \
 && cp target/release/ratatoskr-edge target/release/ratatoskr-ingest target/release/ratatoskr-scheduler /out/

# ---------------------------------------------------------------------------------------------
# export
# ---------------------------------------------------------------------------------------------
# Only the binaries, and nothing else: the local exporter writes this stage's whole filesystem, so
# anything here is published. The release-artifact check refuses a file no unit starts.
FROM scratch AS export
COPY --from=builder /out/ /

# ---------------------------------------------------------------------------------------------
# runtime
# ---------------------------------------------------------------------------------------------
FROM debian:12-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587 AS runtime

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
COPY --from=builder /out/ratatoskr-edge \
                    /out/ratatoskr-ingest \
                    /out/ratatoskr-scheduler \
                    /usr/local/bin/

# Carried through so a running container can report what it is. `platform_build_info` exposes it.
ARG RATATOSKR_GIT_SHA=unknown
ENV RATATOSKR_GIT_SHA=${RATATOSKR_GIT_SHA}

# Never root. The process binds ports above 1024, reads its configuration from the environment and
# writes nothing to the filesystem, so it needs no privilege at all.
USER ratatoskr

# No default CMD: the image carries three deployables and choosing between them silently would make
# a misconfigured unit start the wrong one. Every caller names its binary.
