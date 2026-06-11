.PHONY: test test-verbose test-unit test-e2e test-live test-session-task test-hooks test-agent-msg test-agent-tail test-claude-event test-self-clear test-watchers test-dashboard test-trust-workspace test-claude-tmux-env test-hooks-shim test-doc-links test-install-hooks test-entrypoint test-cw test-mcp-host-bash test-mcp-proxy-auth-shim test-install-host-deps test-launchd-plist test-load-bearer-from-keychain test-personal-mcp-host test-personal-mcp-host-plist test-personal-mcp-install test-ttyd-paste-handler test-claude-md-size build deploy install install-hooks compose-up compose-down compose-build container-build bootstrap redeploy clean

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
# Self-contained: each test runs against a tempdir HOME so the live
# ~/.config/session/queue.json is never touched. ~36s, 165 cases.
test-session-task:
	uv run --python 3.11 --with pytest pytest tools/session-task/tests/ -v

# Run the obligations / hooks Python tests. These are self-contained
# scripts (not pytest), so we just exec them directly. Each runs against
# an isolated $HOME tmpdir so the live obligations.json is never touched.
# The pre-agent-queue-gate-hook test exercises the real `session-task`
# binary; it must be on PATH (or installed via `make install`).
test-hooks:
	python3 tools/obligations/shell_ast.py --test
	tools/hooks/tests/pre-tool-obligations-gate-hook.test
	tools/hooks/tests/pre-agent-queue-gate-hook.test
	tools/hooks/tests/pre-tool-claude-watch-alert-gate-hook.test
	tools/hooks/tests/user-prompt-claude-watch-alert-record-hook.test
	tools/hooks/tests/pre-tool-dispatch-gate-hook.test
	tools/hooks/pre-agent-background-required-hook --test
	tools/hooks/pre-agent-worktree-isolation-hook --test
	tools/hooks/worktree-create-hook --test
	tools/claude-watch-ack/tests/claude-watch-ack.test
	tools/claude-watch-dispatch/tests/claude-watch-dispatch.test

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
# forwarding, proxy passthrough, host hooks-dir bind-mount, set-but-missing
# path warnings). Exercises the wrapper's --print-docker-args debug hook so
# no docker daemon is needed. 23 cases, ~1s.
test-claude-tmux-env:
	container/bin/tests/claude-tmux-env.test

# Run the exec-hook shim tests (settings.json hook safe-exec wrapper for
# cross-arch hooks — ELF passthrough, Mach-O / unknown / missing no-op,
# dedup flag file), the generate-hooks-shim-settings rewrite tests
# (container-local settings.json with every hook command wrapped in
# /usr/local/bin/exec-hook), AND the generate-project-mcp-json tests
# (project-tier .mcp.json with MCP server commands wrapped, the v21
# follow-up fix). All run directly on Linux against synthetic inputs;
# no container needed.
test-hooks-shim:
	container/hooks-shim/tests/exec-hook.test
	container/hooks-shim/tests/exec-hook-bridge.test
	container/hooks-shim/tests/generate-hooks-shim-settings.test
	container/hooks-shim/tests/generate-project-mcp-json.test

# No-broken-links gate for the docs baked into the container image. Runs the
# checker's embedded self-tests, then verifies every relative markdown link in
# container/baked-CLAUDE.md (and repo-wide) resolves to a path that exists in
# the repo. baked-CLAUDE.md now links to its sibling docs by RELATIVE path
# (they are COPYed into /opt/claude-container/ alongside it), so a link to an
# un-baked path is a real in-container 404 — this gate catches it at CI time.
test-doc-links:
	python3 scripts/check-doc-links.py --self-test
	python3 scripts/check-doc-links.py --all

# CLAUDE.md size guard. Every CLAUDE.md is loaded into Claude Code's context
# at session start and stays there all session; /doctor recommends each stay
# under ~40,000 CHARACTERS. This gate fails when a tracked CLAUDE.md exceeds
# the generic HARD_LIMIT (40k) — except container/baked-CLAUDE.md, which is
# intentionally ~76k today and is pinned by a ratchet ceiling in the script's
# ALLOWLIST so it cannot GROW (the lever that drives it back down). The SAME
# script runs in scripts/git-hooks/pre-commit; CI is the real enforcement
# since the local hook is bypassable with `git commit --no-verify`.
test-claude-md-size:
	python3 scripts/check-claude-md-size.py --self-test
	python3 scripts/check-claude-md-size.py

# Test the install-hooks target: asserts it sets a relative, repo-local
# core.hooksPath (not --global, no .git/hooks symlink) and that a fresh
# git worktree resolves + fires the pre-commit hook from its own checkout.
test-install-hooks:
	scripts/git-hooks/tests/install-hooks.test

