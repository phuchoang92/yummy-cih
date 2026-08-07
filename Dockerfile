# ─── Stage 1: graph-ui ────────────────────────────────────────────────────────
# Build the Vite/React graph browser. vite.config.ts resolves outDir as
# `../crates/cih-server/assets/graph` relative to graph-ui/ — mirror that
# path so the resolve() call lands at the right place inside the container.
FROM node:22-slim AS ui-builder

WORKDIR /build/graph-ui
COPY graph-ui/package.json graph-ui/package-lock.json ./
RUN npm ci --prefer-offline

COPY graph-ui/ ./
# Output lands at /build/crates/cih-server/assets/graph
RUN npm run build

# ─── Stage 2: Rust builder ─────────────────────────────────────────────────────
# tree-sitter compiles a C grammar; fastembed downloads the ONNX Runtime binary
# at build time via ort-download-binaries. Both require build tooling.
FROM rust:slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config libssl-dev ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .
# Overwrite the committed assets with the freshly built UI from Stage 1.
COPY --from=ui-builder /build/crates/cih-server/assets/graph \
     ./crates/cih-server/assets/graph

# Build both release binaries in one pass.
# ort-download-binaries fetches libonnxruntime.so into target/release/build/ort-sys-*/out/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release -p cih-server -p cih-engine && \
    cp target/release/cih-server /tmp/cih-server && \
    cp target/release/cih-engine /tmp/cih-engine && \
    find target/release/build -name "libonnxruntime.so*" ! -name "*.gz" \
        -exec cp -L {} /tmp/libonnxruntime.so \; 2>/dev/null ; \
    ls -lh /tmp/libonnxruntime.so 2>/dev/null || echo "ort: no .so found (may be static)"; \
    touch /tmp/libonnxruntime.so

# ─── Stage 2: Runtime ─────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 openjdk-17-jre-headless \
    && rm -rf /var/lib/apt/lists/*

# Binaries
COPY --from=builder /tmp/cih-server /usr/local/bin/cih-server
COPY --from=builder /tmp/cih-engine /usr/local/bin/cih-engine

# ONNX Runtime shared library (only present if fastembed uses dynamic linking)
COPY --from=builder /tmp/libonnxruntime.so /usr/local/lib/libonnxruntime.so
RUN test -s /usr/local/lib/libonnxruntime.so || rm -f /usr/local/lib/libonnxruntime.so; \
    ldconfig 2>/dev/null || true

# Pre-baked embedding model (all-MiniLM-L6-v2 ONNX, vendored in the repo). Makes the
# image self-contained: fastembed finds it via HF_HOME below and never downloads from
# HuggingFace at runtime — required for hosts whose network blocks huggingface.co.
# NOTE: this path is deliberately NOT under a mounted volume (/data), so a named
# volume can't shadow the baked model.
COPY vendor/hf-cache /opt/cih/hf-cache

# ── Volumes ───────────────────────────────────────────────────────────────────
# /data   — graph artifacts (persist across runs); the embedding model is baked
#           into the image at /opt/cih/hf-cache, not here
# /repo   — mount your Java repo here for cih-engine analyze
VOLUME ["/data", "/repo"]

# ── Environment defaults ──────────────────────────────────────────────────────
ENV CIH_BIND=0.0.0.0:8080
ENV FALKOR_URL=redis://falkordb:6379
ENV CIH_GRAPH_KEY=cih
ENV CIH_ARTIFACTS_DIR=/data/artifacts
# Embedding-model cache. Points at the pre-baked model COPYed above so fastembed
# never contacts HuggingFace at runtime. Override to a writable dir only if you
# intend to fetch a different model online.
ENV HF_HOME=/opt/cih/hf-cache
ENV RUST_LOG=info,cih_server=debug

EXPOSE 8080

# Run as a non-root user.
RUN useradd -m -u 1001 cih && \
    mkdir -p /data /repo && \
    chown cih:cih /data /repo
USER cih

# Default entrypoint: run the MCP server.
# Override with `cih-engine` for indexing.
CMD ["cih-server"]
