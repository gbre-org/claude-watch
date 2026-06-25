#!/usr/bin/env python3
"""Tests for pingme lifecycle notifications on session-task queue ops.

Covers the 2026-04-19 addition:

  * `queue register <id>` shells out to ``pingme`` with a
    "queue started: <id>" title + "scope: ... | priority: N" message.
  * `queue done <id>` shells out to ``pingme`` with a
    "queue done: <id>" title + "elapsed: Xm | scope: ..." message.
  * `queue abandon <id> [--reason R]` shells out to ``pingme`` with a
    "queue abandoned: <id>" title + "elapsed: Xm | reason: R | scope: ..."
    message.
  * ``--silent`` on any of the three suppresses the pingme call.
  * ``PINGME_SESSION_TASK=0`` env var suppresses the pingme call.
  * When ``pingme`` is not on PATH, operations still succeed (silent
    no-op, no error raised).
  * A failing ``pingme`` does NOT fail the queue operation.

Also covers the 2026-04-19 one-line summary field:

  * `queue add --summary "..."` stores the headline on the item.
  * Missing `--summary` warns to stderr and defaults to ``"(no summary)"``.
  * Summary appears on line 1 of the pingme message body (so it
    surfaces as the push-notification preview on most providers).
  * `queue show` / `queue list` surface the summary for visibility.
  * `queue set-summary <id> "text"` retrofits or edits a summary.

The tests install a fake ``pingme`` shim into a temporary PATH-only
directory. The shim appends its argv to a log file we can then assert
against. Tests pass without real pingme installed and never actually
fire a push notification.

Run:
    uv run --python 3.11 --with pytest pytest tests/test_queue_pingme.py -v

Or directly:
    python3 tests/test_queue_pingme.py
"""

import json
import os
import subprocess
import sys
import tempfile
import textwrap
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


# ---------------------------------------------------------------------------
# Fake pingme shim
# ---------------------------------------------------------------------------


def _install_fake_pingme(bin_dir: Path, log_path: Path, exit_code: int = 0):
    """Drop a fake ``queue-notify`` executable that logs argv to a file.

    session-task's queue lifecycle hooks shell out to ``queue-notify`` (a
    dedicated Pushover path), so the shim is named ``queue-notify``. The
    helper keeps its historical ``_install_fake_pingme`` name to minimise
    churn across the existing test suite.

    Format of each logged call: one JSON object per line with keys
    ``priority`` (or None), ``message``, ``title``. The shim writes the
    argv verbatim; we parse argparse-style in the test.
    """
    bin_dir.mkdir(parents=True, exist_ok=True)
    notifier = bin_dir / "queue-notify"
    # Keep the shim minimal: log all argv after argv[0], then exit.
    notifier.write_text(textwrap.dedent(f"""\
        #!/usr/bin/env python3
        import json, sys
        with open({str(log_path)!r}, "a") as f:
            f.write(json.dumps(sys.argv[1:]) + "\\n")
        sys.exit({exit_code})
        """))
    notifier.chmod(0o755)
    return notifier


def _env_for_tmp(tmp, extra_path_dir=None, pingme_session_task=None):
    """Build env with HOME=tmp and (optionally) extra dir prepended to PATH.

    If ``extra_path_dir`` is None, PATH is set to a single empty dir so
    ``queue-notify`` cannot be found -- used for the no-op test. Otherwise
    the extra dir (containing the shim) is prepended.
    """
    env = dict(os.environ)
    env["HOME"] = str(tmp)
    if extra_path_dir is None:
        empty = Path(tmp) / ".empty_path"
        empty.mkdir(parents=True, exist_ok=True)
        env["PATH"] = str(empty)
    else:
        env["PATH"] = f"{extra_path_dir}:{env.get('PATH', '')}"
    if pingme_session_task is not None:
        env["PINGME_SESSION_TASK"] = pingme_session_task
    elif "PINGME_SESSION_TASK" in env:
        # Don't let the outer env's setting bleed into tests.
        del env["PINGME_SESSION_TASK"]
    Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)
    return env


