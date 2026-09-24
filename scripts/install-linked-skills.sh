#!/usr/bin/env bash
# install-linked-skills.sh — COPY a repo's Agent Skills into the central
# "linked-skills" plugin dir that claude-container bind-mounts, so repo-linked
# skills (e.g. eichi-search) load INSIDE the container without a docker rebuild.
#
# WHY THIS EXISTS
#   In-container sessions get a fresh tmpfs ~/.claude + the baked
#   /opt/claude-container/skills — they do NOT inherit the host's
#   ~/.claude/skills. Worse, the host's ~/.claude/skills/<name> entries are
#   usually symlinks whose targets are HOST-ABSOLUTE paths (/Users/...), which
#   simply do not exist in the Linux container's filesystem namespace. So a
#   host symlink cannot carry a skill through the bind-mount.
#
# WHY COPY, NOT SYMLINK  (empirically verified 2026-09-24)
#   A symlink resolves through a bind-mount ONLY if its target path is valid
#   in the READER's namespace. Tested inside the container:
#     - host-absolute target (/Users/hallandrew/...)  -> BROKEN in-container
#       (the /Users tree is not mounted / not at that path)
#     - container-absolute (/home/hndrewaall/...) or relative target -> works
#   The natural host workflow (`ln -s`) produces host-absolute targets, so
#   symlinks are NOT reliable across the boundary. This installer COPIES the
#   skill payload into the central dir, which is bind-mounted read-only into
#   the container — the copy has no cross-namespace target to resolve.
#
# THE CENTRAL DIR IS A CLAUDE CODE PLUGIN
#   claude-container loads baked skills via `--plugin-dir`. This installer
#   makes the central dir a second plugin the entrypoint also loads:
#       <dest>/.claude-plugin/plugin.json     (manifest; seeded here if absent)
#       <dest>/skills/<name>/SKILL.md         (each installed skill)
#   Installed skills then surface in-container as `/linked-skills:<name>`.
#   Adding a skill is a RUNTIME op (re-run this installer); no docker rebuild.
#   A fresh central dir needs a container RECREATE only the FIRST time (to add
#   the bind-mount); after that, dropping a skill in is picked up on the next
#   in-container session start (the plugin dir is already mounted).
#
# PROVENANCE  (Andrew #9509)
#   Every installed skill gets a `.provenance.json` alongside its SKILL.md
#   recording source repo + source path + git commit + install timestamp, so
#   everything in the central dir is traceable and stale installs are visible.
#
# Idempotent: re-running refreshes the copy + provenance in place.
#
# Usage:
#   scripts/install-linked-skills.sh [--src DIR] [--dest DIR] [-n] [--prune]
#
# Options:
#   --src DIR     source to install FROM. Either a repo root (its
#                 `.claude/skills/` is used) or a skills dir directly (a dir
#                 whose children are `<name>/SKILL.md`). Default: $PWD.
#   --dest DIR    central linked-skills plugin dir. Default:
#                 $CLAUDE_LINKED_SKILLS_DIR, else
#                 ~/.config/claude-container/linked-skills
#   -n, --dry-run print what would change; touch nothing.
#   --prune       remove linked skills in <dest> whose provenance source path
#                 no longer exists (clean up deleted/renamed upstream skills).
#   -h, --help    this text.
#
# Exit: 0 on success (incl. "nothing to do"), 1 on usage / IO error.

set -euo pipefail

SRC="$PWD"
DEST="${CLAUDE_LINKED_SKILLS_DIR:-$HOME/.config/claude-container/linked-skills}"
DRY_RUN=0
PRUNE=0

usage() { sed -n '2,60p' "$0" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
    case "$1" in
        --src)  SRC="${2:?--src needs a directory}"; shift 2 ;;
        --dest) DEST="${2:?--dest needs a directory}"; shift 2 ;;
        -n|--dry-run) DRY_RUN=1; shift ;;
        --prune) PRUNE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "install-linked-skills: unknown argument: $1" >&2; exit 1 ;;
    esac
done

run() { if [ "$DRY_RUN" -eq 1 ]; then echo "  [dry-run] $*"; else "$@"; fi; }

# --- resolve the source skills dir -------------------------------------
if [ ! -d "$SRC" ]; then
    echo "install-linked-skills: source not found: $SRC" >&2
    exit 1
fi
SRC="$(cd "$SRC" && pwd -P)"
if [ -d "$SRC/.claude/skills" ]; then
    SKILLS_SRC="$SRC/.claude/skills"
elif [ -d "$SRC/skills" ] && ls "$SRC"/skills/*/SKILL.md >/dev/null 2>&1; then
    SKILLS_SRC="$SRC/skills"
elif ls "$SRC"/*/SKILL.md >/dev/null 2>&1; then
    SKILLS_SRC="$SRC"
