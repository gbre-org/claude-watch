#!/usr/bin/env python3
"""Snapshot SHA-256 of every *.js in /usr/share/grafana/public/build/ to stdout as JSON."""
import hashlib, glob, json

BUILD_DIR = "/usr/share/grafana/public/build"

def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()

files = glob.glob(f"{BUILD_DIR}/*.js")
print(json.dumps({f: sha256(f) for f in files}))
