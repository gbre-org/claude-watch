# claude-watch Makefile.
#
# This repo has SIX deployment / build surfaces and the targets below are
# grouped accordingly. `make help` prints the index.
#
#   1. host + systemd  : install -> install-skills -> deploy-systemd
#                        (+ install-cron once, for the host cron fragment)
#   2. host $PATH CLIs : install (daemon copied, scripts symlinked)
#   3. container image : container-build / compose-build
#   4. compose stack   : bootstrap, compose-up/down, deploy-container
#   5. developer gates : install-hooks (git pre-commit), the test-* targets
#   6. macOS helpers   : install-mcp-host-bash-server
#
# `make` with no target still runs `test` (pinned below so it no longer
# depends on which target happens to appear first in this file).
.DEFAULT_GOAL := test

.PHONY: help clean
# Rust test targets
.PHONY: test test-verbose test-unit test-e2e test-live
# Python / shell tool test targets
.PHONY: test-session-task test-obligations-init test-queue-minisite test-hooks
.PHONY: test-agent-msg test-agent-tail test-claude-event test-event-must-act
.PHONY: test-pr-branches
.PHONY: test-self-clear test-self-login test-self-login-tmux test-watchers
.PHONY: test-dashboard test-trust-workspace
.PHONY: test-claude-tmux-env test-cron-toggle test-hooks-shim test-doc-links
.PHONY: test-claude-md-size test-install-hooks test-install-host-skills
.PHONY: test-install-host-cron test-ci-apt-install
.PHONY: test-entrypoint test-cw test-hostjob test-launchd-plist
.PHONY: test-personal-mcp-host test-personal-mcp-host-plist
.PHONY: test-personal-mcp-install test-ttyd-paste-handler test-ttyd-lock-toggle
# Build / install / host-deploy targets
.PHONY: build install install-hooks install-skills install-cron deploy deploy-systemd
.PHONY: install-mcp-host-bash-server
# Container / compose targets
.PHONY: bootstrap compose-build compose-up compose-down
.PHONY: container-build deploy-container redeploy sync-main-clone

# Print every annotated target, grouped by the `##@` section banners below.
help: ## Show this target index
	@awk 'BEGIN {FS = ":.*?## "} \
		/^##@ / {printf "\n%s\n", substr($$0, 5); next} \
		/^[a-zA-Z0-9_-]+:.*?## / {printf "  %-32s %s\n", $$1, $$2}' \
		$(MAKEFILE_LIST)

##@ Tests — Rust daemon

# Each of these prefers cargo-nextest (parallel) and falls back to plain
# `cargo test` when it isn't installed. The fallback is an if/else, NOT
# `nextest || cargo test`: that older form also fell through on a test
# FAILURE, which re-ran the whole suite under the other runner and — for
# the narrower fallbacks below — could turn a real nextest failure into a
# green exit. Mirrors the same if/else in scripts/git-hooks/pre-commit.

# Default: run all tests in parallel via nextest (preferred) or cargo test
test: ## Run the full Rust suite (nextest if available, else cargo test)
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run; \
	else \
		cargo test; \
	fi

test-verbose: ## Full Rust suite with stdout/stderr from passing tests
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run --no-capture; \
	else \
		cargo test -- --nocapture; \
	fi

test-unit: ## Unit + fixture tests only (fast)
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run -E 'not binary(~e2e_)'; \
	else \
		cargo test --lib --test unit_activity_detection; \
	fi

test-e2e: ## e2e tmux-based tests only
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run -E 'binary(~e2e_) and not test(~live)'; \
	else \
		cargo test --test 'e2e_*' -- --skip live; \
	fi

test-live: ## Live e2e tests (spawn real Claude Code, ~1-2 min each; #[ignore] by default)
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run --run-ignored=only; \
	else \
		cargo test -- --ignored; \
	fi

##@ Tests — Python / shell tooling

# Run the session-task Python tests (cross-session queue CLI under tools/).
# Self-contained: each test runs against a tempdir HOME so the live
# ~/.config/session/queue.json is never touched. The slowest suite here
# (order of a minute).
test-session-task: ## session-task queue CLI pytest suite
	uv run --python 3.11 --with pytest pytest tools/session-task/tests/ -v

# Run the obligations-init pytest suite (user-manifest idempotency).
# Self-contained: runs the real obligations-init + obligations CLIs against
# a tempdir HOME so the live ~/.config/claude/obligations.json is untouched.
test-obligations-init: ## obligations-init user-manifest idempotency pytest suite
	uv run --python 3.11 --with pytest pytest tools/obligations/tests/ -v

# Run the queue-minisite end-to-end suites (queue-minisite/test_*.py).
# Each file is a standalone unittest script that boots the Flask app
# in-process against a tempdir-rooted queue.json, so the live
# ~/.config/session/queue.json is never touched. They rewrite os.environ
# and drop `app` from sys.modules at class setup, so every file gets its
# OWN interpreter instead of sharing one pytest session.
#
# uv supplies flask (pinned to the version the container image installs)
# so no checked-in venv is needed. SESSION_TASK_BIN pins the CLI under
# test to THIS checkout, which keeps the suites correct in worktrees and
# renamed clones.
#
# The in-tree tools/ dirs go on PATH ahead of everything else because the
# app's force-start path reaches the obligations CLI through
# `shutil.which("obligations")` and NO-OPS SILENTLY when it is missing.
# Resolving it from the checkout means the suites test this tree's CLIs
# rather than whatever happens to be installed in the developer's ~/bin
# (and gives a machine with nothing installed the same result as CI).
test-queue-minisite: ## queue-minisite Flask end-to-end suites
	@set -e; \
	export PATH="$(CURDIR)/tools/session-task:$(CURDIR)/tools/obligations:$(CURDIR)/tools/claude-event:$$PATH"; \
	for f in queue-minisite/test_*.py; do \
		echo "==> $$f"; \
		SESSION_TASK_BIN="$(CURDIR)/tools/session-task/session-task" \
			uv run --python 3.12 --with flask==3.0.3 python "$$f"; \
	done