# Run the entrypoint CLAUDE_CMD construction tests. Extracts the
# CLAUDE_CMD-building shell block from container/entrypoint.sh by regex
# and exercises it in a fresh `bash -c` subshell against a matrix of
# CLAUDE_SHIM_SETTINGS_PATH + CLAUDE_AUTO_CONTINUE values. Guards against
# the v19 regression where the user-tier was loaded alongside the
# rewritten shim file (additive merge → bare cross-arch hooks still
# fired) AND the CLAUDE_AUTO_CONTINUE auto-resume integration. 12 cases,
# <1s.
#
# Also runs the container-PATH tests that assert
# /home/hndrewaall/.local/bin lives on the image PATH (Dockerfile ENV +
# entrypoint defensive prepend). Without these, Claude Code's
# native-install warning (`Native installation exists but ~/.local/bin
# is not in your PATH`) prints on every launch as soon as a self-update
# materialises ~/.local/bin/claude.
test-entrypoint:
	container/tests/entrypoint-claude-cmd.test
	container/tests/entrypoint-tmux-truecolor.test
	container/tests/container-path-includes-local-bin.test
	container/tests/baked-dirs.test
	container/tests/baked-obligations-hooks.test
	container/tests/config-dir-uid-1000.test
	container/tests/queue-gate-wired.test
	container/tests/claude-watch-alert-gate-wired.test
	container/tests/dispatch-gate-wired.test
	container/tests/event-must-act-wired.test
	container/tests/agent-comms-baked.test
	container/tests/compose-mount-modes.test
	container/tests/state-volume-default.test
	container/tests/process-compose-pid1.test
	container/tests/cron-default-baked.test
	container/tests/in-container-daemon.test
	container/tests/iproute2-installed.test
	container/tests/code-cli-installed.test
	container/tests/claude-event-tail-baked.test
	container/tests/cron-installed.test
	container/tests/entrypoint-launches-cron.test
	container/tests/redeploy-self-recreate.test
	container/tests/claude-event-queue-wired.test
	container/tests/claude-bin-symlink-uid.test
	container/tests/npm-install-as-uid-1000.test
	container/tests/xclip-shim.test
	SKIP_LIVE_CLAUDE=1 container/tests/skill-restart-discovery.test

# Run the cw host-shim tests (examples/compose/bin/cw — attaches a host
# terminal to the running claude-container's tmux session via
# `docker compose exec`). Uses the script's --print-cmd debug hook to
# verify argv construction without requiring docker. 7 cases, <1s.
test-cw:
	examples/compose/bin/tests/cw.test

# Run the mcp-host-bash host-shim tests (examples/compose/bin/mcp-host-bash —
# uvx mcp-proxy + uvx cli-mcp-server launcher that fronts a generic
# "run a bash command on the host" MCP server for the in-container claude
# via CLAUDE_MCP_HTTP_BRIDGE). Uses the script's --print-cmd debug hook to
# verify argv construction + default-policy floor + config-file overrides
# without requiring uvx / mcp-proxy / cli-mcp-server. 11 cases, <1s.
test-mcp-host-bash:
	examples/compose/bin/tests/mcp-host-bash.test

# Tests for examples/compose/bin/mcp-proxy-auth-shim — the bearer-token
# reverse proxy that fronts mcp-proxy. Spins up an in-process fake
# upstream + the shim as a subprocess, drives requests through urllib,
# and asserts the auth gate + header passthrough behavior. 14 cases,
# ~2s (each subprocess boot adds ~100ms; otherwise CPU-light).
test-mcp-proxy-auth-shim:
	examples/compose/bin/tests/mcp-proxy-auth-shim.test

# Tests for examples/compose/bin/install-host-deps — the static
# installer for mcp-proxy + cli-mcp-server. Exercises the uv → pip
# fallback path (TLS-only) by injecting a fake uv via PATH so we
# never actually fetch from PyPI. 10 cases, <1s.
test-install-host-deps:
	examples/compose/bin/tests/install-host-deps.test

# Tests for examples/compose/launchd/org.gbre.claude-watch.mcp-host-bash.plist
# — the macOS LaunchAgent template that persistently auto-starts
# mcp-host-bash on operator-login. File-level structural validation
# only (parses via stdlib plistlib + plutil-lint when available);
# does NOT exercise launchctl because the test runs on Linux CI.
# 21 cases, <1s.
test-launchd-plist:
	examples/compose/bin/tests/launchd-plist.test

# Tests for examples/compose/bin/load-bearer-from-keychain — the
# macOS-only Keychain wrapper that fetches the bearer from the user's
# login Keychain and exec's mcp-host-bash. Mocks the `security` CLI
# and mcp-host-bash via PATH override so the suite runs on Linux CI.
# Covers Keychain hit / miss / empty / non-macOS / unknown failure,
# plist-plaintext fallback, argv passthrough, secret-leak invariants,
# special-char round-trip, custom service-name. 12 cases, <1s.
test-load-bearer-from-keychain:
	examples/compose/bin/tests/load-bearer-from-keychain.test

