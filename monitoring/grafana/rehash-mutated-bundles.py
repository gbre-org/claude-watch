#!/usr/bin/env python3
"""
rehash-mutated-bundles.py
=========================
Runs AFTER all sed mutations in the Grafana Dockerfile.

Steps:
1. Load the pre-sed SHA-256 manifest from /tmp/pre-sed-hashes.json.
2. Re-hash every *.js in BUILD_DIR. Any file whose hash changed is "mutated".
3. For each mutated file: compute new short-hash (first 20 hex chars of SHA-256),
   rename <prefix>.<old20hex>.js -> <prefix>.<new20hex>.js
4. Walk all *.js, *.html, *.json files under PUBLIC_DIR and replace every
   occurrence of the old basename with the new basename (byte-level replace).
5. ALSO patch bare 20-char hash references.  The webpack runtime chunk stores a
   chunk-id→hash map like {2537:"e4502d402c380cee6851",...} and constructs file
   URLs as prefix + "." + hash + ".js" at load time.  Step 4 replaces full-
   filename strings; step 5 replaces the quoted bare hash values so the runtime
   chunk map stays consistent.  We replace only the exact quoted form
   "<old20hex>" → "<new20hex>" to avoid false positives on hash substrings that
   appear coincidentally in other data.
6. Sanity check: confirm no leftover references to any old basename remain.
   Exits non-zero on failure so the Docker build fails loudly.

Idempotent: if no files were mutated, no renames happen and the script exits 0.
"""

import hashlib
import glob
import json
import os
import re
import sys

BUILD_DIR = "/usr/share/grafana/public/build"
PUBLIC_DIR = "/usr/share/grafana/public"
PRE_MANIFEST = "/tmp/pre-sed-hashes.json"

# Filename pattern: <prefix>.<20hex>.js
# Examples:
#   2537.e4502d402c380cee6851.js
#   2537-react19.57de9d194ac4dc5ff65c.js
#   runtime.6fea232581a172580535.js
HASH_RE = re.compile(r'^(.*?)\.([0-9a-f]{20})(\.js)$')


def file_sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def collect_targets():
    """All *.js, *.html, *.json files under PUBLIC_DIR to patch references in."""
    targets = []
    for pattern in (
        f"{BUILD_DIR}/*.js",
        f"{PUBLIC_DIR}/views/*.html",
        f"{BUILD_DIR}/*.json",
    ):
        targets.extend(glob.glob(pattern))
    return targets


def main():
    # Load pre-sed manifest
    if not os.path.exists(PRE_MANIFEST):
        print(f"ERROR: pre-sed manifest not found at {PRE_MANIFEST}", file=sys.stderr)
        sys.exit(1)

    with open(PRE_MANIFEST) as fh:
        pre_hashes = json.load(fh)  # path -> full sha256hex

    # Compute post-sed hashes and find mutated files
    js_files = glob.glob(f"{BUILD_DIR}/*.js")
    # renames: old_basename -> new_basename
    # hash_renames: old_20hex -> new_20hex  (for runtime chunk-map patching)
    renames = {}
    hash_renames = {}

    for fpath in js_files:
        post_hash = file_sha256(fpath)
        pre_hash = pre_hashes.get(fpath)

        if pre_hash is None:
            # File wasn't present before (new file added during build, not mutated)
            continue

        if pre_hash == post_hash:
            continue  # unchanged

        # File was mutated — parse its filename
        basename = os.path.basename(fpath)
        m = HASH_RE.match(basename)
        if not m:
            print(f"WARN: mutated file has unparseable name: {basename} — skipping rehash",
                  file=sys.stderr)
            continue

        prefix, old_short, ext = m.groups()
        new_short = post_hash[:20]

        if old_short == new_short:
            # Astronomically unlikely but handle gracefully
            print(f"INFO: hash collision on {basename}, skipping rename", file=sys.stderr)
            continue

        new_basename = f"{prefix}.{new_short}{ext}"
        new_path = os.path.join(BUILD_DIR, new_basename)

        print(f"rehash: {basename} -> {new_basename}")
        os.rename(fpath, new_path)
        renames[basename] = new_basename
        hash_renames[old_short] = new_short

    if not renames:
        print("rehash: no bundles mutated, nothing to rename")
        sys.exit(0)

    # Walk all target files and patch references.
    # Two passes per file:
    #   (a) full-filename replace: "2537.e4502d402c380cee6851.js" -> "2537.757b0e408e149dc6f8a4.js"
    #   (b) bare-hash replace in quoted context: "e4502d402c380cee6851" -> "757b0e408e149dc6f8a4"
    #       (webpack runtime chunk map stores {chunk_id: "hash"}, not full filenames)
    targets = collect_targets()
    for tgt in targets:
        try:
            with open(tgt, "rb") as fh:
                data = fh.read()
        except OSError as e:
            print(f"WARN: cannot read {tgt}: {e}", file=sys.stderr)
            continue

        orig = data
        changed = False

        # (a) full filename replace
        for old, new in renames.items():
            old_b = old.encode()
            new_b = new.encode()
            if old_b in data:
                data = data.replace(old_b, new_b)
                changed = True
                print(f"  patched filename in {os.path.basename(tgt)}: {old} -> {new}")

        # (b) bare quoted-hash replace for runtime chunk map
        # Replace only the exact quoted form to avoid false positives.
        for old_h, new_h in hash_renames.items():
            # Match "old_hash" (double-quoted) or 'old_hash' (single-quoted)
            old_dq = f'"{old_h}"'.encode()
            new_dq = f'"{new_h}"'.encode()
            old_sq = f"'{old_h}'".encode()
            new_sq = f"'{new_h}'".encode()
            if old_dq in data:
                data = data.replace(old_dq, new_dq)
                changed = True
                print(f"  patched hash-map in {os.path.basename(tgt)}: \"{old_h}\" -> \"{new_h}\"")
            if old_sq in data:
                data = data.replace(old_sq, new_sq)
                changed = True
                print(f"  patched hash-map in {os.path.basename(tgt)}: '{old_h}' -> '{new_h}'")

        if changed:
            with open(tgt, "wb") as fh:
                fh.write(data)

    # Sanity check: no leftover references to old basenames
    targets_after = collect_targets()
    failures = []
    for old in renames:
        old_b = old.encode()
        for tgt in targets_after:
            try:
                if old_b in open(tgt, "rb").read():
                    failures.append((old, tgt))
            except OSError:
                pass

    if failures:
        for old, tgt in failures:
            print(f"FAIL: old filename '{old}' still referenced in {tgt}", file=sys.stderr)
        sys.exit(1)

    print(f"rehash: complete — renamed {len(renames)} bundle(s), all references updated")


if __name__ == "__main__":
    main()
