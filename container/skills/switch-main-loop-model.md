---
name: switch-main-loop-model
description: "Switch the in-container MAIN-LOOP Claude Code model (e.g. to opus-5-5) by editing the user-tier settings.json `model` field, then cwsr to roll the inner process onto it — plus when a daemon rebuild (make deploy-container) is ALSO required"
---
Switch the MODEL the in-container **main-loop** `claude` runs on (e.g. move the dispatcher from `opus-4-8` to `opus-5-5`). The model is NOT set by any `--model` flag, env var (`ANTHROPIC_MODEL` / `CLAUDE_CODE_MODEL`), or cwsr argument — it is read from the **`model` field of Claude Code's user-tier `settings.json`**. Switching the model is therefore a two-step operation: **edit the `model` field, then roll the inner process with `cwsr`** so the new value is read at launch. This skill does NOT change any subagent model (subagents default to Sonnet per operator directive — that is a separate, per-Agent-call knob).

**Confirmed mechanism (investigated, not guessed):**
- `cwsr` reconstructs the claude argv from `entrypoint.sh`'s env vars (`CLAUDE_SHIM_SETTINGS_PATH`, `CLAUDE_AUTO_CONTINUE`, plugin dir, setting-sources). It appends **no `--model`**. Verified: `cwsr --print`.
- There is **no `ANTHROPIC_MODEL` / `CLAUDE_CODE_MODEL`** in `compose.yml`, the override, or `entrypoint.sh`. The launch also carries no `--model`.
- The `--settings` **shim** (`/run/claude/shim/settings.json`) has `model: null` — it is NOT where the model lives.
- The running model comes from the **user tier**. This container launches `--setting-sources user,local` (the sanitized-snapshot-farm branch, `CLAUDE_SHIM_SANITIZED_USER_FARM=1`), so the user tier is the snapshot farm at `$CLAUDE_CONFIG_DIR/settings.json` (`/run/claude/config/settings.json`, on tmpfs). `entrypoint.sh` regenerates that farm copy from `~/.claude/settings.json` via `generate-hooks-shim-settings`, which passes the `model` field through untouched.
- `~/.claude/settings.json` is a symlink to the **rw host bind-mount** (`/run/host-claude/settings.json` ← host `~/.claude/settings.json`). That host file is the DURABLE source of truth; the tmpfs farm copy is the LIVE value the running process reads.

## The `model` field — exact path + format

- **Field:** top-level `"model"` in `settings.json`.
- **Value:** a model id string, e.g. `"us.anthropic.claude-opus-4-8[1m]"` (the `[1m]` suffix selects the 1M-context variant). Set it to whatever id the target model uses, e.g. `"us.anthropic.claude-opus-5-5[1m]"`. If unsure of the exact id string, check the SF gateway first (`/check-new-claude`) — do NOT guess a model id.
- **Two files matter:**
  - `~/.claude/settings.json` (→ host `~/.claude/settings.json`) — **DURABLE**. Survives container recreate. Edit this so the switch is not lost on the next `make deploy-container`.
  - `$CLAUDE_CONFIG_DIR/settings.json` (`/run/claude/config/settings.json`) — **LIVE user tier** the respawned `claude` actually reads. On tmpfs; wiped + regenerated from the host file on every entrypoint run.

**Optional — `modelOverrides`:** the same `settings.json` may carry a `modelOverrides` map (short alias → full gateway id). It only affects short aliases; setting the top-level `model` to a full `us.anthropic.…` id does not require an override entry.

## Mode A — switch model ONLY (fast, keeps the container)

Use when you just want the main loop on a different model and no daemon/image change is involved.

1. **Pick the model id** (verify via `/check-new-claude` if unsure; do not guess).
2. **Edit BOTH settings files' `model` field** — the durable host file AND the live farm copy — so the switch is both applied now AND survives a later recreate. From inside the container both are writable (`~/.claude/settings.json` is an rw bind-mount symlink; the farm copy is tmpfs). Use `jq` to edit in place safely, e.g.:
   ```sh
   NEW='us.anthropic.claude-opus-5-5[1m]'
   for f in "$HOME/.claude/settings.json" "$CLAUDE_CONFIG_DIR/settings.json"; do
     tmp="$(mktemp)"; jq --arg m "$NEW" '.model=$m' "$f" > "$tmp" && cat "$tmp" > "$f" && rm -f "$tmp"
   done
   ```
   (Write the tmp then `cat >` back into the original so the bind-mount symlink target and tmpfs file are updated in place, not replaced.)
3. **Roll the inner process WITHOUT a binary upgrade:** `cwsr --no-upgrade`. This re-execs `claude` in pane 0 so it re-reads the user tier and comes up on the new model, without a `claude install`. (Plain `cwsr` also works but needlessly reinstalls the binary.)
   - **This kills and restarts the current main-loop session** (context is preserved via `CLAUDE_AUTO_CONTINUE` resume, same as any cwsr roll). Save state first: commit/push in-flight work, update the log, `session-task set`.
