#!/usr/bin/env python3
"""Prometheus exporter for LiteLLM (SF eng-ai-model-gateway) token spend.

Polls the LiteLLM gateway spend endpoints and exposes Prometheus metrics for
the operator's own monthly spend, their team-scoped key's lifetime spend, and
their team's aggregate spend + budget.

Endpoints (authenticated with the gateway key, Authorization: Bearer <key>):
  GET /key/info                    -> info.spend  (LIFETIME key spend), key_name,
                                        user_id, team_id
  GET /user/info?user_id=<email>   -> user_info.spend (MONTHLY), max_budget,
                                        budget_reset_at, teams[]
  GET /team/info?team_id=<id>      -> team_info.spend (team aggregate MTD),
                                        max_budget, budget_reset_at,
                                        members_with_roles[]

Auth / role note: an ``internal_user_viewer`` key can read ITS OWN
/user/info, /key/info, and /team/info (aggregate) for teams it belongs to,
but CANNOT read another user's /user/info (403) nor the admin /global/spend
routes. So a per-member spend breakdown is NOT available at this role — the
team AGGREGATE is. This exporter therefore surfaces: my spend, my key's
lifetime spend, and the team aggregate.

Timescale: ``/user/info`` spend is the CURRENT calendar-month (monthly budget,
resets on budget_reset_at); ``/key/info`` spend is LIFETIME. Don't cross-check
one against the other.

Config via env:
  PORT                  listen port (default 9104)
  LITELLM_BASE_URL      gateway base URL (required),
                        e.g. https://eng-ai-model-gateway.sfproxy...aws.sfdc.cl
  LITELLM_API_KEY       gateway sk-... key. If unset, tried in order:
  LITELLM_API_KEY_CMD   shell command whose stdout is the key
                        (e.g. "devbar auth claude")
  LITELLM_API_KEY_FILE  path to a file containing the key
  LITELLM_USER_ID       user email to query (default: taken from /key/info)
  LITELLM_TEAM_IDS      comma-separated team ids (default: auto-discovered
                        from /user/info user_info.teams)
  SCRAPE_TTL_SECONDS    min seconds between upstream polls; results are cached
                        so multiple Prometheus scrapes don't exceed the
                        gateway rpm limit (default 60)
  HTTP_TIMEOUT_SECONDS  per-request timeout (default 20)
"""
import json
import logging
import os
import subprocess
import threading
import time
import urllib.parse
import urllib.request
from datetime import datetime
from http.server import BaseHTTPRequestHandler, HTTPServer

from prometheus_client import (
    CONTENT_TYPE_LATEST,
    CollectorRegistry,
    Gauge,
    generate_latest,
)

logging.basicConfig(
    level=os.environ.get("LOG_LEVEL", "INFO"),
    format="%(asctime)s %(levelname)s litellm-spend-exporter %(message)s",
)
log = logging.getLogger("litellm-spend-exporter")

PORT = int(os.environ.get("PORT", "9104"))
BASE_URL = os.environ.get("LITELLM_BASE_URL", "").rstrip("/")
USER_ID = os.environ.get("LITELLM_USER_ID", "").strip()
TEAM_IDS_ENV = os.environ.get("LITELLM_TEAM_IDS", "").strip()
SCRAPE_TTL = float(os.environ.get("SCRAPE_TTL_SECONDS", "60"))
HTTP_TIMEOUT = float(os.environ.get("HTTP_TIMEOUT_SECONDS", "20"))

REG = CollectorRegistry()

g_user_spend = Gauge(
    "litellm_user_spend_dollars",
    "Current calendar-month spend for the user, in USD (from /user/info).",
    ["user"], registry=REG,
)
g_user_budget = Gauge(
    "litellm_user_max_budget_dollars",
    "Monthly max budget for the user, in USD.",
    ["user"], registry=REG,
)
g_user_reset = Gauge(
    "litellm_user_budget_reset_timestamp_seconds",
    "Unix timestamp when the user's monthly budget resets.",
    ["user"], registry=REG,
)
g_key_spend = Gauge(
    "litellm_key_spend_dollars",
    "Lifetime spend for the gateway key, in USD (from /key/info).",
    ["key_name", "key_hash"], registry=REG,
)
g_team_spend = Gauge(
    "litellm_team_spend_dollars",
    "Aggregate team spend, in USD (from /team/info team_info.spend).",
    ["team", "team_id"], registry=REG,
)
g_team_budget = Gauge(
    "litellm_team_max_budget_dollars",
    "Team max budget, in USD.",
    ["team", "team_id"], registry=REG,
)
g_team_reset = Gauge(
    "litellm_team_budget_reset_timestamp_seconds",
    "Unix timestamp when the team budget resets.",
    ["team", "team_id"], registry=REG,
)
g_team_members = Gauge(
    "litellm_team_members",
    "Number of members in the team.",
    ["team", "team_id"], registry=REG,
)
g_scrape_success = Gauge(
    "litellm_spend_scrape_success",
    "1 if the last upstream poll fully succeeded, 0 otherwise.",
    registry=REG,
)
g_scrape_duration = Gauge(
    "litellm_spend_scrape_duration_seconds",
    "Duration of the last upstream poll, in seconds.",
    registry=REG,
)
g_last_scrape = Gauge(
    "litellm_spend_last_scrape_timestamp_seconds",
    "Unix timestamp of the last upstream poll.",
    registry=REG,
)

_lock = threading.Lock()
_last_poll = 0.0
_api_key = None


