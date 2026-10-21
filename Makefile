# Everything runs in containers; no local Rust toolchain needed.
RUST_IMAGE ?= rust:1-bookworm
VERSION := $(shell sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
DOCKER_RUST = docker run --rm -v $(CURDIR):/work -v oxide-cargo-registry:/usr/local/cargo/registry -w /work $(RUST_IMAGE)

.PHONY: test lint smoke image fixtures openapi up down

# Unit + API + OCR tests (fetches ONNX Runtime and the models once).
test:
	./scripts/test.sh

smoke:
	./scripts/smoke.sh

lint:
	$(DOCKER_RUST) sh -c './scripts/vendor_pdf_oxide.sh && rustup component add clippy rustfmt >/dev/null && \
	  cargo clippy --all-targets -- -D warnings && cargo clippy --all-targets --no-default-features -- -D warnings'

image:
	docker build --build-arg VERSION=$(VERSION) -t paz-oxide-api:latest -t paz-oxide-api:$(VERSION) .

fixtures:
	./scripts/gen_fixtures.sh

openapi:
	./scripts/openapi_json.sh

up:
	./scripts/local.sh

down:
	./scripts/local.sh down
