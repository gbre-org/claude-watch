#!/usr/bin/env bash
#
# ci-apt-install.sh — bounded, retrying `apt-get update && apt-get install`
# for CI steps.
#
# ## Why this exists
#
# Two CI steps install a system package (`tmux` for the e2e job,
# `inotify-tools` for the container-shell job). Both wedge intermittently on
# GitHub's Ubuntu runners, and the failure has no middle ground: a healthy
# install finishes in ~21s, a wedged one produces no output at all and runs
# flat into whatever bound it is given. Measured on three consecutive runs of
# one branch: 312s (fail), 21s (pass), 313s (fail).
#
# The wedge is in `apt-get update`, not the install. A captured failure shows
# apt cycling `Ign:` on the runner's `azure.archive.ubuntu.com` mirror, falling
# back to `archive.ubuntu.com`, and then going silent for 4.5 minutes with the
# transfer half-done.
#
# The steps already carry `timeout-minutes: 5`. That bound is CORRECT and must
# stay — before it existed these hangs ran to GitHub's 360-minute job ceiling,
# so a *larger* bound buys longer failures, not passes. But a 5-minute step cap
# alone converts a hang into a failed job, because the first wedged attempt
# consumes the entire budget and nothing ever retries.
#
# This script catches the hang EARLY and retries inside the same outer bound.
#
# ## Design
#
# 1. Attack the cause first: apt's own network timeouts default to 120s per
#    connection, which is what lets a dead mirror stall the step. We pass
#    `Acquire::{http,https}::Timeout` and `Acquire::Retries` so apt abandons an
#    unreachable mirror in seconds rather than minutes.
#
# 2. `update` and `install` get SEPARATE inner timeouts and retry loops. The
#    observed wedge is in `update`, but `install` pulls from the same mirrors
#    and is exposed to the same stall, so bounding only `update` would move the
#    hang rather than fix it. Keeping the bounds separate means a slow-but-
#    healthy `update` cannot eat the budget `install` needs to finish.
#
# 3. A shared wall-clock DEADLINE (default 240s) caps the whole thing well
#    inside the 5-minute step cap, so a total wedge still fails fast with a
#    legible error instead of being killed anonymously by the runner — and
#    never reverts to the 360-minute default. No attempt is started that cannot
#    finish before the deadline; the per-attempt timeout is clamped to the
#    remaining budget.
#
# With the defaults, one wedged `update` attempt costs 60s and the retry has
# ~170s of headroom to succeed — which a healthy attempt uses ~20s of.
#
# ## Usage
#
#   scripts/ci-apt-install.sh tmux
#   scripts/ci-apt-install.sh inotify-tools
#
# Tunables (all optional, used by the test suite to keep runtimes short):
#
#   CI_APT_TOTAL_BUDGET   total wall-clock seconds for both phases   (240)
#   CI_APT_ATTEMPT_TIMEOUT  inner timeout for one attempt, seconds   (60)
#   CI_APT_ATTEMPTS       max attempts per phase                     (3)
#   CI_APT_BACKOFF        seconds slept between attempts             (5)
#   CI_APT_GET            the apt-get invocation                     (sudo apt-get)
#   CI_APT_SKIP_UPDATE    set to 1 to skip the `update` phase        (unset)
#
# Exit codes: 0 success, 1 install/update failed or budget exhausted,
# 2 usage error.

set -uo pipefail

TOTAL_BUDGET="${CI_APT_TOTAL_BUDGET:-240}"
ATTEMPT_TIMEOUT="${CI_APT_ATTEMPT_TIMEOUT:-60}"
ATTEMPTS="${CI_APT_ATTEMPTS:-3}"
BACKOFF="${CI_APT_BACKOFF:-5}"

# Never start an attempt with less than this much budget left: a few-second
# window cannot plausibly succeed against a package mirror, and burning it just
# delays the real error. Derived rather than fixed at 15 so a deliberately
# short configuration (the test suite runs 2s attempts) cannot deadlock itself
# by demanding more headroom than one whole attempt needs.
MIN_ATTEMPT_SECS=15
[ "$ATTEMPT_TIMEOUT" -lt "$MIN_ATTEMPT_SECS" ] && MIN_ATTEMPT_SECS="$ATTEMPT_TIMEOUT"

