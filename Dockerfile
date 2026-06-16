# ─── Stage 1: Builder ─────────────────────────────────────────────────────────
# tree-sitter compiles a C grammar; fastembed downloads the ONNX Runtime binary
# at build time via ort-download-binaries. Both require build tooling.
FROM rust:slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config libssl-dev ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

# Build both release binaries in one pass.
# ort-download-binaries fetches libonnxruntime.so into target/release/build/ort-sys-*/out/
RUN cargo build --release -p cih-server -p cih-engine

# Collect the ONNX Runtime shared library into a fixed path so the next stage
# can COPY it without needing shell glob expansion.
RUN find target/release/build -name "libonnxruntime.so*" ! -name "*.gz" \
    -exec cp -L {} /tmp/libonnxruntime.so \; 2>/dev/null ; \
    ls -lh /tmp/libonnxruntime.so 2>/dev/null || echo "ort: no .so found (may be static)"; \
    touch /tmp/libonnxruntime.so

# ─── Stage 2: Runtime ─────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Binaries
COPY --from=builder /build/target/release/cih-server /usr/local/bin/cih-server
COPY --from=builder /build/target/release/cih-engine /usr/local/bin/cih-engine

# ONNX Runtime shared library (only present if fastembed uses dynamic linking)
COPY --from=builder /tmp/libonnxruntime.so /usr/local/lib/libonnxruntime.so
RUN test -s /usr/local/lib/libonnxruntime.so || rm -f /usr/local/lib/libonnxruntime.so; \
    ldconfig 2>/dev/null || true

# ── Volumes ───────────────────────────────────────────────────────────────────
# /data   — graph artifacts + HuggingFace model cache (persist across runs)
# /repo   — mount your Java repo here for cih-engine analyze
VOLUME ["/data", "/repo"]

# ── Environment defaults ──────────────────────────────────────────────────────
ENV CIH_BIND=0.0.0.0:8080
ENV FALKOR_URL=redis://falkordb:6379
ENV CIH_GRAPH_KEY=cih
ENV CIH_ARTIFACTS_DIR=/data/artifacts
# fastembed downloads embedding models from HuggingFace on first use; cache them here
ENV HF_HOME=/data/hf-cache
ENV RUST_LOG=info,cih_server=debug

EXPOSE 8080

# Default entrypoint: run the MCP server.
# Override with `cih-engine` for indexing.
CMD ["cih-server"]