def _run(env, *argv, check=False, timeout=15):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    r = subprocess.run(cmd, capture_output=True, text=True, env=env,
                       timeout=timeout)
    if check and r.returncode != 0:
        raise RuntimeError(
            f"command failed rc={r.returncode}\n"
            f"  cmd: {' '.join(argv)}\n"
            f"  stdout: {r.stdout}\n"
            f"  stderr: {r.stderr}"
        )
    return r


def _add(env, desc, scopes, *extra):
    cmd = ["queue", "add", desc, "--json"]
    for s in scopes:
        cmd.extend(["--scope", s])
    cmd.extend(extra)
    return _run(env, *cmd)


def _read_pingme_log(log_path: Path):
    """Return list of argv lists from the shim log, or [] if absent."""
    if not log_path.exists():
        return []
    out = []
    for line in log_path.read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        out.append(json.loads(line))
    return out


def _parse_pingme_argv(argv):
    """Mirror pingme's parser: ``pingme [-p PRIORITY] <message> [title]``.

    Returns dict with keys priority/message/title.
    """
    priority = "normal"
    i = 0
    positional = []
    while i < len(argv):
        a = argv[i]
        if a in ("-p", "--priority"):
            priority = argv[i + 1]
            i += 2
            continue
        positional.append(a)
        i += 1
    message = positional[0] if len(positional) >= 1 else None
    title = positional[1] if len(positional) >= 2 else None
    return {"priority": priority, "message": message, "title": title}


# ---------------------------------------------------------------------------
# 1. register fires pingme with correct shape
# ---------------------------------------------------------------------------


def test_register_fires_pingme_with_start_payload():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        log = Path(tmp) / "pingme.log"
        _install_fake_pingme(bin_dir, log)
        env = _env_for_tmp(tmp, extra_path_dir=bin_dir)

        r1 = _add(env, "register-test", ["repo:pingme-start"])
        d1 = json.loads(r1.stdout)

        rr = _run(env, "queue", "register", d1["id"], "--json", check=True)
        assert rr.returncode == 0

        calls = _read_pingme_log(log)
        assert len(calls) == 1, calls
        parsed = _parse_pingme_argv(calls[0])
        assert parsed["title"] == f"queue started: {d1['id']}"
        assert "repo:pingme-start" in parsed["message"]
        assert "priority: 5" in parsed["message"]  # default priority
        assert parsed["priority"] == "normal"


# ---------------------------------------------------------------------------
# 2. done fires pingme with completion payload + elapsed minutes
# ---------------------------------------------------------------------------


def test_done_fires_pingme_with_done_payload():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        log = Path(tmp) / "pingme.log"
        _install_fake_pingme(bin_dir, log)
        env = _env_for_tmp(tmp, extra_path_dir=bin_dir)

        r1 = _add(env, "done-test", ["repo:pingme-done"])
        d1 = json.loads(r1.stdout)
        _run(env, "queue", "register", d1["id"], "--json", check=True)
        _run(env, "queue", "done", d1["id"], check=True)

        calls = _read_pingme_log(log)
        # two calls total: register + done
        assert len(calls) == 2, calls
        parsed = _parse_pingme_argv(calls[1])
        assert parsed["title"] == f"queue done: {d1['id']}"
        assert "elapsed:" in parsed["message"]
        assert parsed["message"].endswith(
            f"scope: {d1['scope'][0]}"
        ) or "repo:pingme-done" in parsed["message"]
        assert parsed["priority"] == "normal"


# ---------------------------------------------------------------------------
# 3. abandon fires pingme with failure payload + reason
# ---------------------------------------------------------------------------


def test_abandon_fires_pingme_with_abandon_payload():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        log = Path(tmp) / "pingme.log"
        _install_fake_pingme(bin_dir, log)
        env = _env_for_tmp(tmp, extra_path_dir=bin_dir)

        r1 = _add(env, "abandon-test", ["repo:pingme-abandon"])
        d1 = json.loads(r1.stdout)
        _run(env, "queue", "register", d1["id"], "--json", check=True)
        _run(env, "queue", "abandon", d1["id"], "--reason",
             "agent crashed", check=True)

        calls = _read_pingme_log(log)
        # register + abandon
        assert len(calls) == 2, calls
        parsed = _parse_pingme_argv(calls[1])
        assert parsed["title"] == f"queue abandoned: {d1['id']}"
        assert "elapsed:" in parsed["message"]
        assert "agent crashed" in parsed["message"]
        assert "repo:pingme-abandon" in parsed["message"]
        assert parsed["priority"] == "normal"