if [ "$#" -eq 0 ]; then
    echo "usage: ${0##*/} PACKAGE [PACKAGE...]" >&2
    exit 2
fi

# Word-split the apt-get invocation so `sudo apt-get` (the default) and a
# bare `apt-get` (root containers) and a test double all work.
read -r -a APT_GET <<<"${CI_APT_GET:-sudo apt-get}"
if [ "${#APT_GET[@]}" -eq 0 ]; then
    echo "ci-apt-install: CI_APT_GET is empty" >&2
    exit 2
fi

# Bound apt's OWN network waits. This is the root-cause mitigation: the
# default 120s per-connection timeout is what turns one unreachable mirror
# into a multi-minute stall.
APT_OPTS=(
    -o "Acquire::Retries=3"
    -o "Acquire::http::Timeout=15"
    -o "Acquire::https::Timeout=15"
)

export DEBIAN_FRONTEND=noninteractive

START="$(date +%s)"
DEADLINE=$((START + TOTAL_BUDGET))

remaining_budget() {
    echo $((DEADLINE - $(date +%s)))
}

# Killing apt mid-install can leave dpkg half-configured, which would make the
# retry fail for a DIFFERENT reason than the one we are recovering from. A
# bounded repair pass between attempts keeps each retry meaningful. Best
# effort: a failure here is not itself fatal, and it is a no-op on the far more
# common case (a network stall, where dpkg never started).
repair_dpkg() {
    read -r -a dpkg_cmd <<<"${CI_APT_DPKG:-sudo dpkg}"
    echo "==> repairing any interrupted dpkg state before retrying"
    timeout --kill-after=5 30 "${dpkg_cmd[@]}" --configure -a >/dev/null 2>&1 || true
}

# run_phase LABEL RETRY_HOOK CMD...
#
# Runs CMD under an inner `timeout`, retrying up to $ATTEMPTS times while the
# shared deadline allows. RETRY_HOOK is the name of a function to run between
# attempts (`:` for none). Returns 0 on the first success, 1 when the attempts
# or the budget run out.
run_phase() {
    local label="$1" retry_hook="$2"
    shift 2
    local attempt=1 remaining inner rc reason

    while :; do
        remaining="$(remaining_budget)"
        if [ "$remaining" -lt "$MIN_ATTEMPT_SECS" ]; then
            echo "ci-apt-install: ${label}: out of budget (${remaining}s left of ${TOTAL_BUDGET}s); giving up" >&2
            return 1
        fi

        inner="$ATTEMPT_TIMEOUT"
        [ "$inner" -gt "$remaining" ] && inner="$remaining"

        echo "==> ${label}: attempt ${attempt}/${ATTEMPTS} (inner timeout ${inner}s, ${remaining}s budget left)"
        # --kill-after: if the wedged child ignores TERM, SIGKILL it rather
        # than letting it outlive its own timeout.
        timeout --kill-after=10 "$inner" "$@"
        rc=$?
        if [ "$rc" -eq 0 ]; then
            return 0
        fi

        if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then
            reason="TIMED OUT after ${inner}s (the known apt wedge)"
        else
            reason="failed with exit ${rc}"
        fi
        echo "==> ${label}: attempt ${attempt} ${reason}" >&2

        if [ "$attempt" -ge "$ATTEMPTS" ]; then
            echo "ci-apt-install: ${label}: all ${ATTEMPTS} attempts failed" >&2
            return 1
        fi
        attempt=$((attempt + 1))
        "$retry_hook"
        sleep "$BACKOFF"
    done
}

if [ "${CI_APT_SKIP_UPDATE:-}" != "1" ]; then
    # No dpkg repair hook: `update` touches only the package lists.
    if ! run_phase "apt-get update" : "${APT_GET[@]}" update "${APT_OPTS[@]}"; then
        exit 1
    fi
fi

if ! run_phase "apt-get install ($*)" repair_dpkg \
    "${APT_GET[@]}" install -y "${APT_OPTS[@]}" "$@"; then
    echo "ci-apt-install: failed to install: $*" >&2
    exit 1
fi

echo "==> installed: $*"
exit 0