# Run the obligations / hooks Python tests. These are self-contained
# scripts (not pytest), so we just exec them directly. Each runs against
# an isolated $HOME tmpdir so the live obligations.json is never touched.
# The pre-agent-queue-gate-hook test exercises the real `session-task`
# binary; it must be on PATH (or installed via `make install`).
test-hooks: ## obligations + PreToolUse/PostToolUse hook suites
	python3 tools/obligations/shell_ast.py --test
	tools/hooks/tests/pre-tool-obligations-gate-hook.test
	tools/hooks/tests/pre-agent-queue-gate-hook.test
	tools/hooks/tests/pre-tool-claude-watch-alert-gate-hook.test
	tools/hooks/tests/user-prompt-claude-watch-alert-record-hook.test
	tools/hooks/tests/pre-tool-dispatch-gate-hook.test
	tools/hooks/tests/post-tool-agent-arm-hook.test
	tools/hooks/pre-agent-background-required-hook --test
	tools/hooks/pre-agent-worktree-isolation-hook --test
	tools/hooks/worktree-create-hook --test
	tools/claude-watch-ack/tests/claude-watch-ack.test
	tools/claude-watch-dispatch/tests/claude-watch-dispatch.test

# Run the agent-msg embedded test suite (CLI for delivering async
# messages to running Claude Code agents via the obligations gate).
# The script's `--test` flag runs every case in-process against
# isolated tmpdirs, no obligations side effects.
test-agent-msg: ## agent-msg embedded --test suite
	python3 tools/agent-msg/agent-msg --test

# Run the agent-tail embedded test suite (CLI for streaming agent
# JSONL transcripts). Tests cover pure helpers, format_record dispatch,
# resolution under a fake projects tree, and the follow-mode handler
# (truncation + rotation). All cases run in-process against tmpdirs.
test-agent-tail: ## agent-tail embedded --test suite
	python3 tools/agent-tail/agent-tail --test

test-claude-event: ## claude-event + claude-event-tail unit tests
	python3 tools/claude-event/tests/test_claude_event.py

# Run the pr-branches tests (PR branch lifecycle CLI under tools/).
# Fully offline: every GitHub/git accessor is stubbed out, so no test touches
# a real repository, remote, or the GitHub API. Covers each classification
# bucket, the squash-merge trap (ancestry must never decide merged-ness),
# worktree/default-branch precedence, and the delete path refusing when the
# live PR state disagrees with the classification.
test-pr-branches: ## pr-branches classification + merge-assertion unit tests
	python3 tools/pr-branches/tests/test_pr_branches.py

# Run the event-must-act toolchain tests. The toolchain (event-classify,
# event-ack, eval-event-must-act, user-prompt-ambient-inject-hook) lives in
# tools/event-must-act/ as the SHARED, host-portable copy: the container
# bakes them via COPY, and the non-container (systemd) host symlinks them
# into ~/bin. Three of the four carry embedded --self-test suites (the
# ambient-inject hook does not); this target runs those three plus the
# cron-driven dead-watcher recovery injector (cw-watcher-health-check),
# whose bash test stubs `claude-watch` so nothing is ever injected into a
# real pane.
#
# producer-tier-e2e.test covers what the per-tool self-tests structurally
# cannot: that a PRODUCER-shipped `data.tier` survives the WHOLE chain
# (event file -> claude-event-watch -> event-ack ingest -> event-classify ->
# pending/ambient). A unit test of classify() passes even when the watcher
# never reads data.tier, so only the e2e proves the path is wired. It
# sandboxes its own queue/state/lock/PATH and never touches a live watcher.
test-event-must-act: ## event-must-act toolchain self-tests
	python3 tools/event-must-act/event-classify --self-test
	python3 tools/event-must-act/event-ack --self-test
	python3 tools/event-must-act/eval-event-must-act --self-test
	tools/event-must-act/tests/cw-watcher-health-check.test
	tools/event-must-act/tests/producer-tier-e2e.test

# Run the self-clear config-only smoke tests (the full inject flow needs
# a live Claude Code tmux pane, which can't be reproduced in unit tests).
test-self-clear: ## self-clear config-only smoke tests
	python3 tools/watchers/tests/test_self_clear_config.py

# self-login pure predicates + config paths. No terminal needed.
test-self-login: ## self-login unit tests (pane predicates, code validation, config)
	python3 tools/watchers/tests/test_self_login.py

# self-login end-to-end against a THROWAWAY tmux session running a fake login
# screen — never a real Claude Code pane. Split from test-self-login because it
# needs both tmux and a built claude-watch binary; it self-skips without them,
# so it runs in the e2e CI job that has both rather than hiding a vacuous pass
# in the shell-test job.
test-self-login-tmux: ## self-login end-to-end against a real tmux pane
	tools/watchers/tests/test_self_login_tmux.sh

