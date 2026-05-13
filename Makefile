.PHONY: test test-verbose test-unit test-e2e test-live test-session-task test-hooks test-agent-msg test-agent-tail test-claude-event test-self-clear test-watchers test-dashboard test-trust-workspace test-claude-tmux-env build deploy install install-hooks compose-up compose-down compose-build bootstrap clean

# Default: run all tests in parallel via nextest (preferred) or cargo test
test:
	@command -v cargo-nextest >/dev/null 2>&1 && \
		cargo nextest run || \
		cargo test

# Verbose output (show stdout/stderr from passing tests too)
test-verbose:
	@command -v cargo-nextest >/dev/null 2>&1 && \
		cargo nextest run --no-capture || \
		cargo test -- --nocapture

# Unit + fixture tests only (fast, ~0.1s)
test-unit:
	@command -v cargo-nextest >/dev/null 2>&1 && \
		cargo nextest run -E 'not binary(~e2e_)' || \
		cargo test --lib --test unit_activity_detection

# e2e tests only (tmux-based, ~10s)
test-e2e:
	@command -v cargo-nextest >/dev/null 2>&1 && \
		cargo nextest run -E 'binary(~e2e_) and not test(~live)' || \
		cargo test --test 'e2e_*' -- --skip live

# Live e2e tests (spawn real Claude Code, ~1-2 min each, #[ignore] by default)
test-live:
	@command -v cargo-nextest >/dev/null 2>&1 && \
		cargo nextest run --run-ignored=only || \
		cargo test -- --ignored

# Run the session-task Python tests (cross-session queue CLI under tools/).
# Pre-existing 5 failures in test_queue_claude_event.py are tracked in
# tools/session-task/README.md and are unrelated to this migration.
test-session-task:
	uv run --python 3.11 --with pytest pytest tools/session-task/tests/ -v

# Run the obligations / hooks Python tests. These are self-contained
# scripts (not pytest), so we just exec them directly. Each runs against
# an isolated $HOME tmpdir so the live obligations.json is never touched.
# The pre-agent-queue-gate-hook test exercises the real `session-task`
# binary; it must be on PATH (or installed via `make install`).
test-hooks:
	tools/hooks/tests/pre-tool-obligations-gate-hook.test
	tools/hooks/tests/pre-agent-queue-gate-hook.test

# Run the agent-msg embedded test suite (CLI for delivering async
# messages to running Claude Code agents via the obligations gate).
# The script's `--test` flag runs all 38 cases in-process against
# isolated tmpdirs, no obligations side effects.
test-agent-msg:
	python3 tools/agent-msg/agent-msg --test

# Run the agent-tail embedded test suite (CLI for streaming agent
# JSONL transcripts). Tests cover pure helpers, format_record dispatch,
# resolution under a fake projects tree, and the follow-mode handler
# (truncation + rotation). All cases run in-process against tmpdirs.
test-agent-tail:
	python3 tools/agent-tail/agent-tail --test

# Run the claude-event + claude-event-tail unit tests.
test-claude-event:
	python3 tools/claude-event/tests/test_claude_event.py

# Run the self-clear config-only smoke tests (the full inject flow needs
# a live Claude Code tmux pane, which can't be reproduced in unit tests).
test-self-clear:
	python3 tools/watchers/tests/test_self_clear_config.py

# Run the claude-event-watch fast-path smoke test.
test-watchers: test-self-clear
	tools/watchers/tests/test_claude_event_watch.sh

# Run the dashboard parser tests (sources dashboard-lib.sh in a bash
# subshell and exercises conf_get / conf_windows / has_split / expected_panes
# against fixtures). 33 cases, ~1s.
test-dashboard:
	tools/dashboard/tests/dashboard-parser.test

# Run the trust-workspace embedded test suite (claude-container's
# pre-seed for ~/.claude.json's projects[<workspace>].hasTrustDialogAccepted
# entry; suppresses the in-container first-launch trust prompt). 11 cases,
# <0.1s, all in-process against tmpdir HOMEs.
test-trust-workspace:
	python3 container/bin/trust-workspace.py --test

# Run the claude-tmux env / mount passthrough tests (corporate CA bundle
# forwarding, proxy passthrough, host hooks-dir bind-mount). Exercises
# the wrapper's --print-docker-args debug hook so no docker daemon is
# needed. 12 cases, ~1s.
test-claude-tmux-env:
	container/bin/tests/claude-tmux-env.test

# Release build
build:
	cargo build --release

# Build + restart systemd service
deploy: build
	sudo systemctl restart claude-watch

