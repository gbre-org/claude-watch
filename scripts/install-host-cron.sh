#!/usr/bin/env bash
# install-host-cron.sh — render + install this repo's HOST cron fragment
# (cron.d/cw-host) into /etc/cron.d.
#
# WHY THE FRAGMENT IS PARAMETERIZED
#   The container's crontab (container/cron.d/cw-default) travels verbatim
#   because every path in it is fixed by the image (/usr/local/bin/claude-watch,
#   /var/lib/claude-watch). A HOST deployment has no such fixed prefix: the
#   binary lives wherever the operator cloned the repo, under whatever account
#   runs the daemon. Hard-coding one deployment's answers would (a) make the
#   fragment useless to a second deployment and (b) put one operator's home
#   path into a PUBLIC repo. So cron.d/cw-host ships @PLACEHOLDER@s and this
#   script fills them in at install time from the local checkout.
#
# WHAT IT SUBSTITUTES
#   @CW_USER@       user each job runs as        (default: current user)
#   @CW_HOME@       that user's home dir         (default: $HOME)
#   @CW_BIN@        claude-watch binary path     (default: <repo>/target/release/claude-watch)
#   @CW_STATE_DIR@  writable state dir           (default: /var/lib/claude-watch)
#
#   @CW_BIN@ defaults to the checkout's release build because that is what the
#   systemd unit's ExecStart runs, and the build-identity metric
#   (`claude_watch_build_info`) is compiled INTO the binary — so the gauge
#   describes whichever binary cron execs. Point cron at a different copy (e.g.
#   the $BIN_DIR copy `make install` leaves in ~/bin) and the gauge silently
#   reports that copy's commit instead of the running daemon's, with nothing
#   failing loudly. This script cross-checks the resolved path against the
#   installed unit's ExecStart when systemctl is available and warns on a
#   mismatch.
#
# WHAT IT REFUSES TO DO
#   - install a fragment whose @CW_BIN@ does not exist (that is a cron job that
#     will fail silently every minute) — override with --force;
#   - leave any @PLACEHOLDER@ unsubstituted in the output;
#   - symlink the fragment into /etc/cron.d. Cron rejects entries in
#     /etc/cron.d that are symlinks or not root-owned with "WRONG FILE OWNER"
#     and skips them silently, so the file is COPIED in as root:root 0644.
#
# Usage:
#   scripts/install-host-cron.sh [options]
#
# Options:
#   -n, --dry-run       render to stdout, write nothing (also --print)
#       --user USER     override @CW_USER@
#       --home DIR      override @CW_HOME@
#       --bin PATH      override @CW_BIN@
#       --state-dir DIR override @CW_STATE_DIR@
#       --source FILE   template to render (default: <repo>/cron.d/cw-host)
#       --dest PATH     destination (default: /etc/cron.d/cw-host)
#       --force         install even if @CW_BIN@ does not exist
#   -h, --help          this help
#
# Idempotent: re-running re-renders and overwrites the destination with the
# same content. Cron picks the change up on its next minute tick; no restart.
#
# Exit status:
#   0  success (including --dry-run)
#   1  install failure (unwritable destination, sudo refused, ...)
#   2  usage error, missing template, missing binary without --force, or an
#      unsubstituted placeholder

set -euo pipefail

DRY_RUN=0
FORCE=0
DEST="/etc/cron.d/cw-host"
SOURCE=""
CW_USER=""
CW_HOME=""
CW_BIN=""
CW_STATE_DIR="/var/lib/claude-watch"

usage() {
    sed -n '2,/^set -euo/p' "$0" | sed -e 's/^# \{0,1\}//' -e '/^set -euo/d'
}

die() {
    echo "install-host-cron.sh: $1" >&2
    exit "${2:-2}"
}

# -----------------------------------------------------------------------------
# Resolve the repo root from this script's own location, falling back to git.
# Mirrors examples/personal-mac-mcp-host/install.sh so both installers behave
# the same when run from an unusual cwd.
# -----------------------------------------------------------------------------
script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
if [ ! -f "$repo_root/Cargo.toml" ]; then
    if git_root="$(git -C "$script_dir" rev-parse --show-toplevel 2>/dev/null)"; then
        repo_root="$git_root"
    fi
fi

# -----------------------------------------------------------------------------
# Flags
# -----------------------------------------------------------------------------
while [ $# -gt 0 ]; do
    case "$1" in
        -n|--dry-run|--print) DRY_RUN=1; shift ;;
        --force) FORCE=1; shift ;;
        --user) [ $# -ge 2 ] || die "--user needs a value"; CW_USER="$2"; shift 2 ;;
        --home) [ $# -ge 2 ] || die "--home needs a value"; CW_HOME="$2"; shift 2 ;;
        --bin) [ $# -ge 2 ] || die "--bin needs a value"; CW_BIN="$2"; shift 2 ;;
        --state-dir) [ $# -ge 2 ] || die "--state-dir needs a value"; CW_STATE_DIR="$2"; shift 2 ;;
        --source) [ $# -ge 2 ] || die "--source needs a value"; SOURCE="$2"; shift 2 ;;
        --dest) [ $# -ge 2 ] || die "--dest needs a value"; DEST="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown argument: $1" ;;
    esac
done

# -----------------------------------------------------------------------------
# Defaults for anything not overridden
# -----------------------------------------------------------------------------
[ -n "$SOURCE" ] || SOURCE="$repo_root/cron.d/cw-host"
[ -n "$CW_USER" ] || CW_USER="$(id -un)"
[ -n "$CW_HOME" ] || CW_HOME="${HOME:-/home/$CW_USER}"
[ -n "$CW_BIN" ] || CW_BIN="$repo_root/target/release/claude-watch"

[ -f "$SOURCE" ] || die "template not found: $SOURCE"

# Cron ignores files in /etc/cron.d whose name contains a dot.
dest_base="$(basename "$DEST")"
case "$dest_base" in
    *.*) die "cron ignores /etc/cron.d entries containing a dot: $dest_base" ;;
