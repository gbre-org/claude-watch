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
        "updated_at": "2026-08-31T23:49:05.819000+00:00",
    },
}
USER_INFO = {
    "user_id": "hallandrew@salesforce.com",
    "user_info": {
        "user_id": "hallandrew@salesforce.com",
        "spend": 3881.399857500011,
        "max_budget": 50000.0,
        "budget_reset_at": "2026-09-01T00:00:00Z",
        "updated_at": "2026-08-31T23:49:05.056000Z",
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

    # upstream `updated_at` staleness gauges, keyed off the gateway's own
    # last-write time -- independent of when WE polled it.
    assert _sample(
        "litellm_key_spend_updated_at_timestamp_seconds",
        {"key_name": "sk-...fYNw", "key_hash": "244f02fcfd53"},
    ) == 1788220145.819
    assert _sample("litellm_user_spend_updated_at_timestamp_seconds", user) == \
        1788220145.056
    print("OK test_poll_maps_all_surfaces")


def test_partial_failure_sets_not_ok():
    def boom(path, params=None):
        if path == "/key/info":
            return KEY_INFO
        raise RuntimeError("simulated 403")
    exp._get = boom
    assert exp.poll() is False
    print("OK test_partial_failure_sets_not_ok")


def test_team_rename_drops_old_label_set():
    """A team_alias rename (same team_id) must not leave a frozen ghost
    series -- regression test for the 2026-08-31 duplicate-bar incident
    (sf-restricted-opus-5-aman-naimat -> sf-restricted-opus-5-anaimat-team).
    """
    exp._get = _fake_get
    assert exp.poll() is True
    old = {
        "team": "sf-internal-vijay-ramesh-team",
        "team_id": "9bb92f86-7403-5e1c-814d-758e7150c282",
    }
    assert _sample("litellm_team_spend_dollars", old) == 20100.2507930499

    renamed_team_info = {
        "team_id": TEAM_INFO["team_id"],
        "team_info": {**TEAM_INFO["team_info"], "team_alias": "renamed-team"},
    }

    def _get_renamed(path, params=None):
        if path == "/team/info":
            return renamed_team_info
        return _fake_get(path, params)

    exp._get = _get_renamed
    assert exp.poll() is True

    # old alias label-set must be GONE, not left frozen alongside the new one.
    assert _sample("litellm_team_spend_dollars", old) is None
    new = {"team": "renamed-team", "team_id": TEAM_INFO["team_id"]}
    assert _sample("litellm_team_spend_dollars", new) == 20100.2507930499
    print("OK test_team_rename_drops_old_label_set")


def test_transient_team_failure_preserves_last_value():
    """A single failed /team/info call must NOT blank the team gauges --
    only a *confirmed* rename (a successful poll with a different alias
    for the same team_id) removes the old label-set. Regression test for
    a blanket-`.clear()`-at-top-of-poll design that was tried and rejected:
    it fixed the ghost-duplicate bug but blanked every team panel on any
    single upstream timeout, which the live gateway hits often.
    """
    exp._get = _fake_get
    assert exp.poll() is True
    team = {
        "team": "sf-internal-vijay-ramesh-team",
        "team_id": "9bb92f86-7403-5e1c-814d-758e7150c282",
    }
    assert _sample("litellm_team_spend_dollars", team) == 20100.2507930499

    def _get_team_info_times_out(path, params=None):
        if path == "/team/info":
            raise RuntimeError("simulated upstream timeout")
        return _fake_get(path, params)

    exp._get = _get_team_info_times_out
    assert exp.poll() is False  # poll reports the failure...

    # ...but the last-known-good team series is still there, untouched.
    assert _sample("litellm_team_spend_dollars", team) == 20100.2507930499
    print("OK test_transient_team_failure_preserves_last_value")


if __name__ == "__main__":
    test_poll_maps_all_surfaces()
    test_partial_failure_sets_not_ok()
    test_team_rename_drops_old_label_set()
    test_transient_team_failure_preserves_last_value()
    print("ALL PASS")
