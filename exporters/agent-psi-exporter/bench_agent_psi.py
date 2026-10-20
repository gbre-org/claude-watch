#!/usr/bin/env python3
"""Scrape-cost benchmark for agent-psi-exporter on a synthetic corpus.

Builds a projects dir of ``--files`` transcripts totalling ``--mib`` MiB (a
few large main loops plus sub-agents), then runs ``--scrapes`` scrapes, and
before each one appends ``--lines`` lines to ``--active`` of the files (the
live agents). Each mode runs in its own process so peak RSS is per mode.

Modes:
  full        collect_live_transcripts with no cache: every live file is
              re-parsed from scratch on every scrape (the pre-cache behaviour)
  wholefile   bench-only alternative: JSON entries cached per file, keyed by
              (dev, inode, size, mtime_ns); a changed file is re-read whole,
              and intervals are re-derived from the cached entries each scrape
  incremental agent_psi.TranscriptCache (what the exporter uses)
  exporter    the exporter's whole /metrics path (collect + generate_latest),
              with the result cache disabled; AGENT_PSI_INCREMENTAL decides
              the parse mode, and --exporter-dir can point at another
              checkout's exporter to time it on the same corpus

Run:
  uv run --python 3.11 --with prometheus_client \\
      python3 exporters/agent-psi-exporter/bench_agent_psi.py
"""

import argparse
import json
import os
import random
import resource
import statistics
import subprocess
import sys
import tempfile
import time
from importlib.util import module_from_spec, spec_from_file_location

HERE = os.path.dirname(os.path.abspath(__file__))
SESSIONS = ("aaaa0000", "bbbb1111", "cccc2222")


def _iso(epoch):
    from datetime import datetime, timezone

    return (datetime.fromtimestamp(epoch, tz=timezone.utc).isoformat()
            .replace("+00:00", "Z"))


def _lines(rng, t, n, tag, pad):
    """n transcript lines from time t: tool round-trips with padded results."""
    out = []
    for k in range(n):
        tid = f"{tag}-{k}"
        r = rng.random()
        if r < 0.45:
            out.append({"type": "assistant", "timestamp": _iso(t), "message": {
                "model": "claude-opus-5", "stop_reason": "tool_use",
                "usage": {"output_tokens": rng.randint(20, 2000)},
                "content": [{"type": "text", "text": "x" * rng.randint(50, pad)},
                            {"type": "tool_use", "id": tid, "name": "Bash"}]}})
        elif r < 0.9:
            out.append({"type": "user", "timestamp": _iso(t),
                        "toolUseResult": {"stdout": "ok"}, "message": {
                            "role": "user", "content": [{
                                "type": "tool_result", "tool_use_id": tid,
                                "content": "y" * rng.randint(100, pad * 4)}]}})
        else:
            out.append({"type": "system", "timestamp": _iso(t), "content": "."})
        t += rng.choice((0.5, 2, 5, 15))
    return out, t


def _write(path, entries, mode="w"):
    with open(path, mode) as fh:
        for e in entries:
            fh.write(json.dumps(e) + "\n")


def build_corpus(root, mib, files, seed=7):
    """-> list of [path, next_ts]; main loops get ~half the bytes."""
    rng = random.Random(seed)
    now = time.time()
    slug = os.path.join(root, "-home-bench")
    paths = []
    n_main = min(len(SESSIONS), files)
    for i in range(files):
        sess = SESSIONS[i % n_main]
        if i < n_main:
            paths.append(os.path.join(slug, f"{sess}-0000.jsonl"))
        else:
            sub = os.path.join(slug, f"{sess}-0000", "subagents")
            os.makedirs(sub, exist_ok=True)
            paths.append(os.path.join(sub, f"agent-{i:04d}.jsonl"))
    os.makedirs(slug, exist_ok=True)
    budget = mib * 1024 * 1024
    shares = [0.5 / n_main if i < n_main else 0.5 / max(1, files - n_main)
              for i in range(files)]
    state = []
    for path, share in zip(paths, shares):
        t = now - 6 * 3600
        target = budget * share
        with open(path, "w"):
            pass
        while os.path.getsize(path) < target:
            batch, t = _lines(rng, t, 200, os.path.basename(path), 1500)
            _write(path, batch, "a")
        # Keep the timeline inside the trailing hour so windows have content.
        state.append([path, min(t, now - 60)])
    return state


