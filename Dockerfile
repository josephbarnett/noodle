# syntax=docker/dockerfile:1
#
# Multi-stage build for the four noodle deployment binaries.
# One shared compile stage produces all four; four distroless runtime
# stages each carry exactly one binary. Select with `--target`:
#
#   docker build --platform linux/arm64 --target proxy     -t <reg>/noodle-proxy:<tag>     .
#   docker build --platform linux/arm64 --target embellish -t <reg>/noodle-embellish:<tag> .
#   docker build --platform linux/arm64 --target shipper   -t <reg>/noodle-shipper:<tag>   .
#   docker build --platform linux/arm64 --target viewer    -t <reg>/noodle-viewer:<tag>    .
#
# `viewer` embeds the React UI via rust-embed at compile time. Build
# `crates/noodle-viewer/web/dist/` *before* `docker build` so the
# build context carries the assets (`.dockerignore` does not exclude
# `dist/`). The `deploy.sh` runs `npm ci && npm run build` for you.
#
# Toolchain is rustc 1.95 (workspace is edition 2024). RPi5 nodes are
# arm64; this builds natively on Apple Silicon.

FROM rust:1.95-bookworm AS build
# Native-crypto crates (boring/openssl family) build through cmake + a
# C/C++ toolchain; clang/perl/pkg-config cover the common paths.
RUN apt-get update && apt-get install -y --no-install-recommends \
        cmake clang perl pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# Cache the cargo registry and target dir across builds. The target dir
# is a cache mount (not persisted in the layer), so copy the binaries
# out to /out within the same step.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release \
        --bin noodle \
        --bin noodle-embellish \
        --bin noodle-shipper \
        --bin noodle-viewer \
 && mkdir -p /out \
 && cp target/release/noodle \
       target/release/noodle-embellish \
       target/release/noodle-shipper \
       target/release/noodle-viewer /out/

# --- runtime images: distroless nonroot, one binary each ---

FROM gcr.io/distroless/cc-debian12:nonroot AS proxy
# The proxy binary dynamically links zlib (libz.so.1), which distroless
# cc-debian12 does not ship (it carries glibc, libgcc, libstdc++, libssl).
# embellish/shipper don't need it — proxy-only. Verified via `ldd`.
COPY --from=build /lib/aarch64-linux-gnu/libz.so.1 /lib/aarch64-linux-gnu/libz.so.1
COPY --from=build /out/noodle /usr/local/bin/noodle
# proxy listener + ops listener (informational; bound via NOODLE_LISTEN /
# NOODLE_OPS_LISTEN).
EXPOSE 62100 9091
ENTRYPOINT ["/usr/local/bin/noodle"]

FROM gcr.io/distroless/cc-debian12:nonroot AS embellish
COPY --from=build /out/noodle-embellish /usr/local/bin/noodle-embellish
ENTRYPOINT ["/usr/local/bin/noodle-embellish"]

FROM gcr.io/distroless/cc-debian12:nonroot AS shipper
COPY --from=build /out/noodle-shipper /usr/local/bin/noodle-shipper
ENTRYPOINT ["/usr/local/bin/noodle-shipper"]

FROM gcr.io/distroless/cc-debian12:nonroot AS viewer
# noodle-viewer embeds the built React UI via rust-embed at compile time.
# Runtime carries the static binary only — no Node, no assets directory.
# Default listen is 127.0.0.1:9092; bind to 0.0.0.0:9092 in the
# manifest via `--listen 0.0.0.0:9092` for in-cluster reachability.
COPY --from=build /out/noodle-viewer /usr/local/bin/noodle-viewer
EXPOSE 9092
ENTRYPOINT ["/usr/local/bin/noodle-viewer"]