# Run the claude-event-watch fast-path smoke test.
test-watchers: test-self-clear test-self-login ## claude-event-watch fast-path + self-clear/self-login
	tools/watchers/tests/test_claude_event_watch.sh

# Run the dashboard parser tests (sources dashboard-lib.sh in a bash
# subshell and exercises conf_get / conf_windows / has_split / expected_panes
# against fixtures).
test-dashboard: ## dashboard-lib.sh parser tests
	tools/dashboard/tests/dashboard-parser.test

# Container-side ~/.claude state helpers, all in-process against tmpdir
# HOMEs:
#   - trust-workspace.py        : pre-seeds ~/.claude.json's
#     projects[<workspace>].hasTrustDialogAccepted, which suppresses the
#     in-container first-launch trust prompt.
#   - reconcile-native-claude   : reconciles the native-install state.
#   - snapshot-claude-config    : snapshots ~/.claude.json + symlink farm.
test-trust-workspace: ## container ~/.claude state helper suites
	python3 container/bin/trust-workspace.py --test
	python3 container/bin/reconcile-native-claude --test
	python3 container/bin/snapshot-claude-config --test

# Run the claude-tmux env / mount passthrough tests (corporate CA bundle
# forwarding, proxy passthrough, host hooks-dir bind-mount, set-but-missing
# path warnings). Exercises the wrapper's --print-docker-args debug hook so
# no docker daemon is needed.
test-claude-tmux-env: ## claude-tmux env / mount passthrough tests
	container/bin/tests/claude-tmux-env.test

# Run the cron-toggle tests (cw-cron-run flag-file exec-wrapper +
# cw-cron-toggle CLI). Uses a tempdir flag dir; no cron/root needed.
test-cron-toggle: ## cw-cron-run / cw-cron-toggle tests
	container/bin/tests/cw-cron-toggle.test

# Container hooks-shim suites. All run directly on Linux against synthetic
# inputs; no container needed:
#   - exec-hook                     : settings.json hook safe-exec wrapper for
#     cross-arch hooks — ELF passthrough, Mach-O / unknown / missing no-op,
#     dedup flag file.
#   - exec-hook-bridge              : the MCP-bridge half of the same wrapper.
#   - devbar-analytics-spool        : stdin-preserving spool + container->host
#     path rewrite for the `devbar ai-analytics capture` hook (approach B).
#   - generate-hooks-shim-settings  : container-local settings.json with every
#     hook command wrapped in /usr/local/bin/exec-hook.
#   - generate-project-mcp-json     : project-tier .mcp.json with MCP server
#     commands wrapped (the v21 follow-up fix).
test-hooks-shim: ## container hooks-shim / settings-rewrite suites
	container/hooks-shim/tests/exec-hook.test
	container/hooks-shim/tests/exec-hook-bridge.test
	container/hooks-shim/tests/devbar-analytics-spool.test
	container/hooks-shim/tests/generate-hooks-shim-settings.test
	container/hooks-shim/tests/generate-project-mcp-json.test

# No-broken-links gate for the docs baked into the container image. Runs the
# checker's embedded self-tests, then verifies every relative markdown link in
# container/baked-CLAUDE.md (and repo-wide) resolves to a path that exists in
# the repo. baked-CLAUDE.md now links to its sibling docs by RELATIVE path
# (they are COPYed into /opt/claude-container/ alongside it), so a link to an
# un-baked path is a real in-container 404 — this gate catches it at CI time.
test-doc-links: ## Gate: every relative markdown link resolves
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
test-claude-md-size: ## Gate: CLAUDE.md files stay under their size ceiling
	python3 scripts/check-claude-md-size.py --self-test
	python3 scripts/check-claude-md-size.py

# Test the install-hooks target: asserts it sets a relative, repo-local
# core.hooksPath (not --global, no .git/hooks symlink) and that a fresh
# git worktree resolves + fires the pre-commit hook from its own checkout.
test-install-hooks: ## Tests for the install-hooks target (core.hooksPath)
	scripts/git-hooks/tests/install-hooks.test

# Tests for scripts/install-host-skills.sh + its Makefile wiring: the
# skills/ (deployment-agnostic) vs container/skills/ (container-only) split,
# the `cw-` host prefix, absolute in-tree symlinks, idempotency, dry-run, the
# refuse-to-clobber rules for the operator-managed destination dir, and
# own-links-only pruning. Also guards the shipped skills against private-path
# leakage. Runs against throwaway tmpdirs — never touches ~/.claude.
test-install-host-skills: ## Tests for the host-skills installer + its wiring
	scripts/tests/install-host-skills.test

# Tests for cron.d/cw-host + scripts/install-host-cron.sh: that the shipped
# fragment is fully parameterized (no operator paths in a public repo), that
# every placeholder it uses is one the installer substitutes, that rendering
# resolves all of them, and the refuse-to-install guards. Also pins the
# single-binary-identity wiring the fragment depends on: deploy-systemd must
# depend on `install`, or the $BIN_DIR copy goes stale again. Tmpdirs only —
# never touches /etc/cron.d.
test-install-host-cron: ## Tests for the host-cron fragment + its installer
	scripts/tests/install-host-cron.test

