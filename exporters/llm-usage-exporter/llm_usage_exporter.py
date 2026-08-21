#!/usr/bin/env python3
"""Prometheus exporter for per-model LLM usage from Claude Code transcripts.

Reads ~/.claude/projects/*/*.jsonl session transcripts on every scrape and
exposes per-model token usage metrics at /metrics on PORT.

This is the LOCAL per-model usage exporter — it captures the same data
devbar's ai-analytics plugin scrapes from transcripts, NOT the aggregate
REMOTE litellm gateway stats (which are scraped by a separate exporter).

Each Claude Code session transcript JSONL contains assistant turns with:
  .message.model = "claude-opus-4-8"
  .message.usage = {
    input_tokens: N,
    output_tokens: M,
    cache_read_input_tokens: K,
    cache_creation_input_tokens: C,
    output_tokens_details: {thinking_tokens: T},
    ...
  }

The exporter tracks cumulative usage per model across all sessions, with
metrics for input, output, cache read, cache write, and thinking tokens.

Metrics:
  - llm_usage_tokens_total{model,token_type}  counter
  - llm_usage_requests_total{model}           counter
  - llm_usage_last_seen_timestamp{model}      gauge (unix seconds)
  - llm_usage_scrape_errors_total             counter
"""

import json
import logging
import os
import time
from collections import defaultdict
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

from prometheus_client import (
    CollectorRegistry,
    Counter,
    Gauge,
    generate_latest,
    CONTENT_TYPE_LATEST,
)

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("llm-usage-exporter")

PORT = int(os.environ.get("PORT", "9104"))
CLAUDE_PROJECTS_DIR = os.environ.get("CLAUDE_PROJECTS_DIR", os.path.expanduser("~/.claude/projects"))

# Known model prefixes for label normalization (collapse unknowns to "other")
KNOWN_MODEL_PREFIXES = {
    "claude-opus",
    "claude-sonnet",
    "claude-haiku",
    "us.anthropic.claude-opus",
    "us.anthropic.claude-sonnet",
    "us.anthropic.claude-haiku",
    "global.anthropic.claude-opus",
    "global.anthropic.claude-sonnet",
    "global.anthropic.claude-haiku",
}

REG = CollectorRegistry()

c_tokens_total = Counter(
    "llm_usage_tokens",
    "Total LLM tokens consumed, by model and token type",
    ["model", "token_type"],
    registry=REG,
)
c_requests_total = Counter(
    "llm_usage_requests",
    "Total LLM requests, by model",
    ["model"],
    registry=REG,
)
g_last_seen = Gauge(
    "llm_usage_last_seen_timestamp",
    "Unix timestamp of most recent usage for this model",
    ["model"],
    registry=REG,
)
c_scrape_errors = Counter(
    "llm_usage_scrape_errors",
    "Number of errors during transcript scraping",
    registry=REG,
)


def _normalize_model(model: str | None) -> str:
    """Normalize model name to a bounded label set."""
    if not model:
        return "other"
    # Check if it starts with any known prefix
    for prefix in KNOWN_MODEL_PREFIXES:
        if model.startswith(prefix):
            return model
    return "other"


# Track which transcript files we've already processed (by path + mtime).
# Structure: {path: (mtime, last_processed_line_count)}
_processed_transcripts: dict[str, tuple[float, int]] = {}


def _scan_transcripts() -> list[tuple[Path, float]]:
    """Find all transcript JSONL files with their mtimes."""
    transcripts = []
    try:
        projects_path = Path(CLAUDE_PROJECTS_DIR)
        if not projects_path.exists():
            log.warning("Projects directory does not exist: %s", CLAUDE_PROJECTS_DIR)
            return transcripts

        for project_dir in projects_path.iterdir():
            if not project_dir.is_dir():
                continue
            for transcript_file in project_dir.glob("*.jsonl"):
                try:
                    mtime = transcript_file.stat().st_mtime
                    transcripts.append((transcript_file, mtime))
                except OSError as e:
                    log.debug("Could not stat %s: %s", transcript_file, e)
    except OSError as e:
        log.error("Failed to scan projects directory %s: %s", CLAUDE_PROJECTS_DIR, e)
        c_scrape_errors.inc()

    return transcripts


def _process_transcript(path: Path, mtime: float):
    """Process a transcript file, incrementing counters for any new usage records."""
    path_str = str(path)

    # Check if we've already processed this file at this mtime
    if path_str in _processed_transcripts:
        old_mtime, old_line_count = _processed_transcripts[path_str]
        if old_mtime == mtime:
            # File hasn't changed since last scrape
            return

    # File is new or has been modified; process it line by line
    try:
        with open(path, "r") as f:
            lines = f.readlines()
    except OSError as e:
        log.debug("Could not read transcript %s: %s", path, e)
        c_scrape_errors.inc()
        return

    # Determine starting line (skip lines we've already processed)
    start_line = 0
    if path_str in _processed_transcripts:
        _, old_line_count = _processed_transcripts[path_str]
        start_line = old_line_count

    new_records = 0
    for i, line in enumerate(lines[start_line:], start=start_line):
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue

        # We want assistant turns with usage data
        message = record.get("message")
        if not message:
            continue
        if message.get("role") != "assistant":
            continue

        usage = message.get("usage")
        if not usage:
            continue

        model = _normalize_model(message.get("model"))

        # Extract token counts
        input_tokens = usage.get("input_tokens", 0)
        output_tokens = usage.get("output_tokens", 0)
        cache_read_tokens = usage.get("cache_read_input_tokens", 0)
        cache_creation_tokens = usage.get("cache_creation_input_tokens", 0)

        # Thinking tokens from output_tokens_details
        output_details = usage.get("output_tokens_details", {})
        thinking_tokens = output_details.get("thinking_tokens", 0)

        # Increment counters
        c_tokens_total.labels(model=model, token_type="input").inc(input_tokens)
        c_tokens_total.labels(model=model, token_type="output").inc(output_tokens)
        c_tokens_total.labels(model=model, token_type="cache_read").inc(cache_read_tokens)
        c_tokens_total.labels(model=model, token_type="cache_creation").inc(cache_creation_tokens)
        c_tokens_total.labels(model=model, token_type="thinking").inc(thinking_tokens)

        c_requests_total.labels(model=model).inc()

        # Update last-seen timestamp
        g_last_seen.labels(model=model).set(time.time())

        new_records += 1

    # Update our tracking state
    _processed_transcripts[path_str] = (mtime, len(lines))

    if new_records > 0:
        log.debug("Processed %d new records from %s", new_records, path.name)


def collect():
    """Scan all transcripts and process new/modified ones. Called on every /metrics scrape."""
    transcripts = _scan_transcripts()
    for path, mtime in transcripts:
        _process_transcript(path, mtime)


class MetricsHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.split("?", 1)[0] != "/metrics":
            self.send_response(404)
            self.end_headers()
            self.wfile.write(b"not found\n")
            return
        collect()
        body = generate_latest(REG)
        self.send_response(200)
        self.send_header("Content-Type", CONTENT_TYPE_LATEST)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        log.debug(fmt, *args)


def main():
    log.info("Starting LLM usage exporter on :%d (reading %s)", PORT, CLAUDE_PROJECTS_DIR)
    # Prime metrics at startup
    collect()
    HTTPServer(("0.0.0.0", PORT), MetricsHandler).serve_forever()


if __name__ == "__main__":
    main()