# Install built binaries + scripts onto $PATH ($BIN_DIR, default ~/bin).
# Targets:
#   - claude-watch                          : the Rust daemon
#   - session-task                          : Python CLI (queue + resume action)
#   - obligations                           : obligations gate CLI
#   - pre-agent-queue-gate-hook             : PreToolUse hook (Agent matcher)
#   - pre-tool-obligations-gate-hook        : PreToolUse hook (* matcher)
#   - post-tool-obligations-update-hook     : PostToolUse hook (* matcher)
#   - post-tool-mark-attachment-read-hook   : PostToolUse hook (Read matcher)
#
# Install policy:
#   - The claude-watch Rust daemon is a build artifact, so it's a real
#     file copy from target/release/ into $(BIN_DIR). Re-running `make
#     install` after `make build` refreshes it.
#   - Every other tool is a script (Python / shell). Those install as
#     ABSOLUTE-PATH symlinks back to the source under tools/, so editing
#     a script in-tree is immediately reflected in $(BIN_DIR) without
#     another `make install` round-trip. `ln -sfn` makes the operation
#     idempotent (overwrites existing files / stale symlinks; -n
#     prevents following a directory at the link path).
BIN_DIR ?= $(HOME)/bin

install: build
	@mkdir -p $(BIN_DIR)
	@install -m 0755 target/release/claude-watch $(BIN_DIR)/claude-watch
	@ln -sfn $(abspath tools/session-task/session-task) $(BIN_DIR)/session-task
	@ln -sfn $(abspath tools/obligations/obligations) $(BIN_DIR)/obligations
	@ln -sfn $(abspath tools/hooks/pre-agent-queue-gate-hook) $(BIN_DIR)/pre-agent-queue-gate-hook
	@ln -sfn $(abspath tools/hooks/pre-tool-obligations-gate-hook) $(BIN_DIR)/pre-tool-obligations-gate-hook
	@ln -sfn $(abspath tools/hooks/post-tool-obligations-update-hook) $(BIN_DIR)/post-tool-obligations-update-hook
	@ln -sfn $(abspath tools/hooks/post-tool-mark-attachment-read-hook) $(BIN_DIR)/post-tool-mark-attachment-read-hook
	@ln -sfn $(abspath tools/agent-msg/agent-msg) $(BIN_DIR)/agent-msg
	@ln -sfn $(abspath tools/agent-tail/agent-tail) $(BIN_DIR)/agent-tail
	@ln -sfn $(abspath tools/claude-event/claude-event) $(BIN_DIR)/claude-event
	@ln -sfn $(abspath tools/claude-event/claude-event-tail) $(BIN_DIR)/claude-event-tail
	@ln -sfn $(abspath tools/watchers/claude-event-watch) $(BIN_DIR)/claude-event-watch
	@ln -sfn $(abspath tools/watchers/self-clear) $(BIN_DIR)/self-clear
	@echo "Installed to $(BIN_DIR):"
	@echo "  - claude-watch              (file copy, build artifact)"
	@echo "  - session-task              (symlink -> tools/session-task/)"
	@echo "  - obligations               (symlink -> tools/obligations/)"
	@echo "  - pre-agent-queue-gate-hook (symlink -> tools/hooks/)"
	@echo "  - pre-tool-obligations-gate-hook (symlink -> tools/hooks/)"
	@echo "  - post-tool-obligations-update-hook (symlink -> tools/hooks/)"
	@echo "  - post-tool-mark-attachment-read-hook (symlink -> tools/hooks/)"
	@echo "  - agent-msg                 (symlink -> tools/agent-msg/)"
	@echo "  - agent-tail                (symlink -> tools/agent-tail/)"
	@echo "  - claude-event              (symlink -> tools/claude-event/)"
	@echo "  - claude-event-tail         (symlink -> tools/claude-event/)"
	@echo "  - claude-event-watch        (symlink -> tools/watchers/)"
	@echo "  - self-clear                (symlink -> tools/watchers/)"

# Install git pre-commit hook (warning-free build + unit/fixture tests).
# Symlinks scripts/git-hooks/pre-commit into .git/hooks so script edits
# take effect without re-running this target. Removes any previous file
# at .git/hooks/pre-commit (including older inline-generated versions).
install-hooks:
	@rm -f .git/hooks/pre-commit
	@ln -s ../../scripts/git-hooks/pre-commit .git/hooks/pre-commit
	@echo "Pre-commit hook installed (symlink -> scripts/git-hooks/pre-commit)."

# --- examples/compose targets -----------------------------------------
# Convenience wrappers around the integrated docker-compose example at
# examples/compose/. The compose file wires claude-container +
# queue-minisite + eichi-search; see examples/compose/README.md for
# prerequisites (Docker, ANTHROPIC_API_KEY, sibling eichi clone).

# Run the bootstrap helper that checks prereqs, clones eichi sibling,
# and seeds examples/compose/.env from .env.example.
bootstrap:
	@bash examples/compose/bootstrap.sh

# Build the compose stack images (skip the sibling eichi build context
# if eichi isn't cloned next door — `docker compose build` will surface
# the missing-context error if so).
compose-build:
	@cd examples/compose && docker compose build

# Bring the integrated compose stack up in the foreground.
compose-up:
	@cd examples/compose && docker compose up

# Tear down the compose stack (volumes survive; add -v to nuke
# claude-container-versions).
compose-down:
	@cd examples/compose && docker compose down

# Clean build artifacts
clean:
	cargo clean