# The bounded/retrying apt wrapper CI's two package-install steps run through.
# Drives the real script against a fake apt-get that hangs on demand and
# asserts on WALL-CLOCK behavior: a wedged attempt is aborted early, the retry
# recovers, and a permanent wedge still stops inside the total budget. A retry
# policy whose retries cannot fit inside the outer step cap is decoration, and
# a "resilient" wrapper that hangs anyway is worse than none — it reads as
# handled. Fakes only; never touches the real package manager.
test-ci-apt-install: ## Tests for the bounded/retrying CI apt installer
	scripts/tests/ci-apt-install.test

# The container image's static-assertion suite: ~30 scripts that check the
# entrypoint's runtime behaviour and that the Dockerfile actually baked what
# the deployment contract assumes (hooks wired, gates wired, dirs present,
# CLIs installed, volumes/pid1/cron shape). All run on plain Linux against
# the checked-in sources — no docker daemon, no built image.
#
# Two of them are worth calling out because they encode past regressions:
#
#   - entrypoint-claude-cmd.test extracts the CLAUDE_CMD-building shell
#     block from container/entrypoint.sh by regex and exercises it in a
#     fresh `bash -c` subshell across a matrix of CLAUDE_SHIM_SETTINGS_PATH
#     + CLAUDE_AUTO_CONTINUE values. It guards the v19 regression where the
#     user-tier was loaded alongside the rewritten shim file (additive merge
#     -> bare cross-arch hooks still fired), plus the CLAUDE_AUTO_CONTINUE
#     auto-resume integration.
#   - container-path-includes-local-bin.test asserts
#     /home/hndrewaall/.local/bin is on the image PATH (Dockerfile ENV +
#     entrypoint defensive prepend). Without it, Claude Code's native-install
#     warning (`Native installation exists but ~/.local/bin is not in your
#     PATH`) prints on every launch as soon as a self-update materialises
#     ~/.local/bin/claude.
test-entrypoint: ## Container entrypoint + baked-image assertion suites
	container/tests/entrypoint-claude-cmd.test
	container/tests/entrypoint-tmux-truecolor.test
	container/tests/container-path-includes-local-bin.test
	container/tests/baked-dirs.test
	container/tests/baked-obligations-hooks.test
	container/tests/baked-mcp-bridge-baseline.test
	container/tests/config-dir-uid-1000.test
	container/tests/neutralize-home-claude-oauth.test
	container/tests/claude-code-dir-uid-1000.test
	container/tests/queue-gate-wired.test
	container/tests/claude-watch-alert-gate-wired.test
	container/tests/dispatch-gate-wired.test
	container/tests/event-must-act-wired.test
	container/tests/user-obligation-manifest-wired.test
	container/tests/agent-ack-mainloop-scope.test
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
	container/tests/native-install-versions-volume.test
	container/tests/xclip-shim.test
	SKIP_LIVE_CLAUDE=1 container/tests/skill-restart-discovery.test

# Run the cw host-shim tests (examples/compose/bin/cw — attaches a host
# terminal to the running claude-container's tmux session via
# `docker compose exec`). Uses the script's --print-cmd debug hook to
# verify argv construction without requiring docker.
test-cw: ## cw host-shim argv-construction tests
	examples/compose/bin/tests/cw.test

# Run the hostjob tests (examples/compose/bin/hostjob — the detached
# host-job runner that lets an in-container agent launch host commands
# past the 30s host-bash MCP cap, then poll/wait for them). Exercises the
# real run/wait/poll/list/clean surface against a throwaway $HOME so no
# operator state is touched. The legacy hostjob.test covers the core
# run/wait/poll/list/clean surface; the pytest files cover the
# queue-integration, stop-subcommand, and live-tail-broker features.
test-hostjob: ## hostjob detached host-job runner tests
	examples/compose/bin/tests/hostjob.test
	uv run --python 3.11 --with pytest pytest -v \
		examples/compose/bin/tests/test_hostjob_broker.py \
		examples/compose/bin/tests/test_hostjob_queue.py \
		examples/compose/bin/tests/test_hostjob_stop.py

# Tests for examples/compose/launchd/org.gbre.claude-watch.mcp-host-bash.plist
# — the macOS LaunchAgent template that persistently auto-starts
# mcp-host-bash on operator-login. File-level structural validation
# only (parses via stdlib plistlib + plutil-lint when available);
# does NOT exercise launchctl because the test runs on Linux CI.
test-launchd-plist: ## mcp-host-bash LaunchAgent plist structure tests
	examples/compose/bin/tests/launchd-plist.test

# Tests for examples/personal-mac-mcp-host/personal-mcp-host.sh — the
# wrapper that spawns mcp-host-bash + the reverse SSH tunnel for the
# on-demand remote-access pattern. Uses --print-cmd to verify argv
# construction without invoking ssh / mcp-host-bash. Covers env-file
# loading, required-key enforcement, default ssh hardening options,
# PERSONAL_MCP_SSH_EXTRA passthrough, soft kill switch, and the
# `restart` verb (reap a wedged half-up stack, restart both pieces,
# verify each, exit 4 naming whichever did not come back; launchd
# units are driven with kickstart -k via a stand-in launchctl).
# Slower than the other shell suites — the restart cases start real
# stand-in processes.
test-personal-mcp-host: ## personal-mcp-host.sh wrapper tests
	examples/personal-mac-mcp-host/tests/personal-mcp-host.test

