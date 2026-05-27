#!/usr/bin/env bash
# install.sh — one-command LaunchAgent installer for the personal-mac MCP
# host bridge. Replaces the manual "cp the plist, then hand-edit
# /PATH/TO/REPO and /PATH/TO/HOME in your editor" dance.
#
# What this does
#
#   1. Resolves REPO (the absolute path to your local claude-watch
#      checkout — derived from this script's own location, falling back
#      to `git rev-parse --show-toplevel`) and HOME ($HOME). launchd does
#      NOT expand `~` / `${HOME}` in plist values, so the template ships
#      with /PATH/TO/REPO and /PATH/TO/HOME placeholders that have to be
#      replaced with literal absolute paths. This script does that for
#      you.
#   2. Copies the chosen LaunchAgent plist to ~/Library/LaunchAgents/
#      with the placeholders substituted (via sed), and pre-creates
#      ~/Library/Logs/ (launchd auto-creates the log files but not the
#      parent directory).
#   3. Optionally (--bootstrap) `launchctl bootstrap`s the unit. By
#      default it just installs and prints the bootstrap command for you
#      to run.
#
# Which plist
#
#   --bundled       install org.gbre.personal-mcp.host.plist — the
#                   bundled wrapper unit (runs personal-mcp-host.sh,
#                   which spawns mcp-host-bash AND the reverse SSH
#                   tunnel together).
#   --tunnel-only   install org.gbre.personal-mcp.tunnel.plist — the
#                   tunnel-only unit (the SSH reverse tunnel alone,
#                   for operators who run their MCP host server out of
#                   band). Only available if that plist exists in
#                   launchd/.
#
#   Exactly one of --bundled / --tunnel-only is REQUIRED — there is no
#   silent default, so you always know which unit you're wiring up.
#
# Usage
#
#   ./install.sh --bundled                  # install bundled unit, print bootstrap cmd
#   ./install.sh --tunnel-only              # install tunnel-only unit
#   ./install.sh --bundled --bootstrap      # install + launchctl bootstrap now
#   ./install.sh --bundled --print-cmd      # dry run: print resolved paths + target,
#                                           #   write NOTHING, just show what it'd do
#   ./install.sh --help                     # this help
#
# Flags
#
#   --bundled            select org.gbre.personal-mcp.host.plist
#   --tunnel-only        select org.gbre.personal-mcp.tunnel.plist
#   --bootstrap          also `launchctl bootstrap gui/$(id -u) <plist>`
#                        after install (default: install only + print the
#                        bootstrap command).
#   --print-cmd          dry run — resolve REPO / HOME / source / dest,
#                        print them, and print the bootstrap command, but
#                        do NOT write the plist, mkdir, or bootstrap.
#                        (Used by tests/install.test.)
#   --dest-dir DIR       override the LaunchAgents install dir (default
#                        $HOME/Library/LaunchAgents). Mainly for tests.
#   --home DIR           override the HOME value substituted into the
#                        plist (default $HOME). Mainly for tests.
#   --repo DIR           override the REPO value substituted into the
#                        plist (default: auto-resolved checkout root).
#   --help, -h           this help.
#
# Idempotent: safe to re-run. Re-running overwrites the installed plist
# with freshly-substituted paths. `launchctl bootstrap` of an
# already-bootstrapped unit is a no-op-ish error (37 "already in
# progress"); --bootstrap tolerates that and tells you to `bootout`
# first if you want a clean re-bootstrap.
#
# Exit codes
#   0   success (or --help / --print-cmd)
#   2   bad / missing flag, no plist selected, or chosen plist missing
#   1   install or bootstrap failure

set -euo pipefail

usage() {
    sed -n '2,/^set -euo/p' "$0" | sed -e 's/^# \{0,1\}//' -e '/^set -euo/d'
}

# -----------------------------------------------------------------------------
# Resolve this script's directory + the repo root.
# -----------------------------------------------------------------------------

script_dir="$(cd "$(dirname "$0")" && pwd)"

