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
# WHY the code is typed with raw send-keys and NOT `claude-watch inject`.
#
# The original reason was that inject opened with an Escape blast and Escape
# cancels the login modal. That reason expired on 2026-08-18, when inject
# stopped sending the Escape unless `--escape` is passed. These checks pin the
# reasons that DID NOT expire, so the next person to look at self-login and
# think "inject is safe now, delete the raw path" gets a failing test instead
# of a login that silently authenticates with a corrupted code.
#
# There are TWO independent reasons, and they are probed separately below
# because they live at different stages of the choreography:
#
#   * The TYPING corrupts the payload (probe A, checks 4a-4c). inject's
#     INSERT-mode probe `i` and the configured FleetView focus keys are typed
#     into the modal ahead of the code. This is unchanged and is why the code
#     must never be routed through inject's typing path.
#
#   * The SUBMIT now refuses outright (probe B, check 4d). Since the prompt
#     line must hold our payload and nothing else before Enter, and a modal has
#     no prompt line at all, inject can never satisfy that gate in a modal — it
#     retracts and reports `prompt_dirty`.
#
# Probe A deliberately uses `--no-submit` and commits with a RAW tmux Enter.
# That is not a workaround for the new gate, it is what isolates the typing
# defects: `--no-submit` runs the identical type choreography (focus keys +
# `i` probe) but stops before the submit gate, so the corrupted bytes actually
# reach the fake's `read` and can be asserted on. Routing probe A through
# `--submit` would now block at the gate and deliver an EMPTY file, which
# would silently retire checks 4b and 4c rather than test them.
# ---------------------------------------------------------------------------
INJECT_HOME="$WORK/inject-probe-home"
mkdir -p "$INJECT_HOME"
# A config with a NON-EMPTY FleetView focus-to-main key sequence. Copied from
# the repo config so every other field keeps its real default (a partial TOML
# is rejected outright). the-host ships ten Up presses; one is enough to show
# the class. HOME is redirected too, so the user-config overlay cannot make
# this probe depend on whoever is running it.
sed 's/^focus_main_keys = \[\]/focus_main_keys = ["Up"]/' "$REPO/config.toml" \
  > "$WORK/inject-probe-config.toml"

PROBE_CODE="cw-inject-probe-code-9876"
cat > "$WORK/fake-code-prompt.sh" <<EOF
#!/usr/bin/env bash
printf "Paste code here if prompted > "
read -r code
printf "%s\n" "\$code" > "$WORK/inject-probe-received"
sleep 300
EOF
chmod +x "$WORK/fake-code-prompt.sh"

tmux new-session -d -s "$SESSION" -x 80 -y 24 "$WORK/fake-code-prompt.sh"
sleep 1.5

# --- probe A: the TYPING corrupts the payload ---------------------------
# `--no-submit` types the payload with the full choreography and stops short
# of the submit gate; a raw tmux Enter then commits whatever landed, so the
# fake's `read` observes exactly the bytes inject typed.
HOME="$INJECT_HOME" \
  CLAUDE_WATCH_CONFIG="$WORK/inject-probe-config.toml" \
  "$CW_BIN" inject --pane "$PANE" --submit "$PROBE_CODE" --no-submit \
  >/dev/null 2>"$WORK/inject-probe.err"
sleep 1
tmux send-keys -t "$PANE" Enter
sleep 1.5
PROBE_RECEIVED="$(cat "$WORK/inject-probe-received" 2>/dev/null)"

# --- 4a. inject does NOT deliver a modal code intact ---
if [ "$PROBE_RECEIVED" = "$PROBE_CODE" ]; then
  bad "claude-watch inject delivered the code into the modal INTACT — the raw send-keys path in self-login's do_code may now be replaceable; re-check by hand before deleting this test"
else
  ok "claude-watch inject corrupts a code typed into a login modal (raw send-keys path is still required)"
fi

# --- 4b. the corruption is the stray INSERT-mode probe `i` ---
#
# inject enters INSERT by sending one `i` and un-types it only if it can see
# the literal land on the prompt line. A modal shows no `-- INSERT --` and no
# prompt glyph, so the detection is ambiguous, inject fails open, and the `i`
# stays glued to the front of the payload.
case "$PROBE_RECEIVED" in
  *i"$PROBE_CODE") ok "the INSERT-probe \`i\` arrives as a literal prefix on the code" ;;
  *) bad "expected a literal \`i\` immediately before the code, got: $(printf '%s' "$PROBE_RECEIVED" | od -c | head -2)" ;;
esac

# --- 4c. the configured FleetView focus keys leak into the modal ---
case "$PROBE_RECEIVED" in
  *$'\033'*) ok "the FleetView focus-to-main keys leak into the modal as raw escape sequences" ;;
  *) bad "expected the focus_main_keys sequence in the received code, got: $(printf '%s' "$PROBE_RECEIVED" | od -c | head -2)" ;;
esac

tmux kill-session -t "$SESSION" 2>/dev/null