# Tests for examples/personal-mac-mcp-host/launchd/org.gbre.personal-mcp.host.plist
# — the macOS LaunchAgent template for on-demand bring-up of
# personal-mcp-host.sh. Structural validation only (plistlib + plutil
# when available); does NOT invoke launchctl. Covers
# RunAtLoad=false enforcement (this is the on-demand pattern, NOT
# auto-start), Label / paths / EnvironmentVariables shape, the per-mode
# ProgramArguments flags (bundled passes --enable, tunnel-only passes
# --tunnel-only), README walkthrough coverage.
test-personal-mcp-host-plist: ## personal-mcp-host LaunchAgent plist tests
	examples/personal-mac-mcp-host/tests/launchd-plist.test

# Tests for examples/personal-mac-mcp-host/install.sh — the one-command
# LaunchAgent installer that auto-resolves REPO / HOME, substitutes the
# /PATH/TO/REPO and /PATH/TO/HOME placeholders, and copies the chosen
# plist into ~/Library/LaunchAgents/. Runs in --print-cmd / temp-HOME
# dry-run style; asserts the rendered plist has NO surviving /PATH/TO/
# placeholders and points at the resolved repo / home. Idempotency +
# missing-tunnel-plist guard covered. No launchctl.
test-personal-mcp-install: ## personal-mcp-host install.sh dry-run tests
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
# / mixed / image-jpeg / empty-types synthetic paste events.
test-ttyd-paste-handler: ## ttyd browser paste-handler JS tests
	python3 examples/compose/ttyd/tests/test_paste_handler.py

# Tests for examples/compose/ttyd/inject-autodark.py LOCK_TOGGLE_JS — the
# subtle top-right padlock toggle injected into ttyd's bundled index.html.
# When ACTIVE it suppresses keystrokes by returning false from xterm.js's
# attachCustomKeyEventHandler hook (nothing reaches the PTY / input WS) and
# flips window.__cwTerminalLocked so the paste handler also blocks paste.
# Runs the JS body inside Node with DOM / window.term stubs and asserts the
# button state (glyph, aria-pressed, cw-locked class) + key-veto return
# value across the unlocked → locked → unlocked toggle cycle.
test-ttyd-lock-toggle: ## ttyd browser lock-toggle JS tests
	python3 examples/compose/ttyd/tests/test_lock_toggle.py

##@ Build, install, host/systemd deploy

# Release build
build: ## cargo build --release
	cargo build --release

# Install this repo's deployment-agnostic skills (skills/*.md) into the host
# Claude Code commands dir as `cw-`-prefixed slash commands (`/cw-<name>`).
#
# The prefix is the HOST analogue of the container's `claude-container` plugin
# namespace: in either deployment mode, a skill that came from this repo is
# visibly namespaced. Claude Code derives a user-level command's name from the
# FILENAME, so a filename prefix needs no plugin manifest and no change to how
# the operator launches `claude` — which matters, because on a host deploy this
# Makefile does not control the `claude` argv.
#
# Container-only skills (the ones that recreate / restart the container, edit
# its bind-mounts, or reference in-container-only paths) deliberately stay in
# container/skills/ and are NEVER installed here — they cannot work on a
# non-container host.
#
# Deliberately conservative: the destination is normally managed by a separate,
# operator-private config repo full of hand-written skills, so the installer
# owns only the paths it created. It never overwrites a regular file or a
# symlink pointing outside skills/, and only prunes its own dangling links.
# Idempotent; `-n` for a dry run. Override the destination with
# CLAUDE_COMMANDS_DIR (default ~/.claude/commands).
install-skills: ## Install skills/ as /cw-<name> host slash commands
	@scripts/install-host-skills.sh

# Render + install the HOST cron fragment (cron.d/cw-host) into /etc/cron.d.
#
# The container bakes its crontab into the image (container/cron.d/cw-default);
# this is the host equivalent. It can't be baked, because a host deployment has
# no fixed install prefix — the fragment therefore ships @PLACEHOLDER@s and the
# installer fills them in from the local checkout (binary path, user, home,
# state dir), each overridable. That also keeps one operator's home path out of
# this public repo, and lets a second deployment consume the same fragment.
#
# Deliberately NOT a dependency of deploy-systemd: it needs root, and a deploy
# must not silently rewrite the host's crontab. Run it once at setup (and again
# only when the fragment changes); cron re-reads /etc/cron.d on the next tick,
# so there is nothing to restart. `-n` for a dry run that prints the rendered
# file and writes nothing; `--help` for all flags.
install-cron: ## Render + install cron.d/cw-host into /etc/cron.d (needs root)
	@scripts/install-host-cron.sh

# Build + restart systemd service (HOST / systemd install — NOT used in the
# Docker-container setup; see `deploy-container` for that).
#
# Depends on install-skills so a host deploy always re-asserts the repo's
# skills — otherwise a skill added here is invisible on the host until someone
# remembers to hand-install it (exactly how /distill shipped for weeks as a
# container-only command).
#
# Depends on `install` (which itself depends on `build`) for the SAME reason,
# one layer down: a host/systemd deployment has TWO claude-watch binaries on
# disk — the service's ExecStart runs target/release/ directly, while the
# on-PATH CLI is the $(BIN_DIR) copy that `install` places. This target used to
# depend on `build` alone, so a deploy rebuilt + restarted the service and left
# the $(BIN_DIR) copy frozen at whenever `make install` last ran. The two then
# drifted, silently: `claude-watch <subcommand>` on PATH kept running old code,
# and any cron/tooling pointed at the copy reported that copy's compiled-in
# build identity rather than the running daemon's. Depending on `install` makes
# one deploy refresh both from the same build. Do NOT instead symlink
# $(BIN_DIR)/claude-watch into target/release/ — see the install policy below.
deploy-systemd: install install-skills ## Host/systemd deploy: build + install + skills + restart
	sudo systemctl restart claude-watch