resolve_repo_root() {
    # The script lives at <repo>/examples/personal-mac-mcp-host/install.sh,
    # so the checkout root is three levels up. Prefer git's own answer
    # when this is a real checkout; fall back to the path arithmetic so
    # the script still works from a tarball export with no .git.
    local via_git
    if via_git="$(git -C "$script_dir" rev-parse --show-toplevel 2>/dev/null)"; then
        printf '%s\n' "$via_git"
        return 0
    fi
    (cd "$script_dir/../.." && pwd)
}

# -----------------------------------------------------------------------------
# Argv parsing
# -----------------------------------------------------------------------------

UNIT=""            # "bundled" | "tunnel-only"
DO_BOOTSTRAP=0
PRINT_CMD=0
DEST_DIR=""        # default resolved below
HOME_OVERRIDE=""
REPO_OVERRIDE=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --help|-h)
            usage
            exit 0
            ;;
        --bundled)
            if [ -n "$UNIT" ] && [ "$UNIT" != "bundled" ]; then
                echo "install.sh: --bundled conflicts with --tunnel-only; pick one." >&2
                exit 2
            fi
            UNIT="bundled"
            shift
            ;;
        --tunnel-only)
            if [ -n "$UNIT" ] && [ "$UNIT" != "tunnel-only" ]; then
                echo "install.sh: --tunnel-only conflicts with --bundled; pick one." >&2
                exit 2
            fi
            UNIT="tunnel-only"
            shift
            ;;
        --bootstrap)
            DO_BOOTSTRAP=1
            shift
            ;;
        --print-cmd)
            PRINT_CMD=1
            shift
            ;;
        --dest-dir)
            [ "$#" -ge 2 ] || { echo "install.sh: --dest-dir needs a value" >&2; exit 2; }
            DEST_DIR="$2"
            shift 2
            ;;
        --dest-dir=*)
            DEST_DIR="${1#*=}"
            shift
            ;;
        --home)
            [ "$#" -ge 2 ] || { echo "install.sh: --home needs a value" >&2; exit 2; }
            HOME_OVERRIDE="$2"
            shift 2
            ;;
        --home=*)
            HOME_OVERRIDE="${1#*=}"
            shift
            ;;
        --repo)
            [ "$#" -ge 2 ] || { echo "install.sh: --repo needs a value" >&2; exit 2; }
            REPO_OVERRIDE="$2"
            shift 2
            ;;
        --repo=*)
            REPO_OVERRIDE="${1#*=}"
            shift
            ;;
        *)
            printf 'install.sh: unknown argument %q\n' "$1" >&2
            echo 'See --help for usage.' >&2
            exit 2
            ;;
    esac
done

if [ -z "$UNIT" ]; then
    echo "install.sh: pick a unit — pass --bundled or --tunnel-only (see --help)." >&2
    exit 2
fi

# -----------------------------------------------------------------------------
# Resolve REPO / HOME / source plist / dest.
# -----------------------------------------------------------------------------

REPO="${REPO_OVERRIDE:-$(resolve_repo_root)}"
HOME_DIR="${HOME_OVERRIDE:-$HOME}"
DEST_DIR="${DEST_DIR:-$HOME_DIR/Library/LaunchAgents}"
LOG_DIR="$HOME_DIR/Library/Logs"

case "$UNIT" in
    bundled)
        PLIST_NAME="org.gbre.personal-mcp.host.plist"
        ;;
    tunnel-only)
        PLIST_NAME="org.gbre.personal-mcp.tunnel.plist"
        ;;
esac

SRC_PLIST="$script_dir/launchd/$PLIST_NAME"
DEST_PLIST="$DEST_DIR/$PLIST_NAME"
LABEL="${PLIST_NAME%.plist}"

if [ ! -f "$SRC_PLIST" ]; then
    if [ "$UNIT" = "tunnel-only" ]; then
        cat >&2 <<EOF
install.sh: tunnel-only plist not found: $SRC_PLIST

The tunnel-only LaunchAgent template is not present in this checkout.
Use --bundled to install the bundled wrapper unit
(org.gbre.personal-mcp.host.plist) instead, which runs the MCP host
server AND the reverse SSH tunnel together.
EOF
    else
        echo "install.sh: source plist not found: $SRC_PLIST" >&2
    fi
    exit 2
