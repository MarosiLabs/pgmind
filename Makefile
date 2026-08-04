# pgmind — canonical entry points (RFC-001 D8). CI runs exactly these targets.

PG ?= 18
PGRX_VERSION := 0.19.2
# macOS: bindgen needs the SDK path or clang can't find system headers (inttypes.h),
# and Postgres source builds need keg-only icu4c on the pkg-config path
# (prereqs: brew install pkgconf icu4c)
ifeq ($(shell uname),Darwin)
export SDKROOT ?= $(shell xcrun --show-sdk-path)
export PKG_CONFIG_PATH := /opt/homebrew/opt/icu4c/lib/pkgconfig:$(PKG_CONFIG_PATH)
endif

.PHONY: build test lint fmt eval setup clean

build:
	cd extension && cargo build --no-default-features --features pg$(PG)

test:
	cd extension && cargo pgrx test pg$(PG)

lint:
	cd extension && cargo fmt --check
	cd extension && cargo clippy --no-default-features --features pg$(PG) -- -D warnings

fmt:
	cd extension && cargo fmt

eval:
	python3 eval/harness.py

# pgrx-managed Postgres (compiled into ~/.pgrx): self-contained and always writable.
# System installs are traps on macOS: libpq's pg_config is client-only, and writing
# extensions into Postgres.app's TCC-protected bundle fails with EPERM.
# In CI we bind to PGDG's pg_config instead (see .github/workflows/ci.yml).
setup:
	cargo install cargo-pgrx --version $(PGRX_VERSION) --locked
	cargo pgrx init --pg$(PG) download

clean:
	cd extension && cargo clean
	rm -rf eval/results