# Tests for examples/personal-mac-mcp-host/personal-mcp-host.sh — the
# wrapper that spawns mcp-host-bash + the reverse SSH tunnel for the
# on-demand remote-access pattern. Uses --print-cmd to verify argv
# construction without invoking ssh / mcp-host-bash. Covers env-file
# loading, required-key enforcement, default ssh hardening options,
# PERSONAL_MCP_SSH_EXTRA passthrough, soft kill switch. 17 cases, <1s.
test-personal-mcp-host:
	examples/personal-mac-mcp-host/tests/personal-mcp-host.test

# Tests for examples/personal-mac-mcp-host/launchd/org.gbre.personal-mcp.host.plist
# — the macOS LaunchAgent template for on-demand bring-up of
# personal-mcp-host.sh. Structural validation only (plistlib + plutil
# when available); does NOT invoke launchctl. Covers
# RunAtLoad=false enforcement (this is the on-demand pattern, NOT
# auto-start), Label / paths / EnvironmentVariables shape, README
# walkthrough coverage. 22 cases, <1s.
test-personal-mcp-host-plist:
	examples/personal-mac-mcp-host/tests/launchd-plist.test

# Tests for examples/personal-mac-mcp-host/install.sh — the one-command
# LaunchAgent installer that auto-resolves REPO / HOME, substitutes the
# /PATH/TO/REPO and /PATH/TO/HOME placeholders, and copies the chosen
# plist into ~/Library/LaunchAgents/. Runs in --print-cmd / temp-HOME
# dry-run style; asserts the rendered plist has NO surviving /PATH/TO/
# placeholders and points at the resolved repo / home. Idempotency +
# missing-tunnel-plist guard covered. No launchctl. 21 checks, <1s.
test-personal-mcp-install:
	examples/personal-mac-mcp-host/tests/install.test

# Tests for examples/compose/ttyd/inject-autodark.py PASTE_EVENT_HANDLER_JS
# — the browser-side paste handler injected into ttyd's bundled
# index.html. The handler must:
#   - intercept Cmd+V / Ctrl+V when the clipboard contains an image
#     MIME (POST blob to /clipboard-upload + fire \x16), AND
#   - let text-only clipboards fall through to xterm.js's native paste
#     so Cmd+V works for BOTH images and text in one keybinding.
# Runs the JS body inside Node with DOM / clipboard / fetch stubs and
# asserts on preventDefault + side-effects across text-only / image-only
# / mixed / image-jpeg / empty-types synthetic paste events. 5 cases, <1s.
test-ttyd-paste-handler:
	python3 examples/compose/ttyd/tests/test_paste_handler.py

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
#   - pre-agent-background-required-hook    : PreToolUse hook (Agent matcher)
#   - pre-agent-worktree-isolation-hook     : PreToolUse hook (Agent matcher)
#   - worktree-create-hook                  : WorktreeCreate/Remove hook
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
	@ln -sfn $(abspath tools/hooks/pre-agent-background-required-hook) $(BIN_DIR)/pre-agent-background-required-hook
	@ln -sfn $(abspath tools/hooks/pre-agent-worktree-isolation-hook) $(BIN_DIR)/pre-agent-worktree-isolation-hook
	@ln -sfn $(abspath tools/hooks/worktree-create-hook) $(BIN_DIR)/worktree-create-hook
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
	@echo "  - pre-agent-background-required-hook (symlink -> tools/hooks/)"
	@echo "  - pre-agent-worktree-isolation-hook (symlink -> tools/hooks/)"
	@echo "  - worktree-create-hook      (symlink -> tools/hooks/)"
	@echo "  - agent-msg                 (symlink -> tools/agent-msg/)"
	@echo "  - agent-tail                (symlink -> tools/agent-tail/)"
	@echo "  - claude-event              (symlink -> tools/claude-event/)"
	@echo "  - claude-event-tail         (symlink -> tools/claude-event/)"
	@echo "  - claude-event-watch        (symlink -> tools/watchers/)"
	@echo "  - self-clear                (symlink -> tools/watchers/)"