def _wholefile_collect(cache, projects, now):
    import agent_psi as ap

    out, seen = [], set()
    for path, session_id, is_main in ap.discover_transcripts(projects):
        st = os.stat(path)
        if now - st.st_mtime > ap.DEFAULT_LIVE_WINDOW_SECONDS:
            continue
        seen.add(path)
        key = (st.st_dev, st.st_ino, st.st_size, st.st_mtime_ns)
        hit = cache.get(path)
        if hit is None or hit[0] != key:
            entries = []
            with open(path) as fh:
                for line in fh:
                    line = line.strip()
                    if line:
                        try:
                            entries.append(json.loads(line))
                        except ValueError:
                            pass
            cache[path] = hit = (key, entries)
        entries = hit[1]
        running = ap.is_running_transcript(entries)
        ivs = ap.parse_intervals(entries, now=now,
                                 terminated=not is_main and not running)
        out.append((ivs, running, ap.extract_model(entries),
                    ap.extract_api_errors(entries)))
    for p in [p for p in cache if p not in seen]:
        del cache[p]
    return out


def run_mode(args):
    sys.path.insert(0, args.exporter_dir or HERE)
    import agent_psi as ap

    corpus = json.load(open(os.path.join(args.root, "corpus.json")))
    projects = os.path.join(args.root, "projects")
    rng = random.Random(11)
    if args.mode == "exporter":
        os.environ.update(CLAUDE_PROJECTS_DIR=projects, PORT="0",
                          AGENT_PSI_CACHE_TTL_SECONDS="0")
        spec = spec_from_file_location(
            "exp", os.path.join(args.exporter_dir or HERE,
                                "agent_psi_exporter.py"))
        mod = module_from_spec(spec)
        spec.loader.exec_module(mod)
        from prometheus_client import generate_latest

        def scrape():
            mod.collect()
            generate_latest(mod.REG)
    elif args.mode == "incremental":
        cache = ap.TranscriptCache()

        def scrape():
            ap.collect_live_transcripts(projects, time.time(), cache=cache)
    elif args.mode == "wholefile":
        cache = {}

        def scrape():
            _wholefile_collect(cache, projects, time.time())
    else:
        def scrape():
            ap.collect_live_transcripts(projects, time.time())

    times = []
    for i in range(args.scrapes + 1):
        for item in rng.sample(corpus, min(args.active, len(corpus))):
            batch, item[1] = _lines(rng, item[1], args.lines, f"s{i}", 1500)
            _write(item[0], batch, "a")
        t0 = time.perf_counter()
        scrape()
        times.append(time.perf_counter() - t0)
    first, rest = times[0], times[1:]
    rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024
    print(json.dumps({
        "mode": args.mode, "label": args.label or args.mode,
        "first_ms": round(first * 1000, 1),
        "mean_ms": round(statistics.mean(rest) * 1000, 1),
        "p50_ms": round(statistics.median(rest) * 1000, 1),
        "max_ms": round(max(rest) * 1000, 1),
        "peak_rss_mib": round(rss, 1),
    }))


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--mib", type=float, default=40)
    ap.add_argument("--files", type=int, default=23)
    ap.add_argument("--scrapes", type=int, default=20)
    ap.add_argument("--active", type=int, default=6,
                    help="files appended to before each scrape")
    ap.add_argument("--lines", type=int, default=4,
                    help="lines appended per active file per scrape")
    ap.add_argument("--modes", default="full,wholefile,incremental,exporter")
    ap.add_argument("--exporter-dir", help="exporter dir for mode=exporter")
    ap.add_argument("--label")
    ap.add_argument("--mode", help=argparse.SUPPRESS)
    ap.add_argument("--root", help=argparse.SUPPRESS)
    args = ap.parse_args()
    if args.mode:
        return run_mode(args)

    root = tempfile.mkdtemp(prefix="agent-psi-bench-")
    corpus = build_corpus(os.path.join(root, "projects"), args.mib, args.files)
    with open(os.path.join(root, "corpus.json"), "w") as fh:
        json.dump(corpus, fh)
    size = sum(os.path.getsize(p) for p, _ in corpus) / 2**20
    print(f"# corpus: {len(corpus)} files, {size:.1f} MiB, {args.scrapes} "
          f"scrapes, {args.active} files x {args.lines} lines appended per "
          f"scrape", flush=True)
    base = [sys.executable, os.path.abspath(__file__), "--root", root,
            "--scrapes", str(args.scrapes), "--active", str(args.active),
            "--lines", str(args.lines)]
    if args.exporter_dir:
        base += ["--exporter-dir", args.exporter_dir]
    if args.label:
        base += ["--label", args.label]
    for mode in args.modes.split(","):
        # Each mode gets a pristine copy so appends from one don't bias another.
        work = tempfile.mkdtemp(prefix="agent-psi-bench-run-")
        subprocess.run(["cp", "-a", os.path.join(root, "projects"), work],
                       check=True)
        moved = [[p.replace(root, work, 1), t] for p, t in corpus]
        with open(os.path.join(work, "corpus.json"), "w") as fh:
            json.dump(moved, fh)
        cmd = list(base)
        cmd[cmd.index("--root") + 1] = work
        subprocess.run(cmd + ["--mode", mode], check=True)
        subprocess.run(["rm", "-rf", work], check=True)
    subprocess.run(["rm", "-rf", root], check=True)


if __name__ == "__main__":
    main()
