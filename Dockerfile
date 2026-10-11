# syntax=docker/dockerfile:1.7
#
# One image: native PDF + Office extraction and OCR (en, hi, ar).
#   docker build -t oxide-api .
#
# Every base image is pinned by multi-arch digest (Renovate bumps them).
# All RUN stages execute on the build machine ($BUILDPLATFORM) and the Rust
# stage cross-compiles for $TARGETPLATFORM, so an amd64 image builds at native
# speed on an Apple-silicon Mac (no QEMU):
#   docker build --platform linux/amd64 -t oxide-api .

ARG RUST_BOOKWORM=rust:1-bookworm@sha256:59037199c44290f2befcdd58dcc540164763fc296950255aaefeef096a1866b0
ARG ALPINE=alpine:3.20@sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc
ARG CC_BASE=gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
ARG DEBIAN_SLIM=debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251

ARG ORT_VERSION=1.28.3
ARG ORT_SHA256_AMD64=db14e4863bd37893fc59729d986ab2a0d043d10b7d44da1913c4982b7e3d009c
ARG ORT_SHA256_ARM64=6c6b1ae96d7b0be9f555092857c6a4f2b0b5587a5298d732d9e52e8253038ce7

# ---------------------------------------------------------------------------
# vendor: pdf_oxide 0.3.78 + compatibility/hardening patches (see patches/)
# ---------------------------------------------------------------------------
FROM --platform=$BUILDPLATFORM ${ALPINE} AS vendor
RUN apk add --no-cache curl patch
WORKDIR /src
COPY scripts/vendor_pdf_oxide.sh scripts/
COPY patches patches
RUN ./scripts/vendor_pdf_oxide.sh

# ---------------------------------------------------------------------------
# build: the service binary (default features include OCR)
# ---------------------------------------------------------------------------
FROM --platform=$BUILDPLATFORM ${RUST_BOOKWORM} AS build
ARG BUILDARCH
ARG TARGETARCH
# Target triple + C cross compiler (mimalloc and ring compile C). Same Debian
# release as the runtime base, so the glibc the binary links against matches.
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu;  cc=x86_64-linux-gnu-gcc;  pkgs="gcc-x86-64-linux-gnu libc6-dev-amd64-cross" ;; \
      arm64) triple=aarch64-unknown-linux-gnu; cc=aarch64-linux-gnu-gcc; pkgs="gcc-aarch64-linux-gnu libc6-dev-arm64-cross" ;; \
      *) echo "unsupported TARGETARCH $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    if [ "$TARGETARCH" = "$BUILDARCH" ]; then \
      cc=gcc; \
    else \
      apt-get update && apt-get install -y --no-install-recommends $pkgs && rm -rf /var/lib/apt/lists/*; \
    fi; \
    rustup target add "$triple"; \
    printf '%s %s\n' "$triple" "$cc" > /target
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY --from=vendor /src/vendor vendor
COPY src src
COPY openapi.json openapi.yaml ./
RUN --mount=type=cache,id=oxide-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=oxide-target-glibc,target=/src/target \
    set -eu; read -r triple cc < /target; \
    env "CARGO_TARGET_$(echo "$triple" | tr 'a-z-' 'A-Z_')_LINKER=$cc" \
        "CC_$(echo "$triple" | tr '-' '_')=$cc" \
      cargo build --release --locked --target "$triple"; \
    cp "target/$triple/release/oxide-api" /oxide-api

# ---------------------------------------------------------------------------
# onnxruntime: official release tarball, sha256-checked
# ---------------------------------------------------------------------------
FROM --platform=$BUILDPLATFORM ${ALPINE} AS onnxruntime
ARG TARGETARCH
ARG ORT_VERSION
ARG ORT_SHA256_AMD64
ARG ORT_SHA256_ARM64
RUN apk add --no-cache curl
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64|x86_64)  arch=x64;     sum="$ORT_SHA256_AMD64" ;; \
      arm64|aarch64) arch=aarch64; sum="$ORT_SHA256_ARM64" ;; \
      *) echo "unsupported arch ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL --retry 3 -o /tmp/ort.tgz \
      "https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-linux-${arch}-${ORT_VERSION}.tgz"; \
    echo "${sum}  /tmp/ort.tgz" | sha256sum -c -; \
    mkdir -p /ort && tar -xzf /tmp/ort.tgz -C /tmp; \
    cp -P /tmp/onnxruntime-linux-${arch}-${ORT_VERSION}/lib/libonnxruntime.so* /ort/

# ---------------------------------------------------------------------------
# models: detector + en/hi/ar recognizers and dictionaries, sha256-checked
# ---------------------------------------------------------------------------
FROM --platform=$BUILDPLATFORM ${ALPINE} AS models
RUN apk add --no-cache curl
COPY scripts/fetch_ocr_models.sh /fetch_ocr_models.sh
RUN sh /fetch_ocr_models.sh /models

# ---------------------------------------------------------------------------
# fonts: metric-compatible substitutes for the PDF base-14 fonts. OCR reads
# embedded scans without fonts, but pages that must be *rendered* first
# (ocr=force, legacy-font pages, pages without a usable scan) need a
# fallback for non-embedded Helvetica/Times/Courier, or they render blank.
# ---------------------------------------------------------------------------
FROM --platform=$BUILDPLATFORM ${DEBIAN_SLIM} AS fonts
RUN apt-get update && apt-get install -y --no-install-recommends fonts-liberation2 \
 && mkdir -p /fonts && cp /usr/share/fonts/truetype/liberation2/*.ttf /fonts/ \
 && cp /usr/share/doc/fonts-liberation2/copyright /fonts/LICENSE-Liberation

# ---------------------------------------------------------------------------
# runtime (final stage, the default build target). COPY only, no RUN, so it
# never executes target-architecture code at build time.
# ---------------------------------------------------------------------------
FROM ${CC_BASE} AS runtime
ARG ORT_VERSION
LABEL org.opencontainers.image.title="oxide-api" \
      org.opencontainers.image.description="Text and markdown extraction from PDF, Office, OpenDocument and image files, with OCR (en, hi, ar)" \
      org.opencontainers.image.source="https://github.com/OWNER/oxide-api" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.version="0.2.0" \
      dev.oxide-api.pdf-oxide="0.3.78+patched" \
      dev.oxide-api.office-oxide="0.1.13" \
      dev.oxide-api.onnxruntime="${ORT_VERSION}" \
      dev.oxide-api.models.deepghs-paddleocr="e7bd63d9c674c96bd4fdab435af0efb383919c18" \
      dev.oxide-api.models.monkt-paddleocr-onnx="7b02d0a30a07ba2b92ad1ff5a8941ae2c633de65" \
      dev.oxide-api.models.paddleocr-dicts="dab3fe35379033fdcb2d0e9572fac0b36c9a9ebf"
COPY --from=onnxruntime /ort/ /usr/lib/
COPY --from=fonts /fonts /usr/share/fonts/truetype/liberation2
COPY --from=models /models /models
COPY --from=build /oxide-api /usr/local/bin/oxide-api
USER 65532:65532
# No RUST_LOG here: setting it replaces the built-in filter that keeps the
# engines' document-derived log lines out (see init_logging in src/main.rs).
ENV OXIDE_API_ADDR=0.0.0.0:8080 \
    ORT_DYLIB_PATH=/usr/lib/libonnxruntime.so \
    OXIDE_API_OCR_MODEL_DIR=/models
EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=3s --start-period=60s --retries=3 CMD ["/usr/local/bin/oxide-api", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/oxide-api"]
