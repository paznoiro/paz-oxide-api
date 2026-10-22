# syntax=docker/dockerfile:1.7
#
# One image: native PDF + Office extraction, PaddleOCR and the Indra CLI.
#   docker build -t oxide-api .
#
# Every base image is pinned by multi-arch digest (Renovate bumps them).
# Rust and model stages use BUILDPLATFORM. Python dependencies install on
# TARGETPLATFORM (cross-platform builds need emulation for that stage).
#   docker build --platform linux/amd64 -t oxide-api .

ARG RUST_BOOKWORM=rust:1-bookworm@sha256:59037199c44290f2befcdd58dcc540164763fc296950255aaefeef096a1866b0
ARG ALPINE=alpine:3.20@sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc
ARG PYTHON_BASE=python:3.12-slim-bookworm@sha256:6a0e06f5ba0b373d051aabd3fffb9b873f27f9a2a3a14bcbe37dee6f0e3bbc70
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
COPY scripts/fetch_indra_models.sh /fetch_indra_models.sh
RUN sh /fetch_indra_models.sh /models/indra

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
# Python dependencies for the pinned Indra CLI (CPU wheels, no compilers).
# ---------------------------------------------------------------------------
FROM ${PYTHON_BASE} AS indra-runtime
RUN apt-get update && apt-get install -y --no-install-recommends libgomp1 \
    && rm -rf /var/lib/apt/lists/*
COPY requirements-indra.txt /tmp/requirements-indra.txt
RUN pip install --no-cache-dir --only-binary=:all: --require-hashes -r /tmp/requirements-indra.txt \
    && indra-ocr --version

FROM indra-runtime AS runtime
ARG ORT_VERSION
ARG VERSION=0.3.1
LABEL org.opencontainers.image.title="oxide-api" \
      org.opencontainers.image.description="Text and markdown extraction from PDF, Office, OpenDocument and image files, with OCR (en, hi, ar)" \
      org.opencontainers.image.source="https://github.com/paznoiro/paz-oxide-api" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.version="${VERSION}" \
      dev.oxide-api.pdf-oxide="0.3.78+patched" \
      dev.oxide-api.office-oxide="0.1.13" \
      dev.oxide-api.onnxruntime="${ORT_VERSION}" \
      dev.oxide-api.indra-ocr="0.3.2" \
      dev.oxide-api.models.indra="ed345c7f158eddbc7dee0c6a7269afb75a94f7e8" \
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
    OXIDE_API_OCR_MODEL_DIR=/models \
    OXIDE_API_INDRA_MODEL_DIR=/models/indra \
    OXIDE_API_INDRA_MATH=true \
    PYTHONDONTWRITEBYTECODE=1 \
    HF_HUB_OFFLINE=1
EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=3s --start-period=60s --retries=3 CMD ["/usr/local/bin/oxide-api", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/oxide-api"]
