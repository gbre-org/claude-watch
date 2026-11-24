"""Tests for hostjob-remote: registry, role selection, 0600, mocked transport, no token leaks."""
import os
import stat
from importlib.machinery import SourceFileLoader
from importlib.util import module_from_spec, spec_from_loader

import pytest

_P = os.path.join(os.path.dirname(__file__), "..", "hostjob-remote")
_spec = spec_from_loader("hostjob_remote", SourceFileLoader("hostjob_remote", _P))
hr = module_from_spec(_spec)
_spec.loader.exec_module(hr)

TOKEN = "s3cr3t-token-value"


@pytest.fixture
def reg(tmp_path, monkeypatch):
    cred = tmp_path / "cred"
    cred.write_text('{"token": "%s"}' % TOKEN)
    cred.chmod(0o600)
    p = tmp_path / "hosts.toml"
    p.write_text(f'''
[hosts.a]
roles = ["build", "synth"]
endpoint = "http://x/mcp"
credential_file = "{cred}"
hostjob = "/r/hostjob"
cwd = "/r/work"
[hosts.b]
roles = ["build"]
endpoint = "http://y/mcp"
credential_file = "{cred}"
[hosts.lap]
roles = ["laptop", "build2"]
endpoint = "http://z/mcp"
credential_file = "{cred}"
interactive_only = true
''')
    monkeypatch.setenv("HOSTJOB_HOSTS", str(p))
    return p, cred


class Fake(hr.Transport):
    def __init__(self, replies):
        self.replies, self.cmds = list(replies), []

    def exec(self, command, timeout=60):
        self.cmds.append(command)
        return self.replies.pop(0)


def test_registry_and_roles(reg):
    hosts = hr.load_registry()
    assert list(hosts) == ["a", "b", "lap"]
    assert hr.select_host(hosts, role="build").name == "a"
    assert hr.select_host(hosts, role="synth").name == "a"
    assert hr.select_host(hosts, name="lap").interactive_only
    with pytest.raises(hr.RegistryError, match="interactive_only"):
        hr.select_host(hosts, role="laptop")
    with pytest.raises(hr.RegistryError):
        hr.select_host(hosts, name="nope")
    with pytest.raises(hr.RegistryError):
        hr.select_host(hosts)
    with pytest.raises(hr.RegistryError):
        hr.select_host(hosts, name="a", role="build")


def test_registry_errors(tmp_path, monkeypatch):
    monkeypatch.setenv("HOSTJOB_HOSTS", str(tmp_path / "missing.toml"))
    with pytest.raises(hr.RegistryError, match="not found"):
        hr.load_registry()
    bad = tmp_path / "bad.toml"
    bad.write_text("[hosts.x]\nroles = ['r']\n")  # no endpoint
    with pytest.raises(hr.RegistryError, match="endpoint"):
        hr.load_registry(str(bad))


def test_cred_0600_enforced(reg):
    _, cred = reg
    assert hr.read_token(str(cred)) == TOKEN
    cred.chmod(0o644)
    with pytest.raises(hr.RegistryError) as e:
        hr.read_token(str(cred))
    assert TOKEN not in str(e.value)
    cred.chmod(0o640)
    with pytest.raises(hr.RegistryError):
        hr.read_token(str(cred))
    cred.write_text(TOKEN + "\n")
    cred.chmod(0o600)
    assert hr.read_token(str(cred)) == TOKEN  # raw form
    assert stat.S_IMODE(os.stat(cred).st_mode) == 0o600


def run(argv, fake, monkeypatch, capsys):
    monkeypatch.setattr(hr, "make_transport", lambda h: fake)
    rc = hr.main(argv)
    return rc, capsys.readouterr()


def test_run_wait_poll_stop_list(reg, monkeypatch, capsys):
    f = Fake([(0, "launched\n"), (0, "done\n"), (0, "status\n"), (0, "stopped\n"), (0, "jobs\n")])
    rc, o = run(["--role", "build", "run", "--label", "j1", "--", "echo", "a b"], f, monkeypatch, capsys)
    assert rc == 0 and o.out == "launched\n"
    assert f.cmds[0] == "python3 /r/hostjob run --label j1 --cwd /r/work -- echo 'a b'"  # opaque-ref-ok: test fixture command string
    assert run(["--host", "a", "wait", "j1", "--timeout", "5"], f, monkeypatch, capsys)[0] == 0
    assert "wait j1 --max-block 5" in f.cmds[1]
    assert run(["--host", "b", "poll", "j1", "--tail", "7"], f, monkeypatch, capsys)[0] == 0
    assert f.cmds[2] == "hostjob poll j1 --tail 7"
    assert run(["--host", "b", "stop", "j1", "--force"], f, monkeypatch, capsys)[0] == 0
    assert f.cmds[3] == "hostjob stop j1 --force"
    assert run(["--host", "b", "list"], f, monkeypatch, capsys)[0] == 0
    assert f.cmds[4] == "hostjob list"


