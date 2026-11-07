"""Pushed remote hostjob logs: the session-task sink writes them and the
minisite resolves the tail path to them when the local runner dir lacks one."""
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
ST = HERE.parent / "tools" / "session-task" / "session-task"


class PushedLogTest(unittest.TestCase):
    def test_sink_then_resolve(self):
        with tempfile.TemporaryDirectory() as tmp:
            env = dict(os.environ, HOSTJOB_PUSHED_LOG_DIR=tmp + "/pushed",
                       HOME=tmp)
            r = subprocess.run([sys.executable, str(ST), "queue",
                                "hostjob-log-sink", "job1"],
                               input=b"a\nb\n", env=env, capture_output=True)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual((Path(tmp) / "pushed/job1/log").read_bytes(), b"a\nb\n")
            bad = subprocess.run([sys.executable, str(ST), "queue",
                                  "hostjob-log-sink", "../x"],
                                 input=b"", env=env, capture_output=True)
            self.assertEqual(bad.returncode, 2)
            os.environ["HOSTJOB_LOG_DIR"] = tmp + "/none"
            os.environ["HOSTJOB_PUSHED_LOG_DIR"] = tmp + "/pushed"
            sys.path.insert(0, str(HERE))
            import app
            self.assertEqual(app._hostjob_log_path("job1"),
                             Path(tmp) / "pushed/job1/log")


if __name__ == "__main__":
    unittest.main()