4. **Verify** (see Verification below).

`cwsr` does NOT regenerate the farm copy — that is why step 2 edits the live farm file directly. If you edit only the host file and `cwsr`, the respawn reads the STALE farm copy and the model does NOT change until the next full entrypoint run.

## Mode B — switch model + activate a DAEMON rebuild

Use when the model switch rides along with merged **claude-watch daemon** changes (the Rust binary), OR when you would rather let the entrypoint regenerate the farm from the host file than hand-edit the tmpfs copy.

**Why cwsr is not enough here:** `cwsr` rolls the inner `claude` binary + context but does **NOT** rebuild or restart the Rust claude-watch daemon, and does NOT re-run `entrypoint.sh`. Merged daemon fixes only take effect on an image rebuild + force-recreate.

1. **Edit the DURABLE host file's `model` field only** — `~/.claude/settings.json` (the rw bind-mount). You do NOT need to touch the tmpfs farm copy: the force-recreate re-runs `entrypoint.sh`, which regenerates the farm from the host file, so the new `model` is picked up automatically.
   ```sh
   NEW='us.anthropic.claude-opus-5-5[1m]'
   tmp="$(mktemp)"; jq --arg m "$NEW" '.model=$m' "$HOME/.claude/settings.json" > "$tmp" \
     && cat "$tmp" > "$HOME/.claude/settings.json" && rm -f "$tmp"
   ```
2. **Redeploy via the deploy-container skill** — `/claude-container:deploy-container` (runs `make deploy-container` = a single `docker compose up -d --force-recreate claude-container` through host-bash). This rebuilds/relaunches with the new image AND regenerates the farm from the host settings, so the daemon change AND the model switch both land. Save state first (commit/push, log, `session-task set`) — force-recreate kills this session; the next session resumes via the entrypoint's `CLAUDE_AUTO_CONTINUE` + claude-watch resume injection.
   - If the daemon change requires a fresh **image** (not just a recreate), the deploy-container skill covers the build-worktree + main-clone-sync sequence.

## Verification (confirm the RUNNING model after)

The reliable signal is what the running process reads at launch:

1. **User-tier value is what you set:**
   ```sh
   jq -r '.model' "$CLAUDE_CONFIG_DIR/settings.json"   # LIVE tier the process read
   jq -r '.model' "$HOME/.claude/settings.json"        # DURABLE host source
   ```
   Both should print the new id. (The shim `/run/claude/shim/settings.json` will still show `model: null` — that is expected; it is not the model source.)
2. **In-session confirmation:** after the respawn, the main-loop session's own environment banner / `/status`-style model line reports the active model id — confirm it names the new model. (The container CLAUDE.md's session-start line also states the model.)
3. If the value did NOT change: you almost certainly edited only the host file and ran `cwsr` (Mode A) without updating the tmpfs farm copy — the respawn read the stale farm. Re-do Mode A step 2 for `$CLAUDE_CONFIG_DIR/settings.json`, or use Mode B (force-recreate regenerates it).

## Important

- **Model source = user-tier `settings.json` `model` field. NOT** a `--model` flag, NOT an env var, NOT a cwsr argument, NOT the `--settings` shim (which is `model: null`).
- **`cwsr` does not regenerate the snapshot farm.** Mode A must edit the live tmpfs farm copy directly; Mode B lets the force-recreate regenerate it from the host file.
- **Durability:** always edit the host `~/.claude/settings.json` (rw bind-mount). The tmpfs `$CLAUDE_CONFIG_DIR/settings.json` is wiped and regenerated on every container start — an edit there alone is lost on the next recreate.
- **This is the MAIN-LOOP model only.** Subagent model is a separate per-`Agent`-call parameter (defaults to Sonnet per operator directive) — this skill does not touch it.
- **Save state before either mode** — both `cwsr` and `make deploy-container` kill the current session (context resumes via `CLAUDE_AUTO_CONTINUE`).
- **Get the model id right** — verify via `/check-new-claude` against the SF gateway rather than guessing a `us.anthropic.claude-*` string.
- This skill lives in `container/skills/` because it drives the container's own inner-process lifecycle (cwsr / deploy-container) and reads in-container-only paths (`$CLAUDE_CONFIG_DIR`, `/run/claude/...`). It needs an **image rebuild** to land as a baked `/claude-container:switch-main-loop-model` command (see `container/skills/README.md`).
- Backing tools: [`cwsr`](https://github.com/gbre-org/claude-watch/blob/main/container/bin/cwsr), [`entrypoint.sh`](https://github.com/gbre-org/claude-watch/blob/main/container/entrypoint.sh) (farm generation via `generate-hooks-shim-settings`), and the sibling `/claude-container:deploy-container` skill.