def resolve_api_key():
    """Resolve the gateway key from env / cmd / file (once, memoized)."""
    global _api_key
    if _api_key:
        return _api_key
    key = os.environ.get("LITELLM_API_KEY", "").strip()
    if not key:
        cmd = os.environ.get("LITELLM_API_KEY_CMD", "").strip()
        if cmd:
            try:
                key = subprocess.check_output(
                    cmd, shell=True, text=True, timeout=HTTP_TIMEOUT
                ).strip()
            except Exception as e:  # noqa: BLE001
                log.error("LITELLM_API_KEY_CMD failed: %s", e)
    if not key:
        path = os.environ.get("LITELLM_API_KEY_FILE", "").strip()
        if path and os.path.exists(path):
            with open(path, "r", encoding="utf-8") as fh:
                key = fh.read().strip()
    _api_key = key
    return key


def _get(path, params=None):
    url = f"{BASE_URL}{path}"
    if params:
        url = f"{url}?{urllib.parse.urlencode(params)}"
    req = urllib.request.Request(url)
    key = resolve_api_key()
    if key:
        req.add_header("Authorization", f"Bearer {key}")
        req.add_header("x-api-key", key)
    with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT) as resp:
        return json.loads(resp.read().decode("utf-8"))


def _iso_to_epoch(s):
    if not s:
        return None
    try:
        return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()
    except Exception:  # noqa: BLE001
        return None


def poll():
    """Poll the gateway and update the gauges. Returns True if fully ok."""
    ok = True

    # 1. /key/info -> key lifetime spend, user_id fallback.
    user_id = USER_ID
    try:
        kresp = _get("/key/info")
        ki = kresp.get("info", {}) or {}
        # The key hash is the top-level "key" field; "info" carries key_name,
        # spend, user_id (no raw token). Expose a truncated hash only.
        g_key_spend.labels(
            key_name=str(ki.get("key_name", "")),
            key_hash=str(kresp.get("key", ""))[:12],
        ).set(float(ki.get("spend") or 0.0))
        if not user_id:
            user_id = ki.get("user_id") or ""
    except Exception as e:  # noqa: BLE001
        log.error("/key/info failed: %s", e)
        ok = False

    # 2. /user/info -> monthly user spend + budget + team discovery.
    discovered_teams = []
    if user_id:
        try:
            ui = _get("/user/info", {"user_id": user_id}).get("user_info", {}) or {}
            g_user_spend.labels(user=user_id).set(float(ui.get("spend") or 0.0))
            if ui.get("max_budget") is not None:
                g_user_budget.labels(user=user_id).set(float(ui["max_budget"]))
            reset = _iso_to_epoch(ui.get("budget_reset_at"))
            if reset:
                g_user_reset.labels(user=user_id).set(reset)
            discovered_teams = list(ui.get("teams") or [])
        except Exception as e:  # noqa: BLE001
            log.error("/user/info failed for %s: %s", user_id, e)
            ok = False
    else:
        log.error("no user_id available (set LITELLM_USER_ID)")
        ok = False

    # 3. /team/info per team -> team aggregate spend + budget + member count.
    if TEAM_IDS_ENV:
        team_ids = [t.strip() for t in TEAM_IDS_ENV.split(",") if t.strip()]
    else:
        team_ids = discovered_teams
    for tid in team_ids:
        try:
            ti = _get("/team/info", {"team_id": tid}).get("team_info", {}) or {}
            alias = ti.get("team_alias") or tid
            g_team_spend.labels(team=alias, team_id=tid).set(
                float(ti.get("spend") or 0.0)
            )
            if ti.get("max_budget") is not None:
                g_team_budget.labels(team=alias, team_id=tid).set(
                    float(ti["max_budget"])
                )
            reset = _iso_to_epoch(ti.get("budget_reset_at"))
            if reset:
                g_team_reset.labels(team=alias, team_id=tid).set(reset)
            members = ti.get("members_with_roles") or ti.get("members") or []
            g_team_members.labels(team=alias, team_id=tid).set(len(members))
        except Exception as e:  # noqa: BLE001
            log.error("/team/info failed for %s: %s", tid, e)
            ok = False

    return ok


def maybe_poll():
    """Poll at most once per SCRAPE_TTL; cached values serve other scrapes."""
    global _last_poll
    with _lock:
        now = time.time()
        if now - _last_poll < SCRAPE_TTL and _last_poll > 0:
            return
        start = time.time()
        try:
            ok = poll()
        except Exception as e:  # noqa: BLE001
            log.error("poll raised: %s", e)
            ok = False
        g_scrape_success.set(1 if ok else 0)
        g_scrape_duration.set(time.time() - start)
        g_last_scrape.set(time.time())
        _last_poll = now


class MetricsHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.split("?", 1)[0] != "/metrics":
            self.send_response(404)
            self.end_headers()
            self.wfile.write(b"not found\n")
            return
        maybe_poll()
        body = generate_latest(REG)
        self.send_response(200)
        self.send_header("Content-Type", CONTENT_TYPE_LATEST)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        log.debug(fmt, *args)


def main():
    if not BASE_URL:
        raise SystemExit("LITELLM_BASE_URL is required")
    if not resolve_api_key():
        log.warning(
            "no API key resolved (LITELLM_API_KEY / _CMD / _FILE); "
            "scrapes will fail until one is provided"
        )
    log.info("Starting litellm-spend exporter on :%d (base=%s, ttl=%ss)",
             PORT, BASE_URL, SCRAPE_TTL)
    maybe_poll()
    HTTPServer(("0.0.0.0", PORT), MetricsHandler).serve_forever()


if __name__ == "__main__":
    main()