# DEPRECATED alias — kept so any docs / muscle-memory invoking `make deploy`
# keep working. Prefer `make deploy-systemd` (self-documenting). No recipe body;
# just depends on the renamed target.
#
# Deliberately carries NO `##` help annotation: deprecated aliases stay out of
# `make help` so the index only advertises the current names. It also keeps the
# line in the bare `alias: target` form that container/tests/ asserts on for the
# sibling `redeploy` alias — do not append a trailing comment here.
deploy: deploy-systemd

# Install built binaries + scripts onto $PATH ($BIN_DIR, default ~/bin).
# The recipe below is the authoritative list of what lands there — it used
# to be restated in a comment here too, which had drifted to about half the
# real set.
#
# Install policy:
#   - The claude-watch Rust daemon is a build artifact, so it's a real
#     file copy from target/release/ into $(BIN_DIR). Re-running `make
#     install` after `make build` refreshes it.
#
#     WHY A COPY AND NOT A SYMLINK (this has been "fixed" the wrong way
#     before): a symlink into target/release/ dangles the moment anyone
#     runs `cargo clean` or switches profile, which makes the on-PATH CLI
#     vanish rather than merely go stale — and `make install` would put
#     the copy back on the next run anyway. The copy is right; what was
#     wrong is that it used to go stale, because `deploy-systemd` did not
#     depend on this target. It now does, so a host deploy refreshes this
#     copy and restarts the service from the same build.
#   - Every other tool is a script (Python / shell). Those install as
#     ABSOLUTE-PATH symlinks back to the source under tools/, so editing
#     a script in-tree is immediately reflected in $(BIN_DIR) without
#     another `make install` round-trip. `ln -sfn` makes the operation
#     idempotent (overwrites existing files / stale symlinks; -n
#     prevents following a directory at the link path).
BIN_DIR ?= $(HOME)/bin

install: build ## Install daemon (copy) + tool scripts (symlinks) into $BIN_DIR
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
	@ln -sfn $(abspath tools/pr-branches/pr-branches) $(BIN_DIR)/pr-branches
	@ln -sfn $(abspath tools/claude-event/claude-event) $(BIN_DIR)/claude-event
	@ln -sfn $(abspath tools/claude-event/claude-event-tail) $(BIN_DIR)/claude-event-tail
	@ln -sfn $(abspath tools/watchers/claude-event-watch) $(BIN_DIR)/claude-event-watch
	@ln -sfn $(abspath tools/watchers/self-clear) $(BIN_DIR)/self-clear
	@ln -sfn $(abspath tools/watchers/self-login) $(BIN_DIR)/self-login
	@ln -sfn $(abspath tools/obligations/obligations-init) $(BIN_DIR)/obligations-init
	@ln -sfn $(abspath tools/event-must-act/event-classify) $(BIN_DIR)/event-classify
	@ln -sfn $(abspath tools/event-must-act/event-ack) $(BIN_DIR)/event-ack
	@ln -sfn $(abspath tools/event-must-act/eval-event-must-act) $(BIN_DIR)/eval-event-must-act
	@ln -sfn $(abspath tools/event-must-act/heartbeat-ack) $(BIN_DIR)/heartbeat-ack
	@ln -sfn $(abspath tools/event-must-act/user-prompt-ambient-inject-hook) $(BIN_DIR)/user-prompt-ambient-inject-hook
	@ln -sfn $(abspath tools/event-must-act/cw-watcher-health-check) $(BIN_DIR)/cw-watcher-health-check
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
	@echo "  - pr-branches               (symlink -> tools/pr-branches/)"
	@echo "  - claude-event              (symlink -> tools/claude-event/)"
	@echo "  - claude-event-tail         (symlink -> tools/claude-event/)"
	@echo "  - claude-event-watch        (symlink -> tools/watchers/)"
	@echo "  - self-clear                (symlink -> tools/watchers/)"
	@echo "  - self-login                (symlink -> tools/watchers/)"
	@echo "  - obligations-init          (symlink -> tools/obligations/)"
	@echo "  - event-classify            (symlink -> tools/event-must-act/)"
	@echo "  - event-ack                 (symlink -> tools/event-must-act/)"
	@echo "  - eval-event-must-act       (symlink -> tools/event-must-act/)"
	@echo "  - heartbeat-ack             (symlink -> tools/event-must-act/)"
	@echo "  - user-prompt-ambient-inject-hook (symlink -> tools/event-must-act/)"
	@echo "  - cw-watcher-health-check   (symlink -> tools/event-must-act/)"

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
install-hooks: ## Point core.hooksPath at scripts/git-hooks (pre-commit gate)
	@git config core.hooksPath scripts/git-hooks
	@echo "Pre-commit hook installed (core.hooksPath -> scripts/git-hooks; applies to all worktrees)."

# Build + install the host-side host-bash MCP server
# (crates/mcp-host-bash-server) to ~/bin, re-signing on macOS. This is the
# single-process replacement for the old mcp-host-bash launcher + mcp-proxy +
# cli-mcp-server + mcp-proxy-auth-shim chain. See that crate's Makefile.
install-mcp-host-bash-server: ## Build + install the host-bash MCP server to ~/bin
	$(MAKE) -C crates/mcp-host-bash-server install

##@ Container image + compose stack

