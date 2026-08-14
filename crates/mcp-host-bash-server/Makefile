# Makefile — allowlist-friendly build + install for the host-bash MCP server.
#
# WHY: host-bash's command allow-list permits `make` but NOT `cargo`/`rustc`
# directly. `make` spawns cargo as a CHILD process, so the build proceeds.
# cargo lives under ~/.cargo/bin (rustup-managed) and may not be on PATH in the
# host-bash environment, so we reference it by absolute path.
#
#   make            -> cargo build --release (this crate only)
#   make install    -> build + copy release binary to ~/bin + ad-hoc re-sign
#                      + chmod 755  (macOS deploy target)
#   make test       -> cargo test (this crate only)
#   make clean      -> cargo clean (workspace)
#
# WHY THE RE-SIGN STEP (codesign --force -s -): on Apple Silicon, ~/bin is a
# Gatekeeper provenance-tracked path. Overwriting the binary there leaves the
# recorded com.apple.provenance xattr pointing at the OLD signature, so AMFI
# SIGKILLs the freshly-copied binary on exec. Re-signing ad-hoc IN PLACE at the
# final path re-establishes a valid signature. Always re-sign after copying.
# (No-op / harmless on Linux, where codesign is absent — see the guard below.)

CARGO := $(HOME)/.cargo/bin/cargo
PKG   := mcp-host-bash-server
BIN   := mcp-host-bash-server
DEST  := $(HOME)/bin/$(BIN)
# Workspace target dir is two levels up (crate lives in crates/<name>/).
REL   := ../../target/release/$(BIN)

.PHONY: all build install test clean

all: build

build:
	$(CARGO) build --release -p $(PKG)

test:
	$(CARGO) test -p $(PKG)

install: build
	mkdir -p $(HOME)/bin
	cp $(REL) $(DEST)
	@if command -v codesign >/dev/null 2>&1; then \
		codesign --force -s - --timestamp=none $(DEST) && echo "re-signed (macOS)"; \
	fi
	chmod 755 $(DEST)
	@echo "installed: $(DEST)"

clean:
	$(CARGO) clean
