#!/usr/bin/env bash
#
# ci-apt-install.sh — bounded, retrying `apt-get install` for CI steps, with
# `apt-get update` demoted to a FALLBACK and the stalling Ubuntu mirror dropped
# on the way there.
#
# ## Why this exists
#
# A CI step installs `inotify-tools` (for the claude-event-watch tests) on
# GitHub's Ubuntu runners. That install wedges intermittently, and the failure
# has no middle ground: a healthy one finishes in ~21s, a wedged one produces
# no output at all and runs flat into whatever bound it is given. Measured on
# three consecutive runs of one branch: 312s (fail), 21s (pass), 313s (fail).
#
# The wedge is in `apt-get update`, not the install. Captured failures show apt
# cycling `Ign:` on the runner's `azure.archive.ubuntu.com` mirror, eventually
# falling back to `archive.ubuntu.com`, and running out of time part-way
# through the ~11 MB index refresh.
#
# The steps already carry `timeout-minutes: 5`. That bound is CORRECT and must
# stay — before it existed these hangs ran to GitHub's 360-minute job ceiling,
# so a *larger* bound buys longer failures, not passes. But a 5-minute step cap
# alone converts a hang into a failed job, because the first wedged attempt
# consumes the entire budget and nothing ever retries.
#
# ## Design
#
# Four layers, cheapest first.
#
# 1. DON'T REFRESH THE INDEXES AT ALL if you don't have to. The runner image
#    ships pre-baked `/var/lib/apt/lists`, so a plain `apt-get install` already
#    resolves and fetches. That turns an ~11 MB index download — the entire
#    observed failure surface — into two ~40 KB `.deb` fetches. So the default
#    order is install-FIRST, and `apt-get update` runs only when that install
#    fails (stale lists, a package added since the image was built). A stale-
#    list failure is a fast, loud apt error, not a hang, so the fallback costs
#    seconds on the rare path and nothing on the common one.
#
# 2. WHEN A RETRY HAPPENS, CHANGE SOMETHING. The retry loop used to re-run the
#    identical command against the identical mirror list, so a mirror having a
#    bad day failed all three attempts the same way. Before falling back to
#    `update` — and between install attempts — we drop the known-stalling
#    mirror from the runner's mirrorlist so the retry goes somewhere else. In
#    the captured failure the surviving mirror was serving fine; ~23s of each
#    60s attempt was spent thrashing the dead one before apt got there.
#
#    This trades the Azure mirror's normally-excellent throughput (measured
#    8 MB/s) for reliability. That is the right trade on a path we only reach
#    because something already failed.
#
# 3. Attack apt's own timeouts: they default to 120s per connection, which is
#    what lets a dead mirror stall a step. We pass `Acquire::{http,https}::
#    Timeout` and `Acquire::Retries` so apt abandons an unreachable mirror in
#    seconds rather than minutes.
#
# 4. Every phase gets its own inner `timeout` and retry loop, under a shared
#    wall-clock DEADLINE (default 240s) that caps the whole thing well inside
#    the 5-minute step cap. A total wedge still fails fast with a legible error
#    instead of being killed anonymously by the runner — and never reverts to
#    the 360-minute default. No attempt is started that cannot finish before
#    the deadline; the per-attempt timeout is clamped to the remaining budget.
#
# The preflight install gets ONE attempt rather than the full three, so a
# wedged mirror cannot spend the whole budget there and leave the `update`
# fallback with nothing to run in.
#
# ## Usage
#
#   scripts/ci-apt-install.sh inotify-tools
#
# Tunables (all optional, used by the test suite to keep runtimes short):
#
#   CI_APT_TOTAL_BUDGET     total wall-clock seconds, all phases      (240)
#   CI_APT_ATTEMPT_TIMEOUT  inner timeout for one attempt, seconds    (60)
#   CI_APT_ATTEMPTS         max attempts per retried phase            (3)
#   CI_APT_PREFLIGHT_ATTEMPTS  attempts for the no-update install     (1)
#   CI_APT_BACKOFF          seconds slept between attempts            (5)
#   CI_APT_GET              the apt-get invocation           (sudo apt-get)
#   CI_APT_DPKG             the dpkg invocation                 (sudo dpkg)
#   CI_APT_TEE              how to write a root-owned file        (sudo tee)
#   CI_APT_UPDATE_MODE      fallback | always | never            (fallback)
#   CI_APT_SKIP_UPDATE      legacy alias for UPDATE_MODE=never     (unset)
#   CI_APT_MIRRORLIST       runner mirrorlist    (/etc/apt/apt-mirrors.txt)
#   CI_APT_STALL_MIRROR     host to drop from it (azure.archive.ubuntu.com)
#   CI_APT_FALLBACK_MIRROR  floor if dropping would empty the list
#
# Exit codes: 0 success, 1 install/update failed or budget exhausted,
# 2 usage error.

set -uo pipefail

TOTAL_BUDGET="${CI_APT_TOTAL_BUDGET:-240}"
ATTEMPT_TIMEOUT="${CI_APT_ATTEMPT_TIMEOUT:-60}"
ATTEMPTS="${CI_APT_ATTEMPTS:-3}"
PREFLIGHT_ATTEMPTS="${CI_APT_PREFLIGHT_ATTEMPTS:-1}"
BACKOFF="${CI_APT_BACKOFF:-5}"