esac

# -----------------------------------------------------------------------------
# Sanity: the binary must exist, and should be the one the service execs.
# -----------------------------------------------------------------------------
if [ ! -x "$CW_BIN" ] && [ "$FORCE" -eq 0 ]; then
    die "claude-watch binary not found or not executable: $CW_BIN
  Build it first (make build), pass --bin PATH, or re-run with --force."
fi

if command -v systemctl >/dev/null 2>&1; then
    unit_exec="$(systemctl show -p ExecStart --value claude-watch 2>/dev/null || true)"
    if [ -n "$unit_exec" ] && [ "${unit_exec#*"$CW_BIN"}" = "$unit_exec" ]; then
        cat >&2 <<EOF
install-host-cron.sh: WARNING — the installed claude-watch unit does not exec
  the binary this cron fragment will run:
    cron   : $CW_BIN
    service: $unit_exec
  The build-identity metric is compiled into the binary, so it will describe
  the cron copy, not the running daemon. Point --bin at the service's ExecStart
  path (or fix the unit) unless you know you want them to differ.
EOF
    fi
fi

# -----------------------------------------------------------------------------
# Render
# -----------------------------------------------------------------------------
# sed replacement escaping: `|` is the delimiter, `&` means "whole match", and
# a trailing backslash would continue the replacement.
sed_escape() { printf '%s' "$1" | sed -e 's/[\\&|]/\\&/g'; }

rendered="$(
    sed \
        -e "s|@CW_USER@|$(sed_escape "$CW_USER")|g" \
        -e "s|@CW_HOME@|$(sed_escape "$CW_HOME")|g" \
        -e "s|@CW_BIN@|$(sed_escape "$CW_BIN")|g" \
        -e "s|@CW_STATE_DIR@|$(sed_escape "$CW_STATE_DIR")|g" \
        "$SOURCE"
)"

# Nothing may survive substitution — an unfilled placeholder is a cron row that
# fails every minute, or worse, a PATH= line that silently breaks every job.
if leftover="$(printf '%s\n' "$rendered" | grep -o '@CW_[A-Z_]*@' | sort -u)" && [ -n "$leftover" ]; then
    die "unsubstituted placeholder(s) in rendered output: $(echo "$leftover" | tr '\n' ' ')"
fi

if [ "$DRY_RUN" -eq 1 ]; then
    cat >&2 <<EOF
install-host-cron.sh (dry run)
  template   : $SOURCE
  dest       : $DEST
  @CW_USER@      = $CW_USER
  @CW_HOME@      = $CW_HOME
  @CW_BIN@       = $CW_BIN
  @CW_STATE_DIR@ = $CW_STATE_DIR
EOF
    printf '%s\n' "$rendered"
    exit 0
fi

# -----------------------------------------------------------------------------
# Install: regular file, root:root, 0644 (cron skips symlinks / non-root files)
# -----------------------------------------------------------------------------
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
printf '%s\n' "$rendered" > "$tmp"

dest_dir="$(dirname "$DEST")"

# Nearest EXISTING ancestor of the destination — a not-yet-created dest_dir is
# writable iff we could create it there. Checking dest_dir alone would send an
# ordinary writable target (a --dest under a tmpdir, say) down the sudo path
# and leave a root-owned directory behind.
existing_ancestor="$dest_dir"
while [ ! -d "$existing_ancestor" ] && [ "$existing_ancestor" != "/" ]; do
    existing_ancestor="$(dirname "$existing_ancestor")"
done

if [ "$(id -u)" -eq 0 ]; then
    SUDO=""
elif [ -w "$existing_ancestor" ] && { [ ! -e "$DEST" ] || [ -w "$DEST" ]; }; then
    # Writable without elevation (typically a test tmpdir). Skip sudo so the
    # script stays usable non-interactively.
    SUDO=""
elif command -v sudo >/dev/null 2>&1; then
    SUDO="sudo"
else
    die "cannot write $DEST (not root, and sudo is unavailable)" 1
fi

[ -d "$dest_dir" ] || $SUDO mkdir -p "$dest_dir" || die "cannot create $dest_dir" 1

if [ -n "$SUDO" ]; then
    $SUDO install -m 0644 -o root -g root "$tmp" "$DEST" || die "install to $DEST failed" 1
else
    install -m 0644 "$tmp" "$DEST" || die "install to $DEST failed" 1
fi

echo "Installed $DEST (from $SOURCE)"
echo "  user      : $CW_USER"
echo "  binary    : $CW_BIN"
echo "  state dir : $CW_STATE_DIR"

if [ ! -d "$CW_STATE_DIR" ]; then
    cat >&2 <<EOF
install-host-cron.sh: NOTE — state dir $CW_STATE_DIR does not exist yet.
  The active-agents job writes there every minute. Create it once:
    sudo mkdir -p $CW_STATE_DIR && sudo chown $CW_USER $CW_STATE_DIR
EOF
fi

echo "Cron picks this up on its next minute tick — no daemon restart needed."