# --- examples/compose targets -----------------------------------------
# Convenience wrappers around the integrated docker-compose example at
# examples/compose/. The compose file wires claude-container +
# queue-minisite + eichi-search; see examples/compose/README.md for
# prerequisites (Docker, ANTHROPIC_API_KEY, sibling eichi clone).

# Run the bootstrap helper that checks prereqs, clones eichi sibling,
# and seeds examples/compose/.env from .env.example.
bootstrap: ## Check compose prereqs + seed examples/compose/.env
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
# Host-computed build identity passed to container/Dockerfile's
# claude-watch-builder stage (CW_BUILD_COMMIT / CW_BUILD_PR), which build.rs
# bakes into the `claude_watch_build_info` Prometheus gauge. The Docker build
# context prunes `.git/` (.dockerignore), so build.rs cannot run git inside
# the image — we resolve commit + PR HERE on the host (git available) and feed
# them in. CW_BUILD_PR parses the trailing `(#N)` squash-merge convention from
# the HEAD subject (empty if none — matches build.rs's "" fallback).
CW_BUILD_COMMIT = $(shell git rev-parse --short HEAD 2>/dev/null)
CW_BUILD_PR = $(shell git log -1 --format=%s 2>/dev/null | grep -oE '\#[0-9]+' | tail -1 | tr -d '\#')

compose-build: ## Build all compose-stack images
	@cd examples/compose && \
	  DOCKER_BUILDKIT=1 COMPOSE_DOCKER_CLI_BUILD=1 \
	  GIT_SHA="$$(git rev-parse HEAD 2>/dev/null || echo)" \
	  docker compose build \
	    --build-arg GIT_SHA="$$(git rev-parse HEAD 2>/dev/null || echo)" \
	    --build-arg CW_BUILD_COMMIT="$(CW_BUILD_COMMIT)" \
	    --build-arg CW_BUILD_PR="$(CW_BUILD_PR)"

# Build just the claude-container image directly (no compose). Same
# GIT_SHA plumbing as compose-build. Context is the repo root because the
# Dockerfile COPYs from sibling tools/ + container/ trees, and the
# claude-watch-builder stage COPYs the whole working tree to compile the
# Rust daemon.
container-build: ## Build just the claude-container:dev image
	DOCKER_BUILDKIT=1 docker build \
	  --build-arg GIT_SHA="$$(git rev-parse HEAD 2>/dev/null || echo)" \
	  --build-arg CW_BUILD_COMMIT="$(CW_BUILD_COMMIT)" \
	  --build-arg CW_BUILD_PR="$(CW_BUILD_PR)" \
	  -t claude-container:dev \
	  -f container/Dockerfile \
	  .

# Bring the integrated compose stack up in the foreground.
compose-up: ## Bring the compose stack up in the foreground
	@cd examples/compose && docker compose up

# Tear down the compose stack (volumes survive; add -v to nuke
# claude-container-versions).
compose-down: ## Tear the compose stack down (volumes survive)
	@cd examples/compose && docker compose down

# Deploy/recreate the claude-container service (picks up new image / config).
# This is the ONLY correct deploy for the Docker-container setup (the host/
# systemd variant is `deploy-systemd`). `make redeploy` remains a working
# DEPRECATED alias of this target.
#
# TWO ordered compose ops, FIRST of which is the atomic self-redeploy op:
#   1. `docker compose up -d --force-recreate claude-container`  (atomic)
#   2. `docker compose up -d`  (no service arg — fill in the rest of the
#      stack: ttyd, queue-minisite, eichi-search — idempotently, WITHOUT
#      --force-recreate so the just-recreated claude-container is left
#      running untouched).
#
# Command 1 is deliberately ONE host-daemon operation so the target works
# when issued FROM INSIDE the container (self-redeploy): the in-container
# docker CLI hands the recreate request to the HOST docker daemon, which
# performs the stop-old + start-new host-side and COMPLETES it even after
# the issuing container (and the shell that ran `make redeploy`) is torn
# down. The daemon owns that op — no backgrounding, no nohup, no disown.
#
# The claude-container recreate is ordered FIRST precisely so it never
# DEPENDS on a following command. On self-redeploy the recreate tears down
# the issuing container, the recipe shell dies, and the trailing `&& up -d`
# (command 2) simply never runs — which is fine: self-redeploy always runs
# on an already-up system where the siblings are ALREADY running, so the
# claude-container recreate is the only thing it needs. On a HOST cold
# start (docker-autostart after a Docker Desktop restart, everything down),
# the recipe shell runs host-side and SURVIVES the recreate, so command 2
# executes and brings the whole stack up. This is the coverage fix for the
# 'siblings missing after Docker Desktop restart' bug (ttyd / minisite /
# eichi-search never came up because deploy-container only touched
# claude-container). The trailing `up -d` is idempotent: on a normal
# already-up host it no-ops.
#
# Why command 1 is a single op and NOT a `rm -sf && up -d` split: when run
# from inside the container, a FIRST `rm -sf` / `down` destroys the very
# container running the make recipe, so the shell dies and the `&& up -d`
# never executes — the container goes down and never comes back.
# `up -d --force-recreate` is atomic from the CLI's perspective: it issues
# ONE create+start request the daemon carries to completion independently
# of the caller's lifetime.
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
# COMPOSE_FILE wiring (q-2026-06-22-a072): the operator's personal
# bind-mounts (gh token, gitconfig, ssh-agent, Dropbox, ci-logs, the
# clipboard bridge, etc.) live in an override that is intentionally
# OUT of the git tree, in the stable config dir
# `$(HOME)/.config/claude-container/docker-compose.override.yml`.
#
# Docker only AUTO-merges a sibling file literally named
# `docker-compose.override.yml` in the project dir. Because the override
# is gitignored it never exists in a worktree, so a redeploy run from the
# build worktree `~/repos/.worktrees/claude-watch/main` (per the workflow
# convention) silently merged ZERO override and recreated the container
# with NONE of those mounts — the recurring "clipboard mount missing
# after recreate" bug. Moving the override to the config dir + pointing
# COMPOSE_FILE at it makes the merge LOCATION-INDEPENDENT: the base is
# this clone's own compose, the override is always the config-dir file,
# regardless of which clone/worktree issues the redeploy.
#
# The override is appended only if it exists, so a fresh host with no
# personal override still recreates cleanly (base-only).
COMPOSE_BASE := $(CURDIR)/examples/compose/docker-compose.yml
COMPOSE_OVERRIDE := $(HOME)/.config/claude-container/docker-compose.override.yml