def test_abandon_without_reason_has_placeholder():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        log = Path(tmp) / "pingme.log"
        _install_fake_pingme(bin_dir, log)
        env = _env_for_tmp(tmp, extra_path_dir=bin_dir)

        r1 = _add(env, "abandon-no-reason", ["repo:pingme-abandon-nr"])
        d1 = json.loads(r1.stdout)
        _run(env, "queue", "register", d1["id"], "--json", check=True)
        _run(env, "queue", "abandon", d1["id"], check=True)

        calls = _read_pingme_log(log)
        parsed = _parse_pingme_argv(calls[1])
        assert "no reason given" in parsed["message"]


# ---------------------------------------------------------------------------
# 4. --silent suppresses
# ---------------------------------------------------------------------------


def test_silent_flag_suppresses_all_three_hooks():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        log = Path(tmp) / "pingme.log"
        _install_fake_pingme(bin_dir, log)
        env = _env_for_tmp(tmp, extra_path_dir=bin_dir)

        r1 = _add(env, "silent-register", ["repo:silent-a"])
        d1 = json.loads(r1.stdout)
        _run(env, "queue", "register", d1["id"], "--silent", "--json",
             check=True)
        _run(env, "queue", "done", d1["id"], "--silent", check=True)

        r2 = _add(env, "silent-abandon", ["repo:silent-b"])
        d2 = json.loads(r2.stdout)
        _run(env, "queue", "register", d2["id"], "--silent", "--json",
             check=True)
        _run(env, "queue", "abandon", d2["id"], "--silent", "--reason",
             "silenced", check=True)

        calls = _read_pingme_log(log)
        assert calls == [], f"expected no pingme calls, got {calls}"


# ---------------------------------------------------------------------------
# 5. PINGME_SESSION_TASK=0 suppresses
# ---------------------------------------------------------------------------


def test_env_var_suppresses_pingme():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        log = Path(tmp) / "pingme.log"
        _install_fake_pingme(bin_dir, log)
        env = _env_for_tmp(tmp, extra_path_dir=bin_dir,
                           pingme_session_task="0")

        r1 = _add(env, "envmute", ["repo:envmute"])
        d1 = json.loads(r1.stdout)
        _run(env, "queue", "register", d1["id"], "--json", check=True)
        _run(env, "queue", "done", d1["id"], check=True)

        calls = _read_pingme_log(log)
        assert calls == [], f"expected no pingme calls, got {calls}"


# ---------------------------------------------------------------------------
# 6. pingme-not-on-PATH is a silent no-op (no crash, queue op succeeds)
# ---------------------------------------------------------------------------


def test_missing_pingme_is_silent_noop():
    with tempfile.TemporaryDirectory() as tmp:
        # extra_path_dir=None -> PATH is an empty dir, pingme not findable.
        env = _env_for_tmp(tmp, extra_path_dir=None)

        r1 = _add(env, "no-pingme", ["repo:no-pingme"])
        d1 = json.loads(r1.stdout)
        # register + done + abandon cycles must all succeed.
        rr = _run(env, "queue", "register", d1["id"], "--json")
        assert rr.returncode == 0, rr.stderr
        rd = _run(env, "queue", "done", d1["id"])
        assert rd.returncode == 0, rd.stderr

        # Another item: register + abandon
        r2 = _add(env, "no-pingme-2", ["repo:no-pingme-2"])
        d2 = json.loads(r2.stdout)
        rr2 = _run(env, "queue", "register", d2["id"], "--json")
        assert rr2.returncode == 0, rr2.stderr
        ra = _run(env, "queue", "abandon", d2["id"])
        assert ra.returncode == 0, ra.stderr


