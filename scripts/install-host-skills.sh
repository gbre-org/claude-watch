#!/usr/bin/env bash
# install-host-skills.sh — install this repo's deployment-agnostic skills
# (skills/*.md) into a host Claude Code commands dir as `cw-`-prefixed slash
# commands.
#
# WHY A PREFIX
#   Every skill that comes from THIS repo is namespaced so its origin is
#   obvious at the prompt, in both deployment modes:
#     - container deploy : `/claude-container:<name>` (plugin namespace, set by
#                          container/plugin/.claude-plugin/plugin.json)
#     - host deploy      : `/cw-<name>`               (filename prefix, here)
#   Claude Code derives a user-level slash command's name from the FILENAME, so
#   a `cw-` filename prefix is the host analogue of the plugin namespace. It
#   needs no plugin manifest and no change to how the operator launches
#   `claude`, which matters because the host `claude` process is started by the
#   operator (or a tmux bootstrap), not by anything this Makefile controls.
#
# WHY SYMLINKS, NOT COPIES
#   Same policy as `make install`: scripts install as ABSOLUTE-PATH symlinks
#   back into the source tree, so editing a skill in-tree is live immediately
#   with no reinstall round-trip. Only build artifacts get copied.
#
# WHY THIS IS DELIBERATELY TIMID
#   The destination (`~/.claude/commands` by default) is usually managed by
#   SOMEONE ELSE — on a typical host it is a symlink into the operator's own
#   private dotfiles/config repo holding dozens of hand-written skills. This
#   installer therefore owns EXACTLY the paths it created and nothing else:
#     - it never writes over a regular file;
#     - it never writes over a symlink pointing outside this repo's skills/;
#     - it only prunes `cw-*.md` symlinks that point INTO this repo's skills/
#       and whose target no longer exists (rename / delete upstream).
#   Anything it declines to touch is reported on stderr and does NOT fail the
#   run — a deploy must not be blocked by one operator-owned filename clash.
#
# Idempotent: re-running re-asserts the same links and prints the same summary.
#
# Usage:
#   scripts/install-host-skills.sh [-n|--dry-run] [--dest DIR] [--prefix P]
#
# Env:
#   CLAUDE_COMMANDS_DIR   destination dir (default: $HOME/.claude/commands)
#
# Exit status: 0 on success (including "nothing to do"), 1 on usage error or
# an unwritable destination.

set -euo pipefail

PREFIX="cw-"
DRY_RUN=0
DEST="${CLAUDE_COMMANDS_DIR:-$HOME/.claude/commands}"

usage() {
    cat <<'EOF'
install-host-skills.sh — install this repo's deployment-agnostic skills
(skills/*.md) into a host Claude Code commands dir as `cw-`-prefixed slash
commands, so their origin is obvious at the prompt (the host analogue of the
container's `/claude-container:<name>` plugin namespace).

Usage:
  scripts/install-host-skills.sh [-n|--dry-run] [--dest DIR] [--prefix P]

Options:
  -n, --dry-run   print what would change; touch nothing
      --dest DIR  destination commands dir (default: $CLAUDE_COMMANDS_DIR,
                  else ~/.claude/commands)
      --prefix P  filename prefix to install under (default: cw-)
  -h, --help      this text

Installs as absolute-path symlinks back into the source tree, so in-tree edits
are live with no reinstall. Idempotent. Never overwrites a regular file or a
symlink pointing outside this repo; only prunes its own dangling links.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        -n|--dry-run) DRY_RUN=1; shift ;;
        --dest) DEST="${2:?--dest needs a directory}"; shift 2 ;;
        --prefix) PREFIX="${2?--prefix needs a value}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "install-host-skills: unknown argument: $1" >&2; exit 1 ;;
    esac
done

# Resolve the repo's skills/ dir from this script's own location, so the
# installer works from any cwd and from a linked worktree.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
SRC="$(cd "$SCRIPT_DIR/.." && pwd -P)/skills"

if [ ! -d "$SRC" ]; then
    echo "install-host-skills: source dir not found: $SRC" >&2
    exit 1
fi

run() {
    if [ "$DRY_RUN" -eq 1 ]; then
        echo "  [dry-run] $*"
    else
        "$@"
    fi
}

if [ ! -d "$DEST" ]; then
    if [ "$DRY_RUN" -eq 1 ]; then
        echo "  [dry-run] mkdir -p $DEST"
    else
        mkdir -p "$DEST" || {
            echo "install-host-skills: cannot create destination: $DEST" >&2
            exit 1
        }
    fi
fi

installed=0
skipped=0
pruned=0

# --- install / refresh -------------------------------------------------
for src in "$SRC"/*.md; do
    [ -e "$src" ] || continue
    base="$(basename "$src")"
    # README.md documents the dir; it is not a skill.
    [ "$base" = "README.md" ] && continue

    target="$DEST/${PREFIX}${base}"

    if [ -e "$target" ] && [ ! -L "$target" ]; then
        echo "install-host-skills: SKIP $target — a regular file already exists there (not ours; refusing to overwrite)" >&2
        skipped=$((skipped + 1))
        continue
    fi

    if [ -L "$target" ]; then
        current="$(readlink "$target")"
        case "$current" in
            "$SRC"/*) : ;;  # ours (possibly a stale target); safe to re-point
            *)
                echo "install-host-skills: SKIP $target — existing symlink points outside this repo ($current); refusing to overwrite" >&2
                skipped=$((skipped + 1))
                continue
                ;;
        esac
    fi

    run ln -sfn "$src" "$target"
    installed=$((installed + 1))
done

# --- prune links we own whose source is gone ---------------------------
# Only `${PREFIX}*.md` symlinks resolving into THIS repo's skills/ qualify.
# A renamed or deleted skill leaves a dangling link that would otherwise
# surface as a broken slash command forever.
for target in "$DEST/${PREFIX}"*.md; do
    [ -L "$target" ] || continue
    current="$(readlink "$target")"
    case "$current" in
        "$SRC"/*)
            if [ ! -e "$current" ]; then
                run rm -f "$target"
                pruned=$((pruned + 1))
            fi
            ;;
    esac
done

echo "install-host-skills: ${installed} skill(s) linked into ${DEST} as /${PREFIX}<name>; ${pruned} stale link(s) pruned; ${skipped} path(s) skipped (not ours)."