def test_wait_exit_codes(reg):
    hosts = hr.load_registry()
    t = [0.0]

    def clock():
        t[0] += 12
        return t[0]
    a = hr.build_parser().parse_args(["--host", "a", "wait", "j", "--timeout", "30"])
    f = Fake([(75, "still\n")] * 10)
    assert hr.cmd_wait(hosts["a"], f, a, sleep=lambda s: None, clock=clock) == 75
    assert len(f.cmds) >= 2
    f = Fake([(3, "boom\n")])
    assert hr.cmd_wait(hosts["a"], f, a, sleep=lambda s: None) == 3
    f = Fake([(75, ""), (0, "ok\n")])
    assert hr.cmd_wait(hosts["a"], f, a, sleep=lambda s: None, clock=lambda: 0.0) == 0


def test_error_mapping(reg, monkeypatch):
    class Boom(hr.Transport):
        def exec(self, c, timeout=60):
            raise hr.TransportError("ConnectionRefusedError: nope")
    monkeypatch.setattr(hr, "make_transport", lambda h: Boom())
    assert hr.main(["--host", "a", "list"]) == hr.EX_UNAVAILABLE
    assert hr.main(["--role", "laptop", "list"]) == 2
    assert hr.main(["--host", "a", "run", "--label", "x"]) == 2


def test_mcp_exit_code_parse_and_no_token_leak(monkeypatch):
    tr = hr.McpHttpTransport("http://x/mcp", TOKEN)
    seen = []

    def post(body, sid=None, timeout=60):
        seen.append(body)
        if body.get("method") == "tools/call":
            return None, '{"result":{"isError":true,"content":[{"type":"text","text":"out\\n\\n[exit code 75]"}]}}'
        return "sid1", "{}"
    monkeypatch.setattr(tr, "_post", post)
    rc, out = tr.exec("hostjob wait j")
    assert rc == 75 and out == "out\n"
    assert TOKEN not in repr(seen)

    def bad(*a, **k):
        raise OSError("connect failed")
    monkeypatch.setattr(tr, "_post", bad)
    with pytest.raises(hr.TransportError) as e:
        tr.exec("x")
    assert TOKEN not in str(e.value)


def test_hosts_listing_hides_secrets(reg, capsys):
    assert hr.main(["hosts"]) == 0
    out = capsys.readouterr().out
    assert "a\troles=build,synth" in out and "interactive_only" in out
    assert TOKEN not in out


# ------------------------------------------------------------------- sync
import subprocess  # noqa: E402


def _g(cwd, *a):
    return subprocess.run(["git", "-C", str(cwd), *a], check=True, capture_output=True, text=True,
                          env={**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t",
                               "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t"}).stdout.strip()


def _commit(cwd, name, content="x"):
    p = cwd / name
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(content)
    _g(cwd, "add", "-A")
    _g(cwd, "commit", "-qm", name)
    return _g(cwd, "rev-parse", "HEAD")


@pytest.fixture
def repos(tmp_path):
    """origin (work repo) + a clone, both on main."""
    origin, clone = tmp_path / "origin", tmp_path / "clone"
    origin.mkdir()
    _g(origin, "init", "-q", "-b", "main")
    first = _commit(origin, "a")
    subprocess.run(["git", "clone", "-q", str(origin), str(clone)], check=True)
    return origin, clone, first


class Local(hr.Transport):
    """Runs the command in a local bash: exercises the real sync_script."""

    def exec(self, command, timeout=60):
        p = subprocess.run(["bash", "-c", command], capture_output=True, text=True)
        return p.returncode, p.stdout


def _host(repo, **kw):
    return hr.Host("h", {"endpoint": "http://x", "repo": str(repo), **kw})


def _sync(host):
    return hr.sync_hosts({"h": host}, None, lambda h: Local())[0]


def test_derive_repo():
    assert hr.derive_repo("/Users/a/repos/claude-watch/examples/compose/bin/hostjob") == "/Users/a/repos/claude-watch"
    assert hr.derive_repo("hostjob") is None
    assert hr.derive_repo("/r/hostjob") is None