# ---------------------------------------------------------------------------
# 7. failing pingme does not block the queue op
# ---------------------------------------------------------------------------


def test_failing_pingme_does_not_block_queue_op():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        log = Path(tmp) / "pingme.log"
        _install_fake_pingme(bin_dir, log, exit_code=17)
        env = _env_for_tmp(tmp, extra_path_dir=bin_dir)

        r1 = _add(env, "failing-pingme", ["repo:failing-pingme"])
        d1 = json.loads(r1.stdout)
        rr = _run(env, "queue", "register", d1["id"], "--json")
        assert rr.returncode == 0, rr.stderr
        rd = _run(env, "queue", "done", d1["id"])
        assert rd.returncode == 0, rd.stderr

        # Shim still logged its argv before exiting non-zero.
        calls = _read_pingme_log(log)
        assert len(calls) == 2, calls


# ---------------------------------------------------------------------------
# 8. register --if-absent no-op (already running) does NOT re-fire pingme
# ---------------------------------------------------------------------------


def test_if_absent_register_noop_does_not_fire_pingme_again():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        log = Path(tmp) / "pingme.log"
        _install_fake_pingme(bin_dir, log)
        env = _env_for_tmp(tmp, extra_path_dir=bin_dir)

        r1 = _add(env, "if-absent-test", ["repo:if-absent-test"])
        d1 = json.loads(r1.stdout)
        _run(env, "queue", "register", d1["id"], "--json", check=True)
        # Second register --if-absent: should be a no-op (already running).
        _run(env, "queue", "register", d1["id"], "--if-absent", check=True)

        calls = _read_pingme_log(log)
        assert len(calls) == 1, (
            f"if-absent re-register should not fire pingme again, got: {calls}"
        )


# ---------------------------------------------------------------------------
# 9. --summary appears in pingme register message (line 1, push-notification preview)
# ---------------------------------------------------------------------------


def test_summary_appears_in_pingme_register_message():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        log = Path(tmp) / "pingme.log"
        _install_fake_pingme(bin_dir, log)
        env = _env_for_tmp(tmp, extra_path_dir=bin_dir)

        summary_text = "roll the search index after ebook ingest"
        r1 = _add(env, "register-with-summary", ["repo:summary-a"],
                  "--summary", summary_text)
        d1 = json.loads(r1.stdout)

        _run(env, "queue", "register", d1["id"], "--json", check=True)
        _run(env, "queue", "done", d1["id"], check=True)

        calls = _read_pingme_log(log)
        assert len(calls) == 2, calls

        # register pingme: summary on line 1 (push-notification preview), scope on line 2
        reg = _parse_pingme_argv(calls[0])
        assert reg["title"] == f"queue started: {d1['id']}"
        first_line = (reg["message"] or "").split("\n", 1)[0]
        assert first_line == summary_text, (
            f"expected summary on first line of register message, got: {reg['message']!r}"
        )
        assert "repo:summary-a" in reg["message"]

        # done pingme: summary on line 1, elapsed/scope on line 2
        done = _parse_pingme_argv(calls[1])
        assert done["title"] == f"queue done: {d1['id']}"
        done_first = (done["message"] or "").split("\n", 1)[0]
        assert done_first == summary_text
        assert "elapsed:" in done["message"]


# ---------------------------------------------------------------------------
# 10. omitting --summary warns on stderr and defaults to placeholder
# ---------------------------------------------------------------------------


def test_missing_summary_warns_and_defaults():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        log = Path(tmp) / "pingme.log"
        _install_fake_pingme(bin_dir, log)
        env = _env_for_tmp(tmp, extra_path_dir=bin_dir)

        r1 = _add(env, "no-summary-test", ["repo:nosummary"])
        # Warning must be on stderr, not stdout (stdout is the JSON payload).
        assert "no --summary provided" in r1.stderr, (
            f"expected stderr warning, got stderr={r1.stderr!r}"
        )
        assert r1.returncode == 0, r1.stderr  # still accepts the add
        d1 = json.loads(r1.stdout)

        _run(env, "queue", "register", d1["id"], "--json", check=True)

        calls = _read_pingme_log(log)
        assert len(calls) == 1, calls
        parsed = _parse_pingme_argv(calls[0])
        # Placeholder surfaces verbatim as the push-notification preview.
        first_line = (parsed["message"] or "").split("\n", 1)[0]
        assert first_line == "(no summary)", (
            f"expected '(no summary)' preview, got: {parsed['message']!r}"
        )


