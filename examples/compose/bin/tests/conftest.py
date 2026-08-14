"""pytest backstop: no test in this directory may touch operator state.

The per-file `hostjob_testkit.install()` calls are the primary isolation. This
autouse session fixture wraps the whole run in an OUTER isolation so a test
file that forgets to install one — or a helper that shells out before
`setUpClass` runs — still cannot reach the operator's real
`session-task` queue or `claude-event` stream. Cheap: one tempdir per pytest
session.

Note the outer layer deliberately does NOT touch any `hostjob` module's
``STATE_ROOT`` (there is no module to point at here); it isolates ``$PATH``
and ``$HOME`` only, which is what keeps the shell-outs contained.
"""

from __future__ import annotations

import pytest

import hostjob_testkit


@pytest.fixture(scope="session", autouse=True)
def _isolate_operator_state():
    iso = hostjob_testkit.install(prefix="hostjob-test-session-")
    try:
        yield iso
    finally:
        iso.teardown()
