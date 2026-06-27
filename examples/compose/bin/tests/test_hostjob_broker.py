#!/usr/bin/env python3
"""Integration test for the hostjob live-tail line broker.

the runner had no pre-existing pytest dir when authored, so this is a
minimal stdlib ``unittest`` file dropped next to ``hostjob``. It exercises
the broker's publish -> subscribe contract end to end:

  1. Launch the broker on an ephemeral free port (NOT 8799, so it never
     collides with a live broker the operator may have running).
  2. Open a streaming SSE GET on /tail/<label> in a background thread.
  3. POST a few raw lines to /ingest/<label>.
  4. Assert every line is delivered to the subscriber over SSE.

Also covers late-subscriber backlog replay (POST before subscribe).

Run::

    python3 -m pytest test_hostjob_broker.py -v
    # or
    python3 test_hostjob_broker.py
"""

from __future__ import annotations

import importlib.util
import socket
import threading
import time
import unittest
import urllib.request
from importlib.machinery import SourceFileLoader
from pathlib import Path

HERE = Path(__file__).resolve().parent
HOSTJOB = HERE.parent / "hostjob"


def _load_hostjob():
    loader = SourceFileLoader("hostjob_under_test", str(HOSTJOB))
    spec = importlib.util.spec_from_loader("hostjob_under_test", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


def _free_port() -> int:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


class _Args:
    def __init__(self, port):
        self.port = port


class BrokerTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.mod = _load_hostjob()
        cls.port = _free_port()
        cls.base = "http://127.0.0.1:%d" % cls.port
        # Run the broker in a daemon thread (cmd_broker is a blocking
        # serve_forever loop). It binds 127.0.0.1:<ephemeral>.
        cls.thread = threading.Thread(
            target=cls.mod.cmd_broker, args=(_Args(cls.port),), daemon=True
        )
        cls.thread.start()
        # Wait for the port to answer /healthz.
        deadline = time.time() + 5
        while time.time() < deadline:
            try:
                with urllib.request.urlopen(cls.base + "/healthz", timeout=1) as r:
                    if r.read() == b"ok":
                        break
            except Exception:
                time.sleep(0.05)
        else:
            raise RuntimeError("broker did not come up on %s" % cls.base)

    def _ingest(self, label, line_bytes):
        req = urllib.request.Request(
            self.base + "/ingest/" + label,
            data=line_bytes,
            headers={"Content-Type": "application/octet-stream"},
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=3) as r:
            self.assertIn(r.status, (200, 204))

    def _collect(self, label, n, timeout=5):
        """Open an SSE tail, return the first n `data:` payloads."""
        out = []
        resp = urllib.request.urlopen(self.base + "/tail/" + label, timeout=timeout)

        def reader():
            for raw in resp:
                line = raw.decode("utf-8", "replace").rstrip("\n")
                if line.startswith("data: "):
                    out.append(line[len("data: "):])
                if len(out) >= n:
                    break

        t = threading.Thread(target=reader, daemon=True)
        t.start()
        return out, t, resp

    def test_publish_then_subscribe_live(self):
        label = "live-job"
        # Subscribe FIRST, then publish -> lines arrive live.
        out, t, resp = self._collect(label, 3)
        time.sleep(0.2)  # let the subscriber register
        self._ingest(label, b"alpha\n")
        self._ingest(label, b"beta\n")
        self._ingest(label, b"gamma\n")
        t.join(timeout=5)
        try:
            resp.close()
        except Exception:
            pass
        self.assertEqual(out[:3], ["alpha", "beta", "gamma"], out)

    def test_late_subscriber_gets_backlog(self):
        label = "backlog-job"
        # Publish BEFORE anyone subscribes -> ring buffers them.
        self._ingest(label, b"one\n")
        self._ingest(label, b"two\n")
        time.sleep(0.1)
        out, t, resp = self._collect(label, 2)
        t.join(timeout=5)
        try:
            resp.close()
        except Exception:
            pass
        self.assertEqual(out[:2], ["one", "two"], out)

    def test_bad_label_404(self):
        req = urllib.request.Request(
            self.base + "/ingest/bad..%2fslash", data=b"x", method="POST"
        )
        try:
            urllib.request.urlopen(req, timeout=3)
            self.fail("expected HTTPError for bad label")
        except urllib.error.HTTPError as e:
            self.assertEqual(e.code, 404)


if __name__ == "__main__":
    unittest.main(verbosity=2)