MIRRORLIST="${CI_APT_MIRRORLIST:-/etc/apt/apt-mirrors.txt}"
STALL_MIRROR="${CI_APT_STALL_MIRROR-azure.archive.ubuntu.com}"
FALLBACK_MIRROR="${CI_APT_FALLBACK_MIRROR:-http://archive.ubuntu.com/ubuntu/}"

# `never` skips the refresh entirely, `always` restores the old update-then-
# install order, `fallback` (the default) only refreshes when the install
# against the image's pre-baked lists fails.
UPDATE_MODE="${CI_APT_UPDATE_MODE:-fallback}"
[ "${CI_APT_SKIP_UPDATE:-}" = "1" ] && UPDATE_MODE="never"

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
    local dpkg_cmd
    read -r -a dpkg_cmd <<<"${CI_APT_DPKG:-sudo dpkg}"
    echo "==> repairing any interrupted dpkg state before retrying"
    timeout --kill-after=5 30 "${dpkg_cmd[@]}" --configure -a >/dev/null 2>&1 || true
}

# Drop the known-stalling mirror from the runner's mirrorlist so that a retry
# actually goes somewhere new. Idempotent (the grep guard makes a second call a
# no-op) and best effort: if the file is absent, already clean, or unwritable,
# we carry on with whatever apt has.
demote_stalling_mirror() {
    [ -n "$STALL_MIRROR" ] || return 0
    [ -f "$MIRRORLIST" ] || return 0
    grep -qF -- "$STALL_MIRROR" "$MIRRORLIST" 2>/dev/null || return 0

    local kept
    kept="$(grep -vF -- "$STALL_MIRROR" "$MIRRORLIST")"
    # An empty mirrorlist is worse than a slow one: apt would have nowhere to
    # go at all. Fall back to the canonical archive rather than write nothing.
    if [ -z "${kept//[[:space:]]/}" ]; then
        kept="$FALLBACK_MIRROR"
    fi

    echo "==> dropping ${STALL_MIRROR} from ${MIRRORLIST} so the retry uses a different mirror"
    local tee_cmd
    read -r -a tee_cmd <<<"${CI_APT_TEE:-sudo tee}"
    if ! printf '%s\n' "$kept" | "${tee_cmd[@]}" "$MIRRORLIST" >/dev/null 2>&1; then
        echo "==> could not rewrite ${MIRRORLIST}; continuing with it unchanged" >&2
    fi
}

# Between install attempts: repair any half-done dpkg state AND move off the
# mirror that just stalled. Doing only the first would retry into the same
# dead mirror, which is exactly the failure this script now exists to break.
install_retry_hook() {
    repair_dpkg
    demote_stalling_mirror
}

# run_phase LABEL RETRY_HOOK MAX_ATTEMPTS CMD...
#
# Runs CMD under an inner `timeout`, retrying up to MAX_ATTEMPTS times while
# the shared deadline allows. RETRY_HOOK is the name of a function to run
# between attempts (`:` for none). Returns 0 on the first success, 1 when the
# attempts or the budget run out.
run_phase() {
    local label="$1" retry_hook="$2" max_attempts="$3"
    shift 3
    local attempt=1 remaining inner rc reason

    while :; do
        remaining="$(remaining_budget)"
        if [ "$remaining" -lt "$MIN_ATTEMPT_SECS" ]; then
            echo "ci-apt-install: ${label}: out of budget (${remaining}s left of ${TOTAL_BUDGET}s); giving up" >&2
            return 1
        fi

        inner="$ATTEMPT_TIMEOUT"
        [ "$inner" -gt "$remaining" ] && inner="$remaining"

        echo "==> ${label}: attempt ${attempt}/${max_attempts} (inner timeout ${inner}s, ${remaining}s budget left)"
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

        if [ "$attempt" -ge "$max_attempts" ]; then
            echo "ci-apt-install: ${label}: all ${max_attempts} attempts failed" >&2
            return 1
        fi
        attempt=$((attempt + 1))
        "$retry_hook"
        sleep "$BACKOFF"
    done
}

run_update() {
    # `update` touches only the package lists, so there is no dpkg state to
    # repair between attempts — but there IS a mirror to move off, which is
    # the whole reason a second attempt might behave differently.
    run_phase "apt-get update" demote_stalling_mirror "$ATTEMPTS" \
        "${APT_GET[@]}" update "${APT_OPTS[@]}"
}

run_install() {
    local label="$1" max_attempts="$2"
    shift 2
    run_phase "$label" install_retry_hook "$max_attempts" \
        "${APT_GET[@]}" install -y "${APT_OPTS[@]}" "$@"
}

succeed() {
    echo "==> installed: $*"
    exit 0
}

fail_install() {
    echo "ci-apt-install: failed to install: $*" >&2
    exit 1
}

if [ "$UPDATE_MODE" = "always" ]; then
    run_update || exit 1
    run_install "apt-get install ($*)" "$ATTEMPTS" "$@" || fail_install "$@"
    succeed "$@"
fi

# Preflight: install straight from the image's pre-baked package lists. On the
# happy path this is the whole job and no package index is fetched at all.
if run_install "apt-get install ($*), pre-baked lists" "$PREFLIGHT_ATTEMPTS" "$@"; then
    succeed "$@"
fi

if [ "$UPDATE_MODE" = "never" ]; then
    fail_install "$@"
fi

echo "==> install against the image's pre-baked package lists failed; refreshing the lists and retrying"
demote_stalling_mirror
run_update || exit 1

run_install "apt-get install ($*), after refresh" "$ATTEMPTS" "$@" || fail_install "$@"
succeed "$@"
