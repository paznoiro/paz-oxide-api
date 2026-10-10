# Everything runs in containers; no local Rust toolchain needed.
RUST_IMAGE ?= rust:1-bookworm
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
	docker build -t oxide-api:latest -t oxide-api:0.1.0 .

fixtures:
	./scripts/gen_fixtures.sh

openapi:
	./scripts/openapi_json.sh

up:
	./scripts/local.sh

down:
	docker compose down