fi

# launchctl bootstrap command we install for / print.
bootstrap_cmd="launchctl bootstrap gui/\$(id -u) \"$DEST_PLIST\""

# -----------------------------------------------------------------------------
# --print-cmd: dry run. Resolve + print everything, write nothing.
# -----------------------------------------------------------------------------

if [ "$PRINT_CMD" = "1" ]; then
    echo "UNIT:          $UNIT"
    echo "REPO:          $REPO"
    echo "HOME:          $HOME_DIR"
    echo "SRC_PLIST:     $SRC_PLIST"
    echo "DEST_PLIST:    $DEST_PLIST"
    echo "LOG_DIR:       $LOG_DIR"
    echo "LABEL:         $LABEL"
    echo
    echo "Substituted plist preview (NOT written):"
    echo "--- BEGIN PLIST ---"
    sed -e "s#/PATH/TO/REPO#${REPO}#g" -e "s#/PATH/TO/HOME#${HOME_DIR}#g" "$SRC_PLIST"
    echo "--- END PLIST ---"
    echo
    echo "Bootstrap command:"
    echo "  $bootstrap_cmd"
    exit 0
fi

# -----------------------------------------------------------------------------
# Install: mkdir, substitute, write.
# -----------------------------------------------------------------------------

mkdir -p "$DEST_DIR"
mkdir -p "$LOG_DIR"

# sed `#` delimiter so absolute paths (which contain `/`) need no
# escaping. Write to a temp file in the dest dir then mv into place so a
# re-run is atomic and never leaves a half-written plist.
tmp_plist="$(mktemp "${DEST_PLIST}.XXXXXX")"
trap 'rm -f "$tmp_plist"' EXIT
sed -e "s#/PATH/TO/REPO#${REPO}#g" -e "s#/PATH/TO/HOME#${HOME_DIR}#g" "$SRC_PLIST" > "$tmp_plist"

# Sanity: no placeholders may survive. If any do, something is wrong with
# the substitution — bail rather than install a broken plist.
if grep -q '/PATH/TO/' "$tmp_plist"; then
    echo "install.sh: ERROR — placeholders survived substitution in $tmp_plist:" >&2
    grep -n '/PATH/TO/' "$tmp_plist" >&2
    exit 1
fi

chmod 0644 "$tmp_plist"
mv "$tmp_plist" "$DEST_PLIST"
trap - EXIT

echo "install.sh: installed $LABEL"
echo "  -> $DEST_PLIST"
echo "  REPO: $REPO"
echo "  HOME: $HOME_DIR"
echo "  logs: $LOG_DIR"

# -----------------------------------------------------------------------------
# --bootstrap: launchctl bootstrap now. Otherwise print the command.
# -----------------------------------------------------------------------------

if [ "$DO_BOOTSTRAP" = "1" ]; then
    if ! command -v launchctl >/dev/null 2>&1; then
        echo "install.sh: launchctl not found — are you on macOS?" >&2
        echo "install.sh: plist is installed; bootstrap manually with:" >&2
        echo "  $bootstrap_cmd" >&2
        exit 1
    fi
    echo "install.sh: bootstrapping $LABEL ..."
    if launchctl bootstrap "gui/$(id -u)" "$DEST_PLIST"; then
        echo "install.sh: bootstrapped. Start a session with:"
        echo "  launchctl kickstart gui/\$(id -u)/$LABEL"
    else
        rc=$?
        echo "install.sh: launchctl bootstrap exited $rc." >&2
        echo "install.sh: if the unit is already bootstrapped (rc=37), bootout first:" >&2
        echo "  launchctl bootout gui/\$(id -u)/$LABEL && $bootstrap_cmd" >&2
        exit 1
    fi
else
    echo
    echo "Next: bootstrap the unit (registers it; RunAtLoad=false means it won't fire):"
    echo "  $bootstrap_cmd"
    echo "Then start a session on demand with:"
    echo "  launchctl kickstart gui/\$(id -u)/$LABEL"
    echo "(or re-run this script with --bootstrap to do the bootstrap now.)"
fi
