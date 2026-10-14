# syntax=docker/dockerfile:1
FROM rust:1.90-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
COPY proto ./proto
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates util-linux xfsprogs e2fsprogs \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/clever-csi-driver /usr/local/bin/clever-csi-driver
# The controller overrides this to UID 65532. The node service must mount devices.
USER 0:0
ENTRYPOINT ["/usr/local/bin/clever-csi-driver"]
