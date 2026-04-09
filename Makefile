# mstat (fdu-mstat) — cross-compile + bundle releases
#
# Usage:
#   make                    Build release for host
#   make all                Build all targets via cross (Docker)
#   make bundle             Build all + create tarballs in dist/
#   make install            Install to /usr/local/bin
#   make fmt                Run rustfmt
#   make check              Show build configuration
#
# Cross-compilation requires `cross` (cargo install cross) and Docker.

BINARY   := mstat
VERSION  := $(shell grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
DIST     := dist
HOST     := $(shell rustc -vV | grep '^host:' | cut -d' ' -f2)
TARGET   ?= $(HOST)

# All release targets (Linux-only — this tool reads /proc)
TARGETS  := x86_64-unknown-linux-gnu \
            x86_64-unknown-linux-musl \
            aarch64-unknown-linux-gnu \
            aarch64-unknown-linux-musl \
            armv7-unknown-linux-gnueabihf \
            riscv64gc-unknown-linux-gnu

# Use cross (via podman) for non-host targets, plain cargo for host.
# cross needs CROSS_CONTAINER_ENGINE=podman since we use podman, not docker.
export CROSS_CONTAINER_ENGINE ?= podman

ifeq ($(TARGET),$(HOST))
  CARGO_CMD = cargo
else
  CARGO_CMD = cross
endif

CARGO_FLAGS := --release --target $(TARGET)

.PHONY: build release all bundle install clean check fmt

# ── Single-target ────────────────────────────────────────────────────────────

build:
	$(CARGO_CMD) build $(CARGO_FLAGS)

release: build
	@mkdir -p $(DIST)
	@BIN=target/$(TARGET)/release/$(BINARY); \
	if [ -f "$$BIN" ]; then \
		cp "$$BIN" $(DIST)/$(BINARY)-$(TARGET); \
		strip $(DIST)/$(BINARY)-$(TARGET) 2>/dev/null || true; \
		ls -lh $(DIST)/$(BINARY)-$(TARGET); \
	fi

# ── Multi-target ─────────────────────────────────────────────────────────────

all:
	@for t in $(TARGETS); do \
		echo "══ $$t"; \
		$(MAKE) --no-print-directory release TARGET=$$t \
			|| echo "  SKIP (missing toolchain or Docker image)"; \
	done

bundle: all
	@mkdir -p $(DIST)
	@for t in $(TARGETS); do \
		BIN=$(DIST)/$(BINARY)-$$t; \
		if [ -f "$$BIN" ]; then \
			TAR=$(DIST)/$(BINARY)-$(VERSION)-$$t.tar.gz; \
			tar czf "$$TAR" -C $(DIST) $(BINARY)-$$t; \
			echo "  $$TAR ($$(du -h "$$TAR" | cut -f1))"; \
		fi; \
	done
	@echo "── Checksums ──"
	@cd $(DIST) && sha256sum *.tar.gz > SHA256SUMS 2>/dev/null && cat SHA256SUMS

# ── Install ──────────────────────────────────────────────────────────────────

PREFIX ?= /usr/local
install: release
	install -Dm755 $(DIST)/$(BINARY)-$(TARGET) $(DESTDIR)$(PREFIX)/bin/$(BINARY)

# ── Development ──────────────────────────────────────────────────────────────

fmt:
	cargo fmt

check:
	@echo "Version:  $(VERSION)"
	@echo "Host:     $(HOST)"
	@echo "Target:   $(TARGET)"
	@echo "Backend:  $(CARGO_CMD)"
	@echo "Targets:  $(TARGETS)"
	@which cross >/dev/null 2>&1 && echo "cross:    $(shell cross --version 2>/dev/null)" || echo "cross:    not installed"

clean:
	cargo clean
	rm -rf $(DIST)
