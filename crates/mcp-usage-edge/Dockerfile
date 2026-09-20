# syntax=docker/dockerfile:1.7
#
# Build context is the WORKSPACE ROOT, not this directory:
#   docker build -f crates/mcp-usage-edge/Dockerfile .
#
# Build stage
# Pinned to the Debian release the runtime stage is built from, so the glibc the
# binary is linked against matches the one it runs on.
FROM rust:1.96-slim-bookworm AS builder

# cmake, clang and a linker are aws-lc-sys build inputs, pulled in by the `tls`
# feature. A sidecar that only ever fronts a loopback or private-network
# upstream can drop all of this by building with --no-default-features.
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake clang libclang-dev pkg-config \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

# The target directory is a BuildKit cache mount, so incremental artifacts
# survive between builds. A cache mount is NOT part of the resulting layer, so
# the binary has to be copied out of it inside this same RUN.
#
# sharing=locked serializes concurrent builds rather than letting them corrupt
# the shared target directory. --locked fails loudly on a stale Cargo.lock.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/app/target,sharing=locked \
    cargo build --release --locked -p mcp-usage-edge \
 && mkdir -p /out \
 && cp target/release/mcp-usage-edge /out/mcp-usage-edge

# Runtime stage
#
# distroless/cc is glibc + libgcc and nothing else: no shell, no package
# manager, no coreutils. Root certificates come with the image, which is what
# an https upstream needs.
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /out/mcp-usage-edge /usr/local/bin/mcp-usage-edge
COPY crates/mcp-usage-edge/edge.example.toml /etc/mcp-usage-edge/edge.toml

EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/mcp-usage-edge"]
CMD ["/etc/mcp-usage-edge/edge.toml"]