# DEPLOY_ENV_FILE wiring (same worktree-invisibility class of bug as the
# COMPOSE_OVERRIDE above): docker compose auto-loads `.env` from the
# project dir (examples/compose/), but `.env` is gitignored — it exists in
# the main clone's examples/compose/.env but NOT in a worktree's. Since the
# workflow convention deploys from the build worktree
# `~/repos/.worktrees/claude-watch/main`, that auto-loaded `.env` is EMPTY,
# so deploy-critical vars (notably CLAUDE_HOST_MANAGED_SETTINGS_DIR) default
# to their fallbacks (e.g. the managed-settings dir → /dev/null mount).
#
# Fix: keep the deploy-critical vars in a LOCATION-INDEPENDENT config-dir
# env-file and pass it explicitly with `--env-file`, mirroring how
# COMPOSE_FILE resolves the override from the config dir. Operators migrate
# the relevant vars into this file once (see the PR / README note); a host
# without it still recreates cleanly (compose falls back to the project
# `.env` if present, else the in-file defaults).
DEPLOY_ENV_FILE := $(HOME)/.config/claude-container/deploy.env

# Sync the BIND-MOUNT SOURCE clone to origin/main (ff-only) — the activation
# step for bind-mounted Python-CLI fixes. The compose bind-mount `${HOME}/repos/`
# mounts the operator's MAIN CLONE (below), NOT whatever worktree built the
# image, and the in-container obligations/session-task CLIs (+ tools/obligations/*)
# resolve from it via PATH BEFORE the baked /usr/local/bin copy. So a stale main
# clone SHADOWS merged+baked CLI fixes and `make deploy-container` alone won't
# activate them (the recreate re-mounts the same stale clone). Run this before
# deploying. Deliberately NOT a dependency of deploy-container — a target that
# mutated the operator's working clone on every deploy is a bigger decision
# (it could ff-fail on local commits); keep it explicit + opt-in. `--ff-only`
# is safe: it refuses (non-zero) rather than clobbering divergent local work.
CW_MAIN_CLONE := $(HOME)/repos/claude-watch
sync-main-clone: ## ff-only sync the bind-mount source clone to origin/main
	@echo "Syncing bind-mount source clone $(CW_MAIN_CLONE) to origin/main (ff-only)..."
	@git -C "$(CW_MAIN_CLONE)" fetch origin
	@git -C "$(CW_MAIN_CLONE)" merge --ff-only origin/main
	@echo "Now at: $$(git -C "$(CW_MAIN_CLONE)" log -1 --oneline)"

deploy-container: container-build ## Container deploy: force-recreate claude-container, then up -d the rest of the stack
	@cd examples/compose && \
	  if [ -x bin/prepare-host-claude-state ]; then ./bin/prepare-host-claude-state; fi && \
	  env_flag=""; \
	  if [ -f "$(DEPLOY_ENV_FILE)" ]; then env_flag="--env-file $(DEPLOY_ENV_FILE)"; fi; \
	  export CW_BUILD_COMMIT="$(CW_BUILD_COMMIT)"; \
	  export CW_BUILD_PR="$(CW_BUILD_PR)"; \
	  export GIT_SHA="$$(git rev-parse HEAD 2>/dev/null || echo)"; \
	  if [ -f "$(COMPOSE_OVERRIDE)" ]; then export COMPOSE_FILE="$(COMPOSE_BASE):$(COMPOSE_OVERRIDE)"; fi; \
	  docker compose $$env_flag up -d --force-recreate claude-container && \
	  docker compose $$env_flag up -d

# DEPRECATED alias — kept so the baked image's own scripts/docs (entrypoint,
# cwsr, container/tests/redeploy-self-recreate.test, baked-CLAUDE.md) and the
# self-redeploy contract keep working until an image rebuild bakes the new name
# `deploy-container` everywhere. `make redeploy` MUST keep working. No recipe
# body; just depends on the renamed target.
#
# Deliberately carries NO `##` help annotation, and MUST stay in the bare
# `redeploy: deploy-container` form: container/tests/redeploy-self-recreate.test
# asserts on that exact line (regex `^redeploy:\s*deploy-container\s*$`), so
# even a trailing comment fails the self-redeploy contract gate.
redeploy: deploy-container

##@ Housekeeping

# Clean build artifacts
clean: ## cargo clean
	cargo clean