else
    echo "install-linked-skills: no Agent Skills (<name>/SKILL.md) found under $SRC" >&2
    echo "  looked in: $SRC/.claude/skills, $SRC/skills, $SRC" >&2
    exit 1
fi

# Source repo root + commit for provenance (best-effort; skills need not be
# in a git repo).
SRC_REPO=""; SRC_COMMIT="unknown"
if git -C "$SKILLS_SRC" rev-parse --show-toplevel >/dev/null 2>&1; then
    SRC_REPO="$(git -C "$SKILLS_SRC" rev-parse --show-toplevel 2>/dev/null || true)"
    SRC_COMMIT="$(git -C "$SKILLS_SRC" rev-parse HEAD 2>/dev/null || echo unknown)"
    # append -dirty if the skill tree has uncommitted changes
    if ! git -C "$SKILLS_SRC" diff --quiet -- "$SKILLS_SRC" 2>/dev/null; then
        SRC_COMMIT="${SRC_COMMIT}-dirty"
    fi
fi
[ -n "$SRC_REPO" ] || SRC_REPO="$SRC"

NOW="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# --- seed the plugin manifest if absent --------------------------------
if [ ! -f "$DEST/.claude-plugin/plugin.json" ]; then
    run mkdir -p "$DEST/.claude-plugin"
    if [ "$DRY_RUN" -eq 1 ]; then
        echo "  [dry-run] write $DEST/.claude-plugin/plugin.json (name=linked-skills)"
    else
        cat > "$DEST/.claude-plugin/plugin.json" <<'PLUGIN'
{
  "name": "linked-skills",
  "version": "0.1.0",
  "description": "Repo-linked Agent Skills installed into the claude-container central dir (see scripts/install-linked-skills.sh). Skills surface as /linked-skills:<name>.",
  "author": { "name": "claude-watch contributors", "url": "https://github.com/gbre-org/claude-watch" }
}
PLUGIN
    fi
fi
run mkdir -p "$DEST/skills"

# --- install / refresh each skill --------------------------------------
installed=0
for skill_md in "$SKILLS_SRC"/*/SKILL.md; do
    [ -e "$skill_md" ] || continue
    skill_dir="$(dirname "$skill_md")"
    name="$(basename "$skill_dir")"
    [ "$name" = "synced" ] && continue   # reserved name (claude.ai sync)
    target="$DEST/skills/$name"

    # clean replace so a removed source file doesn't linger in dest
    run rm -rf "$target"
    run mkdir -p "$target"
    if [ "$DRY_RUN" -eq 1 ]; then
        echo "  [dry-run] cp -R $skill_dir/. $target/"
    else
        cp -R "$skill_dir/." "$target/"
    fi

    # stamp provenance alongside SKILL.md
    src_abs="$(cd "$skill_dir" && pwd -P)"
    if [ "$DRY_RUN" -eq 1 ]; then
        echo "  [dry-run] write $target/.provenance.json (repo=$SRC_REPO commit=$SRC_COMMIT)"
    else
        cat > "$target/.provenance.json" <<PROV
{
  "skill": "$name",
  "source_repo": "$SRC_REPO",
  "source_path": "$src_abs",
  "source_commit": "$SRC_COMMIT",
  "installed_at": "$NOW",
  "installed_by": "install-linked-skills.sh"
}
PROV
    fi
    echo "install-linked-skills: installed '$name' <- $src_abs (commit ${SRC_COMMIT})"
    installed=$((installed + 1))
done

# --- prune skills whose source path is gone ----------------------------
pruned=0
if [ "$PRUNE" -eq 1 ] && [ -d "$DEST/skills" ]; then
    for prov in "$DEST"/skills/*/.provenance.json; do
        [ -e "$prov" ] || continue
        sp="$(sed -n 's/.*"source_path": *"\([^"]*\)".*/\1/p' "$prov" | head -1)"
        if [ -n "$sp" ] && [ ! -e "$sp/SKILL.md" ]; then
            run rm -rf "$(dirname "$prov")"
            echo "install-linked-skills: pruned '$(basename "$(dirname "$prov")")' (source gone: $sp)"
            pruned=$((pruned + 1))
        fi
    done
fi

echo "install-linked-skills: ${installed} skill(s) installed into ${DEST}/skills as /linked-skills:<name>; ${pruned} pruned."
if [ "$installed" -gt 0 ]; then
    echo "install-linked-skills: if this is the FIRST install, recreate the container once to add the bind-mount (make deploy-container); after that new skills load on the next in-container session start."
fi
