"""Offline unit tests for the LiteLLM spend exporter.

Feeds recorded gateway JSON fixtures (shapes captured live from the SF
eng-ai-model-gateway on 2026-08-13) through poll() with the network layer
(_get) monkeypatched, then asserts each gauge lands the right value. No
network, no key required — run with:

    python -m pytest test_litellm_spend_exporter.py        # if pytest present
    python test_litellm_spend_exporter.py                  # plain runner
"""
import litellm_spend_exporter as exp

# --- Recorded fixtures (trimmed to fields the exporter reads) ---------------
KEY_INFO = {
    "key": "244f02fcfd53cf5c7524ece4aa3477ad1cc0c0a7aaccfaeec6b47534b1c2cbca",
    "info": {
        "key_name": "sk-...fYNw",
        "spend": 24780.32742304994,
        "user_id": "hallandrew@salesforce.com",
        "team_id": "9bb92f86-7403-5e1c-814d-758e7150c282",
    },
}
USER_INFO = {
    "user_id": "hallandrew@salesforce.com",
    "user_info": {
        "user_id": "hallandrew@salesforce.com",
        "spend": 3881.399857500011,
        "max_budget": 50000.0,
        "budget_reset_at": "2026-09-01T00:00:00Z",
        "teams": ["9bb92f86-7403-5e1c-814d-758e7150c282"],
    },
}
TEAM_INFO = {
    "team_id": "9bb92f86-7403-5e1c-814d-758e7150c282",
    "team_info": {
        "team_alias": "sf-internal-vijay-ramesh-team",
        "spend": 20100.2507930499,
        "max_budget": 50000000.0,
        "budget_reset_at": "2026-09-01T00:00:00Z",
        "members_with_roles": [{"user_id": f"u{i}@x"} for i in range(49)],
    },
}


def _fake_get(path, params=None):
    if path == "/key/info":
        return KEY_INFO
    if path == "/user/info":
        return USER_INFO
    if path == "/team/info":
        return TEAM_INFO
    raise AssertionError(f"unexpected path {path}")


def _sample(name, labels):
    return exp.REG.get_sample_value(name, labels)


def test_poll_maps_all_surfaces(monkeypatch=None):
    exp._get = _fake_get  # monkeypatch the network layer
    ok = exp.poll()
    assert ok is True, "poll should fully succeed on the happy-path fixtures"

    # key lifetime spend + truncated hash from the top-level "key" field.
    assert _sample(
        "litellm_key_spend_dollars",
        {"key_name": "sk-...fYNw", "key_hash": "244f02fcfd53"},
    ) == 24780.32742304994

    # user monthly spend + budget + reset timestamp.
    user = {"user": "hallandrew@salesforce.com"}
    assert _sample("litellm_user_spend_dollars", user) == 3881.399857500011
    assert _sample("litellm_user_max_budget_dollars", user) == 50000.0
    assert _sample("litellm_user_budget_reset_timestamp_seconds", user) > 0

    # team aggregate spend + budget + member count (auto-discovered team).
    team = {
        "team": "sf-internal-vijay-ramesh-team",
        "team_id": "9bb92f86-7403-5e1c-814d-758e7150c282",
    }
    assert _sample("litellm_team_spend_dollars", team) == 20100.2507930499
    assert _sample("litellm_team_max_budget_dollars", team) == 50000000.0
    assert _sample("litellm_team_members", team) == 49
    print("OK test_poll_maps_all_surfaces")


def test_partial_failure_sets_not_ok():
    def boom(path, params=None):
        if path == "/key/info":
            return KEY_INFO
        raise RuntimeError("simulated 403")
    exp._get = boom
    assert exp.poll() is False
    print("OK test_partial_failure_sets_not_ok")


if __name__ == "__main__":
    test_poll_maps_all_surfaces()
    test_partial_failure_sets_not_ok()
    print("ALL PASS")
