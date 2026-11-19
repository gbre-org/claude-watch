"""The unacked_agent_completion row must seed an evaluator cmd that resolves
on a host install (evaluator on PATH via ~/bin), not a hardcoded container
path that does not exist there."""
import importlib.machinery
import importlib.util
from pathlib import Path

INIT = Path(__file__).resolve().parents[1] / "obligations-init"
NAME = "eval-unacked-agent-completion"


def _load():
    loader = importlib.machinery.SourceFileLoader("obligations_init_t", str(INIT))
    spec = importlib.util.spec_from_loader("obligations_init_t", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


def test_host_layout_uses_bare_name(monkeypatch):
    mod = _load()
    monkeypatch.delenv("CW_EVAL_BIN_DIR", raising=False)
    monkeypatch.setattr(mod.os.path, "exists", lambda p: False)
    assert mod._eval_cmd(NAME) == NAME


def test_container_layout_keeps_baked_path(monkeypatch):
    mod = _load()
    monkeypatch.delenv("CW_EVAL_BIN_DIR", raising=False)
    monkeypatch.setattr(mod.os.path, "exists", lambda p: True)
    assert mod._eval_cmd(NAME) == "/usr/local/bin/" + NAME


def test_env_override_wins(monkeypatch):
    mod = _load()
    monkeypatch.setenv("CW_EVAL_BIN_DIR", "/opt/x")
    monkeypatch.setattr(mod.os.path, "exists", lambda p: False)
    assert mod._eval_cmd(NAME) == "/opt/x/" + NAME


def test_seed_row_cmd_is_resolvable_on_host(monkeypatch):
    """The cmd handed to the evaluator predicate is a bare name that
    shutil.which finds when the evaluator dir is on PATH."""
    mod = _load()
    monkeypatch.delenv("CW_EVAL_BIN_DIR", raising=False)
    monkeypatch.setattr(mod.os.path, "exists", lambda p: False)
    params = mod._main_loop_only_evaluator_params(mod._eval_cmd(NAME))
    flat = repr(params)
    assert f"'cmd': '{NAME}'" in flat
    assert "/usr/local/bin" not in flat