# ---------------------------------------------------------------------------
# 11. summary round-trips via queue show (stored in queue.json)
# ---------------------------------------------------------------------------


def test_summary_roundtrips_via_queue_show():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp, extra_path_dir=None)  # no pingme needed

        summary_text = "translate chapter 3 glossary check"
        r1 = _add(env, "roundtrip", ["repo:roundtrip"],
                  "--summary", summary_text)
        d1 = json.loads(r1.stdout)

        r2 = _run(env, "queue", "show", d1["id"], check=True)
        shown = json.loads(r2.stdout)
        assert shown.get("summary") == summary_text, (
            f"expected summary to round-trip, got: {shown!r}"
        )

        # And list output should render it.
        r3 = _run(env, "queue", "list", check=True)
        assert summary_text in r3.stdout, (
            f"expected summary in queue list output, got: {r3.stdout!r}"
        )


# ---------------------------------------------------------------------------
# 12. queue set-summary updates an existing item + shows up in pingme
# ---------------------------------------------------------------------------


def test_queue_set_summary_updates_and_surfaces_in_pingme():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        log = Path(tmp) / "pingme.log"
        _install_fake_pingme(bin_dir, log)
        env = _env_for_tmp(tmp, extra_path_dir=bin_dir)

        # Add without summary, then retrofit via set-summary.
        r1 = _add(env, "set-summary-test", ["repo:setsum"])
        d1 = json.loads(r1.stdout)

        new_sum = "retrofitted one-line summary for old item"
        rs = _run(env, "queue", "set-summary", d1["id"], new_sum, check=True)
        assert "summary updated" in rs.stdout

        # Confirm queue show reflects it, and summary_updated_at is recorded.
        rshow = _run(env, "queue", "show", d1["id"], check=True)
        shown = json.loads(rshow.stdout)
        assert shown.get("summary") == new_sum
        assert "summary_updated_at" in shown

        # Register -> pingme message should use the NEW summary, not placeholder.
        _run(env, "queue", "register", d1["id"], "--json", check=True)
        calls = _read_pingme_log(log)
        assert len(calls) == 1, calls
        parsed = _parse_pingme_argv(calls[0])
        first_line = (parsed["message"] or "").split("\n", 1)[0]
        assert first_line == new_sum, (
            f"expected updated summary in pingme preview, got: {parsed['message']!r}"
        )

        # set-summary with empty string is refused.
        bad = _run(env, "queue", "set-summary", d1["id"], "")
        assert bad.returncode != 0
        assert "non-empty" in bad.stderr


# ---------------------------------------------------------------------------
# Entry point for direct invocation
# ---------------------------------------------------------------------------


def _all_tests():
    return [
        test_register_fires_pingme_with_start_payload,
        test_done_fires_pingme_with_done_payload,
        test_abandon_fires_pingme_with_abandon_payload,
        test_abandon_without_reason_has_placeholder,
        test_silent_flag_suppresses_all_three_hooks,
        test_env_var_suppresses_pingme,
        test_missing_pingme_is_silent_noop,
        test_failing_pingme_does_not_block_queue_op,
        test_if_absent_register_noop_does_not_fire_pingme_again,
        test_summary_appears_in_pingme_register_message,
        test_missing_summary_warns_and_defaults,
        test_summary_roundtrips_via_queue_show,
        test_queue_set_summary_updates_and_surfaces_in_pingme,
    ]


if __name__ == "__main__":
    fail = 0
    for t in _all_tests():
        try:
            t()
            print(f"PASS: {t.__name__}")
        except Exception as e:
            fail += 1
            print(f"FAIL: {t.__name__}: {e}")
    sys.exit(0 if fail == 0 else 1)