# Install git pre-commit hook (warning-free build + unit/fixture tests).
# Points core.hooksPath at the tracked scripts/git-hooks/ dir instead of
# symlinking into .git/hooks/. Two reasons this is the correct form:
#   1. The setting is RELATIVE, so it resolves against each worktree's own
#      top-level — every worktree runs its own checked-out hooks.
#   2. git config lives in the shared common dir, so this auto-applies to
#      every existing AND future worktree of this repo. A symlink into
#      .git/hooks/ does NOT: linked worktrees have a private gitdir and
#      never consult the main repo's .git/hooks, so a fresh worktree
#      silently ran with no pre-commit gate.
# Scoped to THIS repo (local .git/config), NOT --global — other repos are
# untouched. Idempotent: re-running just re-asserts the same value.
install-hooks:
	@git config core.hooksPath scripts/git-hooks
	@echo "Pre-commit hook installed (core.hooksPath -> scripts/git-hooks; applies to all worktrees)."

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
#
# GIT_SHA build-arg flows to container/Dockerfile's `LABEL
# claude_watch_sha=...` so `docker inspect claude-container:dev --format
# '{{ index .Config.Labels "claude_watch_sha" }}'` reports which local
# revision was baked. `git rev-parse HEAD` is the working-tree HEAD;
# operators who want origin/main should `git pull --rebase` before
# invoking this target (the Dockerfile no longer pins a remote SHA — it
# COPYs the local working tree).
compose-build:
	@cd examples/compose && \
	  GIT_SHA="$$(git rev-parse HEAD 2>/dev/null || echo)" \
	  docker compose build \
	    --build-arg GIT_SHA="$$(git rev-parse HEAD 2>/dev/null || echo)"

# Build just the claude-container image directly (no compose). Same
# GIT_SHA plumbing as compose-build. Context is the repo root because the
# Dockerfile COPYs from sibling tools/ + container/ trees, and the
# claude-watch-builder stage COPYs the whole working tree to compile the
# Rust daemon.
container-build:
	docker build \
	  --build-arg GIT_SHA="$$(git rev-parse HEAD 2>/dev/null || echo)" \
	  -t claude-container:dev \
	  -f container/Dockerfile \
	  .

# Bring the integrated compose stack up in the foreground.
compose-up:
	@cd examples/compose && docker compose up

# Tear down the compose stack (volumes survive; add -v to nuke
# claude-container-versions).
compose-down:
	@cd examples/compose && docker compose down

# Redeploy the claude-container service (picks up new image / config).
#
# A SINGLE `docker compose up -d --force-recreate claude-container`.
# This is deliberately one host-daemon operation so the target works
# when issued FROM INSIDE the container (self-redeploy): the in-
# container docker CLI hands the recreate request to the HOST docker
# daemon, which performs the stop-old + start-new host-side and
# COMPLETES it even after the issuing container (and the shell that ran
# `make redeploy`) is torn down. The daemon owns the operation — no
# backgrounding, no nohup, no disown, no second `&& up -d` that would
# die with the issuing container.
#
# Why a single command and NOT a `rm -sf && up -d` split: when run from
# inside the container, the FIRST command (`rm -sf` / `down`) destroys
# the very container running the make recipe, so the shell dies and the
# `&& up -d` never executes — the container goes down and never comes
# back. `up -d --force-recreate` is atomic from the CLI's perspective:
# it issues ONE create+start request that the daemon carries to
# completion independently of the caller's lifetime.
#
# Why force-recreate no longer wedges (the bug #292 worked around):
# in-place recreate only ever stuck because a grandchild outlived PID
# 1's shutdown and pinned the netns + shared tmux-socket volume. The
# chief offender was crond — `sudo -n /usr/sbin/cron` FORKED a root
# cron that survived SIGKILL of the sudo wrapper. That is now fixed at
# the source: the Dockerfile sudoers carve-out disables pam_session +
# pam_setcred for the cron argv so sudo `execve()`s cron directly (no
# orphan), and cw-claude-watch-launch `exec`s claude-watch. With clean
# teardown, the old container fully releases the netns + named volumes
# before the fresh one starts, so `--force-recreate` succeeds every
# time. Named volumes survive (no -v), so claude state / versions / the
# tmux socket dir persist across the redeploy.
#
# Host-side init (prepare-host-claude-state) runs FIRST, mirroring
# `cw --up`: on macOS it bridges the Keychain Claude token into the
# dir-mounted ~/.claude/.credentials.json (fail-closed — a locked
# keychain aborts the redeploy so we never recreate into a logged-out
# container) and one-time-seeds the container-only ~/.claude.json.
# It is a clean no-op on Linux and when run from INSIDE the container
# (no `security` CLI), and it never tears down the running container,
# so the recipe shell survives to issue the atomic recreate below —
# the self-redeploy contract is preserved. Guarded by `-x` exactly as
# cw does, so a removed/relocated helper just skips the step.
redeploy:
	@cd examples/compose && \
	  if [ -x bin/prepare-host-claude-state ]; then ./bin/prepare-host-claude-state; fi && \
	  docker compose up -d --force-recreate claude-container

# Clean build artifacts
clean:
	cargo clean
