#!/usr/bin/env bash
# End-to-end test for self-login against a REAL tmux pane.
#
# The unit suite (test_self_login.py) covers the pure predicates. This one
# covers the parts that only exist once a terminal does: reading a hard-wrapped
# OAuth URL back out of a live pane, typing an authorization code into a modal
# with raw send-keys, and recognising the outcome.
#
# It never touches a real Claude Code session. It spins up its own throwaway
# tmux session running a shell script that impersonates the login screens, and
# points self-login at that pane with --pane.
#
# Run:
#   tools/watchers/tests/test_self_login_tmux.sh

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
SELF_LOGIN="$REPO/tools/watchers/self-login"
CW_BIN=""
for cand in "$REPO/target/release/claude-watch" "$REPO/target/debug/claude-watch"; do
  [ -x "$cand" ] && CW_BIN="$cand" && break
done

if ! command -v tmux >/dev/null 2>&1; then
  echo "SKIP: tmux not available"
  exit 0
fi
if [ -z "$CW_BIN" ]; then
  echo "SKIP: no built claude-watch binary (run 'cargo build' first)"
  exit 0
fi

WORK="$(mktemp -d)"
SESSION="cw-self-login-test-$$"
PASS=0
FAIL=0

cleanup() {
  tmux kill-session -t "$SESSION" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

# self-login shells out to `claude-watch login-url`, so the built binary has to
# win on PATH over any installed copy.
mkdir -p "$WORK/bin"
ln -sf "$CW_BIN" "$WORK/bin/claude-watch"
export PATH="$WORK/bin:$PATH"
export CLAUDE_SELF_LOGIN_LOG="$WORK/self-login.log"
export CLAUDE_SELF_LOGIN_STATE="$WORK/self-login.json"
export CLAUDE_SELF_LOGIN_LOCK="$WORK/self-login.lock"
unset CLAUDE_SELF_LOGIN_NOTIFY_CMD
# Event isolation, and it is load bearing. `self-login` emits through whatever
# `claude-event` is on PATH, and PATH here PREPENDS to the caller's rather than
# replacing it — so on a host that has the real binary, a test run drops
# high-priority events into the operator's live queue and a running main loop
# acts on them as real. Pointing the queue at the tempdir keeps the real
# `claude-event` in the test (its absence must not be what makes this pass)
# while sending everything it writes somewhere harmless.
export CLAUDE_EVENT_QUEUE="$WORK/events"
export CRON_EVENT_QUEUE="$WORK/events"
mkdir -p "$WORK/events"

ok()   { PASS=$((PASS+1)); echo "  PASS: $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }

check_eq() { # desc expected actual
  if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (expected '$2', got '$3')"; fi
}

FAKE_URL="https://claude.com/cai/oauth/authorize?code=true&client_id=cw-test-client&state=abcdefghijklmnopqrstuvwxyz0123456789&code_challenge=ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ&code_challenge_method=S256"

# ---------------------------------------------------------------------------
# Fake login screen. Prints the URL screen, blocks on a read, then reports
# success — the same shape as Claude Code's own flow.
# ---------------------------------------------------------------------------
cat > "$WORK/fake-login.sh" <<EOF
#!/usr/bin/env bash
printf "Browser didn't open? Use the url below to sign in\n\n"
printf '%s\n\n' "$FAKE_URL"
printf "Paste code here if prompted > "
read -r code
printf "\n\nLogin successful. Press Esc to go back to login options.\n"
printf "%s\n" "\$code" > "$WORK/received-code"
sleep 300
EOF
chmod +x "$WORK/fake-login.sh"

echo "== self-login against a live tmux pane =="

# A narrow pane so the long URL HARD-WRAPS across several lines. This is the
# case the reassembly logic exists for; a wide pane would not exercise it.
tmux new-session -d -s "$SESSION" -x 80 -y 24 "$WORK/fake-login.sh"
sleep 1.5
PANE="$SESSION:0.0"

# --- 1. the URL comes back off the pane, fully reassembled ---
GOT_URL="$("$SELF_LOGIN" --pane "$PANE" url 2>"$WORK/url.err")"
check_eq "self-login url reassembles the hard-wrapped OAuth URL" "$FAKE_URL" "$GOT_URL"

# Prove the URL really was wrapped, so the test above means something: the
# complete URL must NOT appear on any single captured line, yet self-login
# still returned it whole. That is only possible via reassembly.
if tmux capture-pane -t "$PANE" -p | grep -qF "$FAKE_URL"; then
  bad "the URL fit on one pane line; the reassembly path was NOT exercised"
else
  ok "the URL is hard-wrapped on the pane yet came back whole (reassembly exercised)"
fi

# --- 2. claude-watch login-url --not rejects a known-stale URL ---
"$CW_BIN" login-url --pane "$PANE" --not "$FAKE_URL" >/dev/null 2>&1
check_eq "login-url --not rejects the stale URL (exit 4)" "4" "$?"

# --- 3. a code is typed into the modal and the outcome is recognised ---
OUT="$("$SELF_LOGIN" --pane "$PANE" --json code "cw-test-code-12345#state-xyz" \
        --verify-timeout 20 2>"$WORK/code.err")"
RC=$?
check_eq "self-login code exits 0 when the login reports success" "0" "$RC"
if printf '%s' "$OUT" | grep -q '"ok": true'; then
  ok "self-login code returns ok:true as JSON"
else
  bad "self-login code JSON did not report ok:true (got: $OUT)"
fi
sleep 0.5
RECEIVED="$(cat "$WORK/received-code" 2>/dev/null)"
check_eq "the code arrived at the fake login intact" "cw-test-code-12345#state-xyz" "$RECEIVED"

# --- 4. the state file records the completed login ---
if grep -q '"status": "logged-in"' "$CLAUDE_SELF_LOGIN_STATE" 2>/dev/null; then
  ok "state file records status=logged-in"
else
  bad "state file did not record status=logged-in (got: $(cat "$CLAUDE_SELF_LOGIN_STATE" 2>/dev/null))"
fi

tmux kill-session -t "$SESSION" 2>/dev/null

# ---------------------------------------------------------------------------
# A pane with NO login dialog must be refused, not silently typed into.
# ---------------------------------------------------------------------------
tmux new-session -d -s "$SESSION" -x 80 -y 24 "sleep 300"
sleep 1
"$SELF_LOGIN" --pane "$PANE" code "abc123" --verify-timeout 5 >/dev/null 2>&1
check_eq "self-login code refuses a pane with no code prompt (exit 5)" "5" "$?"

"$SELF_LOGIN" --pane "$PANE" url >/dev/null 2>&1
check_eq "self-login url exits 4 when there is no URL on the pane" "4" "$?"

if grep -q '"status": "failed"' "$CLAUDE_SELF_LOGIN_STATE" 2>/dev/null; then
  ok "a refused code submission is recorded as a FAILURE, not a success"
else
  bad "refused code submission did not record a failure state"
fi

tmux kill-session -t "$SESSION" 2>/dev/null

# ---------------------------------------------------------------------------
# `cancel`: the watchdog that hands the pane back when an auto-fired login is
# never answered. This is the failure mode the whole abandon path exists for
# — /login opens a MODAL, so a login nobody completes takes the session down
# for as long as it stands there.
#
# The fake here has to model the one property that matters: Escape closes the
# dialog and the TUI comes back. A fake that ignored Escape would let a broken
# cancel pass.
# ---------------------------------------------------------------------------
cat > "$WORK/fake-cancellable-login.sh" <<EOF
#!/usr/bin/env bash
draw_login() {
  clear
  printf "Browser didn't open? Use the url below to sign in\n\n"
  printf '%s\n\n' "$FAKE_URL"
  printf "Paste code here if prompted > "
}
draw_tui() {
  clear
  printf "> \n"
  printf "  bypass permissions on · 0 shells\n"
  printf "                          12345 tokens\n"
}
draw_login
# Read one keystroke at a time; Escape (\$'\e') drops back to the TUI, which is
# what self-login cancel is supposed to achieve.
while IFS= read -r -n1 -s key; do
  if [ "\$key" = \$'\e' ]; then
    draw_tui
    break
  fi
done
sleep 300
EOF
chmod +x "$WORK/fake-cancellable-login.sh"

tmux new-session -d -s "$SESSION" -x 80 -y 24 "$WORK/fake-cancellable-login.sh"
sleep 1.5

# --- 5. a standing, unconsumed login dialog is escaped out of ---
OUT="$("$SELF_LOGIN" --pane "$PANE" --json cancel 2>"$WORK/cancel.err")"
check_eq "self-login cancel exits 0 on a standing login dialog" "0" "$?"
if printf '%s' "$OUT" | grep -q '"cancelled": true'; then
  ok "self-login cancel reports it actually dismissed the dialog"
else
  bad "self-login cancel did not report cancelled:true (got: $OUT)"
fi
sleep 0.5
if tmux capture-pane -t "$PANE" -p | grep -qF "Paste code here"; then
  bad "the login dialog is STILL on the pane after cancel"
else
  ok "the login dialog is gone from the pane after cancel"
fi
if grep -q '"status": "cancelled"' "$CLAUDE_SELF_LOGIN_STATE" 2>/dev/null; then
  ok "state file records status=cancelled"
else
  bad "state file did not record status=cancelled (got: $(cat "$CLAUDE_SELF_LOGIN_STATE" 2>/dev/null))"
fi

# --- 6. cancel on an ALREADY-clean pane is a no-op, not a keystroke ---
#
# The load-bearing property: the abandon watchdog fires on a timer without
# first proving the dialog is still up, so cancelling a healthy session must
# do nothing at all. If it blindly pressed Escape it would interrupt whatever
# the session was doing.
OUT="$("$SELF_LOGIN" --pane "$PANE" --json cancel 2>"$WORK/cancel2.err")"
check_eq "self-login cancel exits 0 when there is no dialog" "0" "$?"
if printf '%s' "$OUT" | grep -q '"cancelled": false'; then
  ok "self-login cancel is a no-op on a pane with no login dialog"
else
  bad "self-login cancel did not report cancelled:false (got: $OUT)"
fi

tmux kill-session -t "$SESSION" 2>/dev/null

# ---------------------------------------------------------------------------
# The PROACTIVE expiry detector, against a real terminal.
#
# The unit tests feed it synthetic strings. This feeds it a genuine tmux pane
# at a width narrow enough that Claude Code's warning really does hard-wrap,
# which is the case the whitespace-stripping matcher exists for and the one a
# literal match would silently fail.
#
# Everything here is read-only: `login-expiry` never injects, types, or opens
# a dialog, and the credential paths below are fixtures, not the real store.
# ---------------------------------------------------------------------------
WARNING_LINE="Your login expires in 2 days · run /login to renew"

# The wrap has to be forced from INSIDE the pane. `tmux new-session -x` is a
# request, not a guarantee — on a host with an attached client or a
# `default-size` setting it is quietly overridden, and a test that assumed a
# 24-column pane would sail through on an 80-column one having exercised
# nothing. So the fake reads its own real width and pads the line to start a
# few columns short of the right edge, which wraps the phrase MID-WORD at any
# terminal size.
cat > "$WORK/fake-warning-tui.sh" <<EOF
#!/usr/bin/env bash
clear
W=\$(tput cols 2>/dev/null || echo 80)
PAD=\$(printf '%*s' \$((W - 10)) '' | tr ' ' '.')
printf '%s%s\n' "\$PAD" "$WARNING_LINE"
printf "> \n"
printf "  bypass permissions on · 0 shells\n"
printf "  12345 tokens\n"
sleep 300
EOF
chmod +x "$WORK/fake-warning-tui.sh"

tmux new-session -d -s "$SESSION" -x 24 -y 12 "$WORK/fake-warning-tui.sh"
sleep 1.5

# Prove the wrap actually happened, so the assertion below means something.
if tmux capture-pane -t "$PANE" -p | grep -qF "$WARNING_LINE"; then
  bad "the warning fit on one pane line; the wrap-tolerant path was NOT exercised"
else
  ok "the warning is hard-wrapped on the pane (wrap-tolerant path exercised)"
fi

# --- 7. the warning is read back off a wrapped, real pane ---
OUT="$("$CW_BIN" login-expiry --pane "$PANE" \
        --credentials-file "$WORK/no-such-credentials.json" --json 2>&1)"
RC=$?
if printf '%s' "$OUT" | grep -q '"pane_warning_days":2'; then
  ok "login-expiry reads the wrapped warning off a real pane"
else
  bad "login-expiry did not report the wrapped warning (got: $OUT)"
fi
check_eq "login-expiry exits 3 on an uncorroborated pane warning" "3" "$RC"
if printf '%s' "$OUT" | grep -q '"credentials":"unknown"'; then
  ok "an unreadable credential store reports UNKNOWN, not healthy"
else
  bad "unreadable credential store was not reported as unknown (got: $OUT)"
fi

# --- 8. a healthy credential store VETOES the pane warning ---
#
# The guard that keeps the daemon from firing /login at the sentence "Your
# login expires in 2 days" merely appearing in conversation.
FAR="$(python3 -c 'import time; print(int((time.time()+90*86400)*1000))')"
printf '{"claudeAiOauth":{"refreshTokenExpiresAt":%s}}\n' "$FAR" \
  > "$WORK/healthy-credentials.json"
OUT="$("$CW_BIN" login-expiry --pane "$PANE" \
        --credentials-file "$WORK/healthy-credentials.json" --json 2>&1)"
RC=$?
check_eq "a healthy credential store vetoes the pane warning (exit 0)" "0" "$RC"
if printf '%s' "$OUT" | grep -q '"credentials":"healthy"'; then
  ok "the credential store is reported healthy despite the on-screen warning"
else
  bad "healthy credential store not reported (got: $OUT)"
fi

tmux kill-session -t "$SESSION" 2>/dev/null

# --- 9. an ordinary pane with no warning reports nothing ---
tmux new-session -d -s "$SESSION" -x 80 -y 24 "sleep 300"
sleep 1
OUT="$("$CW_BIN" login-expiry --pane "$PANE" \
        --credentials-file "$WORK/healthy-credentials.json" --json 2>&1)"
check_eq "login-expiry exits 0 on a pane with no warning" "0" "$?"
if printf '%s' "$OUT" | grep -q '"pane_warning_days":null'; then
  ok "no warning on the pane is reported as null, not as a number"
else
  bad "a pane with no warning did not report null (got: $OUT)"
fi

tmux kill-session -t "$SESSION" 2>/dev/null

# --- 10. nothing this suite did escaped into the real event queue ---
#
# Asserted rather than assumed: the failure mode is silent, and its blast
# radius is somebody else's production main loop reacting to a fixture.
if [ -d "$HOME/claude-events" ]; then
  ESCAPED="$(grep -rl "cw-self-login-test-" "$HOME/claude-events" 2>/dev/null | head -5)"
  if [ -n "$ESCAPED" ]; then
    bad "test events escaped into the real queue: $ESCAPED"
  else
    ok "no test events landed in the real claude-events queue"
  fi
else
  ok "no real claude-events queue on this host; nothing could escape"
fi
if [ -n "$(ls -A "$WORK/events" 2>/dev/null)" ]; then
  ok "test events were captured in the isolated queue"
fi

echo
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
