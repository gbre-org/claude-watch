#!/usr/bin/env python3
"""cw-clear mod path selection in self-clear (slice 1).

Pure path-selection tests (fresh heartbeat -> mod; stale / missing / not ready
/ flag off -> tmux), config parsing defaults, and child_main() end to end with
the tmux layer mocked: the mod path types /cw-clear and never /clear, and a
mod that fails to clear falls back to the unchanged tmux path.

Run: python3 tools/watchers/tests/test_self_clear_mod_path.py
"""
import importlib.machinery
import importlib.util
import json
import os
import tempfile
import types
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
SCRIPT = REPO / "tools" / "watchers" / "self-clear"
SETUP = REPO / "mods" / "cw-clear" / "cw-clear-setup"
IDLE = "some output\n❯ \n"
NOW = 1_800_000_000.0


def load(path, name):
    loader = importlib.machinery.SourceFileLoader(name, str(path))
    spec = importlib.util.spec_from_loader(name, loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


def hb(age=5, **kw):
    d = {"ts": int((NOW - age) * 1000), "event": "tick", "busy": False,
         "turns": 3, "ready": True}
    d.update(kw)
    return d


CFG = {"enabled": True, "heartbeat_max_age_secs": 60.0}


class DecidePath(unittest.TestCase):
    def setUp(self):
        self.m = load(SCRIPT, "sc_mod_decide")

    def test_fresh_heartbeat_selects_mod(self):
        use, reason = self.m.decide_clear_path(CFG, hb(), NOW)
        self.assertTrue(use, reason)

    def test_seconds_timestamp_also_accepted(self):
        use, _ = self.m.decide_clear_path(CFG, hb(ts=NOW - 5), NOW)
        self.assertTrue(use)

    def test_stale_heartbeat_selects_tmux(self):
        use, reason = self.m.decide_clear_path(CFG, hb(age=300), NOW)
        self.assertFalse(use)
        self.assertIn("stale", reason)

    def test_missing_heartbeat_selects_tmux(self):
        use, reason = self.m.decide_clear_path(CFG, None, NOW)
        self.assertFalse(use)
        self.assertIn("missing", reason)

    def test_flag_off_selects_tmux_even_with_fresh_heartbeat(self):
        use, reason = self.m.decide_clear_path({**CFG, "enabled": False}, hb(), NOW)
        self.assertFalse(use)
        self.assertEqual(reason, "flag-off")

    def test_not_ready_and_session_end_select_tmux(self):
        self.assertFalse(self.m.decide_clear_path(CFG, hb(ready=False), NOW)[0])
        self.assertFalse(self.m.decide_clear_path(CFG, hb(ready=None), NOW)[0])
        self.assertFalse(self.m.decide_clear_path(CFG, hb(event="session.end"), NOW)[0])

    def test_garbage_timestamps_select_tmux(self):
        for ts in ("now", None, True, [1]):
            self.assertFalse(self.m.decide_clear_path(CFG, hb(ts=ts), NOW)[0], ts)

    def test_future_heartbeat_selects_tmux(self):
        self.assertFalse(self.m.decide_clear_path(CFG, hb(age=-600), NOW)[0])

    def test_custom_max_age(self):
        cfg = {**CFG, "heartbeat_max_age_secs": 10.0}
        self.assertFalse(self.m.decide_clear_path(cfg, hb(age=30), NOW)[0])
        self.assertTrue(self.m.decide_clear_path(cfg, hb(age=3), NOW)[0])


class ConfigRead(unittest.TestCase):
    def setUp(self):
        self.m = load(SCRIPT, "sc_mod_cfg")
        self.d = tempfile.TemporaryDirectory()
        self.addCleanup(self.d.cleanup)

    def cfg_with(self, toml):
        p = Path(self.d.name) / "config.toml"
        p.write_text(toml)
        self.m._find_config_file = lambda: str(p)
        return self.m.read_cw_clear_mod_config()

    def test_default_is_enabled(self):
        # Mod mode is the default; the fresh-heartbeat gate still decides.
        self.m._find_config_file = lambda: None
        self.assertTrue(self.m.read_cw_clear_mod_config()["enabled"])
        self.assertTrue(self.cfg_with("[tmux]\nx = 1\n")["enabled"])
        self.assertFalse(self.cfg_with("[cw_clear_mod]\nenabled = false\n")["enabled"])

    def test_enabled_and_overrides(self):
        c = self.cfg_with('[cw_clear_mod]\nenabled = true\nheartbeat_file = "/x/hb"\n'
                          'resume_file = "/x/r"\nheartbeat_max_age_secs = 20\n')
        self.assertTrue(c["enabled"])
        self.assertEqual((c["heartbeat_file"], c["resume_file"]), ("/x/hb", "/x/r"))
        self.assertEqual(c["heartbeat_max_age_secs"], 20.0)

    def test_non_bool_enabled_and_bad_toml_fail_closed(self):
        self.assertFalse(self.cfg_with('[cw_clear_mod]\nenabled = "yes"\n')["enabled"])
        self.assertFalse(self.cfg_with("[cw_clear_mod\nenabled = true")["enabled"])


class ChildMainPaths(unittest.TestCase):
    """Run child_main with tmux mocked; assert which keystrokes were typed."""

    def wire(self, heartbeat, enabled=True, tokens=None, resume_turn=True):
        m = load(SCRIPT, "sc_mod_child")
        d = tempfile.TemporaryDirectory()
        self.addCleanup(d.cleanup)
        hb_path = Path(d.name) / "hb.json"
        resume_path = Path(d.name) / "resume.txt"
        if heartbeat is not None:
            hb_path.write_text(json.dumps(heartbeat))
        cfg = {"enabled": enabled, "heartbeat_file": str(hb_path),
               "resume_file": str(resume_path), "heartbeat_max_age_secs": 60.0}
        m.read_cw_clear_mod_config = lambda: cfg
        m.HANDOFF_FILE = str(Path(d.name) / "handoff")
        m.RESUME_PROMPT = "RESUME-TEXT"

        clock = [NOW]
        m.time = types.SimpleNamespace(
            time=lambda: clock[0],
            sleep=lambda s=0: clock.__setitem__(0, clock[0] + s))
        readings = list(tokens or [900000, 40000])

        def get_token_count():
            clock[0] += 1.0
            return readings.pop(0) if len(readings) > 1 else readings[0]

        typed, logs = [], []
        m.get_token_count = get_token_count
        m.log = logs.append
        m.run = lambda cmd, *a, **k: (IDLE, 0)
        m.capture_pane_text = lambda pane: "idle"
        m.ensure_main_loop_focus = lambda pane: True
        m.cancel_stray_rewind_picker = lambda pane: False
        m.interrupt_and_wait = lambda pane, timeout=60: True
        m.ensure_no_rewind = lambda pane, **k: True
        m.dismiss_feedback_prompt = lambda pane: None
        m.wait_for_idle = lambda pane, timeout=60: True

        def inject(pane, text, label="inject", slash_command=False, escape=False):
            typed.append(text)
            # The mod "submits the resume" by bumping turns in the heartbeat.
            if text == "/cw-clear" and resume_turn and heartbeat is not None:
                hb_path.write_text(json.dumps({**heartbeat, "turns": heartbeat["turns"] + 1}))
        m.inject = inject
        return m, typed, logs, resume_path

    def args(self):
        return types.SimpleNamespace(timeout=5, delay=5, no_resume=False)

    def test_fresh_heartbeat_uses_mod_and_never_types_clear(self):
        m, typed, logs, resume = self.wire(hb(age=0), tokens=[900000, 40000])
        m.child_main("%1", self.args())
        self.assertEqual(typed, ["/cw-clear"])
        self.assertEqual(resume.read_text(), "RESUME-TEXT")
        self.assertTrue(any(l.startswith("PATH-USED: mod") for l in logs), logs)
        self.assertTrue(os.path.exists(m.HANDOFF_FILE))

    def test_stale_heartbeat_uses_tmux_path_unchanged(self):
        m, typed, logs, _ = self.wire(hb(age=600), tokens=[900000, 40000])
        m.child_main("%1", self.args())
        self.assertEqual(typed, ["/clear", "RESUME-TEXT"])
        self.assertTrue(any(l.startswith("PATH-USED: tmux (heartbeat-stale") for l in logs), logs)

    def test_missing_heartbeat_uses_tmux(self):
        m, typed, logs, _ = self.wire(None, tokens=[900000, 40000])
        m.child_main("%1", self.args())
        self.assertEqual(typed, ["/clear", "RESUME-TEXT"])
        self.assertTrue(any("PATH-USED: tmux (heartbeat-missing" in l for l in logs), logs)

    def test_flag_off_uses_tmux(self):
        m, typed, logs, _ = self.wire(hb(age=0), enabled=False, tokens=[900000, 40000])
        m.child_main("%1", self.args())
        self.assertEqual(typed, ["/clear", "RESUME-TEXT"])
        self.assertTrue(any("PATH-USED: tmux (flag-off" in l for l in logs), logs)

    def test_mod_command_unavailable_falls_back_to_tmux(self):
        # /cw-clear typed but the session never shrinks (unknown command);
        # the tmux path then clears normally.
        m, typed, logs, _ = self.wire(hb(age=0), tokens=[900000] * 14 + [40000], resume_turn=False)
        m.child_main("%1", self.args())
        self.assertEqual(typed[0], "/cw-clear")
        self.assertIn("/clear", typed)
        self.assertEqual(typed[-1], "RESUME-TEXT")
        self.assertTrue(any("PATH-USED: tmux (mod-failed" in l for l in logs), logs)

    def test_mod_cleared_but_no_resume_turn_injects_resume_over_tmux(self):
        m, typed, logs, _ = self.wire(hb(age=0), tokens=[900000, 40000], resume_turn=False)
        m.child_main("%1", self.args())
        self.assertEqual(typed, ["/cw-clear", "RESUME-TEXT"])
        self.assertTrue(any("PATH-USED: mod-clear+tmux-resume" in l for l in logs), logs)


class SetupScript(unittest.TestCase):
    def test_merge_preserves_existing_and_is_idempotent(self):
        s = load(SETUP, "cw_clear_setup")
        base = {"permissions": {"allow": ["Bash(ls:*)"]},
                "pluginConfigs": {"other": {"options": {"a": 1}},
                                  "cw-clear": {"options": {"probe": False}}}}
        once = s.merge_settings(base, "/h", "/r")
        self.assertEqual(once["permissions"], base["permissions"])
        self.assertEqual(once["pluginConfigs"]["other"], {"options": {"a": 1}})
        self.assertEqual(once["pluginConfigs"]["cw-clear"]["options"],
                         {"probe": False, "enabled": True, "stateFile": "/h", "resumeFile": "/r"})
        self.assertEqual(s.merge_settings(once, "/h", "/r"), once)
        self.assertNotIn("cw-clear", base["pluginConfigs"]["other"])

    def test_apply_dry_run_does_not_write(self):
        s = load(SETUP, "cw_clear_setup2")
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "settings.json"
            p.write_text('{"x": 1}')
            self.assertEqual(s.main(["--apply", "--settings", str(p), "--dry-run"]), 0)
            self.assertEqual(json.loads(p.read_text()), {"x": 1})
            self.assertEqual(s.main(["--apply", "--settings", str(p)]), 0)
            self.assertTrue(json.loads(p.read_text())["pluginConfigs"]["cw-clear"]["options"]["enabled"])
            p.write_text("not json")
            self.assertEqual(s.main(["--apply", "--settings", str(p)]), 1)
            self.assertEqual(p.read_text(), "not json")


if __name__ == "__main__":
    unittest.main()