def test_decide_sync_table():
    D = hr.decide_sync
    assert D({"OLD": ["a"], "NEW": ["a"], "BRANCH": ["main"], "TARGET": ["a"]})[0] == "CURRENT"
    s = D({"OLD": ["a"], "NEW": ["b"], "BRANCH": ["main"], "TARGET": ["b"], "CHANGED": ["2"]})
    assert s[0] == "UPDATED" and s[4] is True
    assert D({"OLD": ["a"], "NEW": ["b"], "BRANCH": ["main"], "TARGET": ["b"], "CHANGED": ["0"]})[4] is False
    s = D({"OLD": ["a"], "BRANCH": ["main"], "DIRTY": [" M src/x.rs"]})
    assert s[0] == "REFUSED" and "src/x.rs" in s[1]
    assert D({"OLD": ["a"], "BRANCH": ["feat"]})[0] == "REFUSED"
    assert D({"OLD": ["a"], "BRANCH": ["main"], "FETCHERR": ["boom"]})[0] == "FAILED"
    assert D({"OLD": ["a"], "BRANCH": ["main"], "TARGET": ["b"], "MERGEERR": ["fatal"]})[0] == "FAILED"
    assert D({"ERR": ["no checkout at /x"]})[0] == "FAILED"


def test_sync_real_repo_updates_and_flags_mcp_crate(repos):
    origin, clone, first = repos
    new = _commit(origin, "crates/mcp-host-bash-server/src/main.rs")
    r = _sync(_host(clone))
    assert (r["status"], r["old"], r["new"], r["mcp"]) == ("UPDATED", first, new, True)
    assert "mac-nix switch" in hr.format_sync_table([r])
    assert _g(clone, "rev-parse", "HEAD") == new
    assert _sync(_host(clone))["status"] == "CURRENT"
    new2 = _commit(origin, "docs.md")
    r = _sync(_host(clone))
    assert r["status"] == "UPDATED" and r["new"] == new2 and r["mcp"] is False
    out = hr.format_sync_table([r])
    assert "%s -> %s" % (new[:7], new2[:7]) in out and "NOTE" not in out


def test_sync_refuses_tracked_changes_but_allows_untracked(repos):
    origin, clone, first = repos
    new = _commit(origin, "b")
    (clone / "a").write_text("edited")
    r = _sync(_host(clone))
    assert r["status"] == "REFUSED" and "a" in r["detail"] and _g(clone, "rev-parse", "HEAD") == first
    assert hr.sync_exit_code([r]) == 1
    _g(clone, "checkout", "--", "a")
    (clone / ".env.bak-123").write_text("junk")
    assert _sync(_host(clone))["status"] == "UPDATED"
    assert _g(clone, "rev-parse", "HEAD") == new


def test_sync_refuses_non_main_and_fails_on_diverged(repos):
    origin, clone, first = repos
    _g(clone, "checkout", "-q", "-b", "feat")
    assert _sync(_host(clone))["status"] == "REFUSED"
    _g(clone, "checkout", "-q", "main")
    _commit(clone, "local-only")
    _commit(origin, "remote-only")
    r = _sync(_host(clone))
    assert r["status"] == "FAILED" and "ff-only" in r["detail"]


def test_sync_missing_checkout(tmp_path):
    r = _sync(_host(tmp_path / "nope"))
    assert r["status"] == "FAILED" and "no checkout" in r["detail"]


class Dead(hr.Transport):
    def exec(self, command, timeout=60):
        raise hr.TransportError("URLError: down")


def test_sync_skips_unreachable_and_interactive(reg):
    hosts = hr.load_registry()
    for h in hosts.values():
        h.repo = "/r"
    res = hr.sync_hosts(hosts, None, lambda h: Dead())
    by = {r["host"]: r for r in res}
    assert by["a"]["status"] == by["b"]["status"] == "SKIPPED" and "unreachable" in by["a"]["detail"]
    assert by["lap"]["status"] == "SKIPPED" and "interactive_only" in by["lap"]["detail"]
    assert hr.sync_exit_code(res) == 0  # skips never fail a deploy
    # naming the laptop explicitly does attempt it
    assert "unreachable" in hr.sync_hosts(hosts, "lap", lambda h: Dead())[0]["detail"]
    with pytest.raises(hr.RegistryError):
        hr.sync_hosts(hosts, "nope")


def test_rev_warning_and_check_rev_never_fails(capsys):
    h = hr.Host("g", {"endpoint": "http://x", "repo": "/r/cw"})
    assert hr.rev_warning(h, "a" * 40, "a" * 40) is None
    assert hr.rev_warning(h, "", "b" * 40) is None
    assert "aaaaaaa" in hr.rev_warning(h, "a" * 40, "b" * 40)
    f = Fake([(0, "%s\n%s\trefs/heads/main\n" % ("a" * 40, "b" * 40))])
    hr.check_rev(h, f)
    assert "warning" in capsys.readouterr().err
    assert f.cmds[0].startswith("git -C /r/cw rev-parse HEAD")
    hr.check_rev(h, Dead())  # transport failure is swallowed
    assert capsys.readouterr().err == ""