# --- probe B: a real `--submit` REFUSES rather than authenticating -------
#
# Until 2026-08-19 inject reported `status: submitted` over the corrupted
# payload, because its success check was "the payload cleared from the prompt
# line" and a modal has no prompt line — so a caller checking the exit code or
# status field learned nothing, which is what made this dangerous rather than
# merely wrong.
#
# Now the payload must BE the whole prompt line before Enter is pressed. A
# modal renders no prompt line, so that can never hold: inject backspaces its
# payload out and reports `prompt_dirty` / exit 4. The corrupted code is never
# committed. self-login still must not use inject here — but the failure mode
# is now a loud refusal instead of a silent bad authentication.
rm -f "$WORK/inject-probe-received"
tmux new-session -d -s "$SESSION" -x 80 -y 24 "$WORK/fake-code-prompt.sh"
sleep 1.5

INJECT_OUT="$(HOME="$INJECT_HOME" \
  CLAUDE_WATCH_CONFIG="$WORK/inject-probe-config.toml" \
  "$CW_BIN" inject --pane "$PANE" --submit "$PROBE_CODE" --json 2>"$WORK/inject-probe2.err")"
INJECT_RC=$?
sleep 1.5

check_eq "inject exits 4 (refused) when asked to submit into a modal" "4" "$INJECT_RC"

if printf '%s' "$INJECT_OUT" | grep -q '"status":"prompt_dirty"'; then
  ok "inject reports status=prompt_dirty in a modal (no prompt line can ever be exclusively ours)"
else
  bad "expected inject to report status=prompt_dirty (got: $INJECT_OUT)"
fi

# The field that made a reviewer misread the refusal as a submission. It must
# track the OUTCOME, not the `--no-submit` request flag.
if printf '%s' "$INJECT_OUT" | grep -q '"submitted":false'; then
  ok "a refused inject reports submitted:false (the JSON does not claim a submit it did not make)"
else
  bad "expected \"submitted\":false alongside prompt_dirty (got: $INJECT_OUT)"
fi

# The whole point: nothing reached the fake login.
PROBE2_RECEIVED="$(cat "$WORK/inject-probe-received" 2>/dev/null)"
if [ -z "$PROBE2_RECEIVED" ]; then
  ok "no code was committed to the modal by the refused inject"
else
  bad "a refused inject still delivered something to the modal: $(printf '%s' "$PROBE2_RECEIVED" | od -c | head -2)"
fi

tmux kill-session -t "$SESSION" 2>/dev/null

# ---------------------------------------------------------------------------
# The 2026-08-18 default flip, asserted from the modal's point of view: a
# plain `claude-watch inject` must leave a standing login dialog alone, and
# `--escape` must still be able to cancel it. This is what made self-login's
# `/login` submission safe to route through inject in the first place.
#
# The fake reads one keystroke at a time and drops back to the TUI on Escape,
# so "did the dialog survive" is a real observation, not a rendering guess.
#
# These two use the repo's stock config (focus_main_keys EMPTY), unlike the
# probes above. That is deliberate and it is not cheating: a byte-at-a-time
# reader sees an arrow key's `ESC [ A` as a bare Escape, so a non-empty
# focus-key sequence would trip this fake no matter what inject did, and the
# assertion would stop being about inject. A real TUI disambiguates the two;
# this fake cannot, so the focus keys are held out and their (real) damage is
# pinned by check 4c instead.
# ---------------------------------------------------------------------------
cat > "$WORK/fake-escape-sensitive.sh" <<EOF
#!/usr/bin/env bash
draw_login() {
  clear
  printf "Paste code here if prompted > "
}
draw_tui() {
  clear
  printf "> \n"
  printf "  bypass permissions on · 0 shells\n"
}
draw_login
while IFS= read -r -n1 -s key; do
  if [ "\$key" = \$'\e' ]; then
    draw_tui
    break
  fi
done
sleep 300
EOF
chmod +x "$WORK/fake-escape-sensitive.sh"

# --- 5a. default inject leaves the modal standing ---
tmux new-session -d -s "$SESSION" -x 80 -y 24 "$WORK/fake-escape-sensitive.sh"
sleep 1.5
HOME="$INJECT_HOME" CLAUDE_WATCH_CONFIG="$REPO/config.toml" \
  "$CW_BIN" inject --pane "$PANE" --submit 'probe' --no-submit >/dev/null 2>&1
sleep 1
if tmux capture-pane -t "$PANE" -p | grep -qF "Paste code here"; then
  ok "a default (no --escape) inject leaves a standing login modal alone"
else
  bad "a default inject CANCELLED the login modal — the 2026-08-18 flip regressed"
fi
tmux kill-session -t "$SESSION" 2>/dev/null

# --- 5b. --escape still cancels it (the behaviour, now opt-in) ---
tmux new-session -d -s "$SESSION" -x 80 -y 24 "$WORK/fake-escape-sensitive.sh"
sleep 1.5
HOME="$INJECT_HOME" CLAUDE_WATCH_CONFIG="$REPO/config.toml" \
  "$CW_BIN" inject --pane "$PANE" --submit 'probe' --no-submit --escape >/dev/null 2>&1
sleep 1
if tmux capture-pane -t "$PANE" -p | grep -qF "Paste code here"; then
  bad "--escape did NOT cancel the login modal; the cancelling path is not reachable"
else
  ok "--escape still reaches the Escape blast (cancels the modal), so the flag is a real opt-in"
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
