#!/usr/bin/env python3
"""Markdown link checker — fail on broken relative links.

Walks markdown files in the repo (default: the baked managed-policy doc plus
the docs/ tree and the READMEs it cross-links into) and verifies that every
*relative* link target resolves to an existing path on disk.

Why this exists: container/baked-CLAUDE.md is baked into the container image at
/etc/claude-code/CLAUDE.md and now references sibling docs by plain RELATIVE
links (docs/..., README.md, container/README.md, ...) instead of absolute
raw-GitHub URLs, because the docs are now COPYed into the image alongside it.
A relative link that points at a path which does not exist in the repo would
silently 404 in-container, so this check guards against that class of rot. It
also runs repo-wide (--all) so the cross-doc links in docs/*.md stay honest.

Rules:
  * External links (http://, https://, mailto:, etc.) are NOT checked.
  * Pure in-page anchors (#section) are skipped.
  * A trailing #anchor on a file target is stripped before the existence check
    (we verify the file/dir exists; we do not validate the heading slug).
  * Link targets are resolved relative to the directory of the file the link
    appears in (standard markdown semantics).
  * Both files and directories are accepted as valid targets (some links point
    at a directory, e.g. `container` or `examples/compose/bin`).

Exit code 0 = all relative links resolve; 1 = at least one broken link.

Usage:
  scripts/check-doc-links.py                 # check the default doc set
  scripts/check-doc-links.py path/to/file.md # check a specific file (or files)
  scripts/check-doc-links.py --all           # check every *.md in the repo
  scripts/check-doc-links.py --self-test     # run the embedded unit tests
"""

from __future__ import annotations

import os
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

# Inline markdown link: [text](target).  We deliberately do NOT match
# reference-style links or autolinks; this repo's docs use the inline form.
# The target stops at the first whitespace (markdown allows a `(url "title")`
# form) or closing paren.
LINK_RE = re.compile(r"\[[^\]]*\]\(\s*(<[^>]+>|[^)\s]+)")

# Anything with a URL scheme (https:, mailto:, etc.) is not a local path.
EXTERNAL_RE = re.compile(r"^[a-zA-Z][a-zA-Z0-9+.-]*:")

# Default doc set: the baked managed-policy CLAUDE.md plus the docs/ tree and
# the top-level + nested READMEs that baked-CLAUDE.md cross-links into.
DEFAULT_DOCS = [
    "container/baked-CLAUDE.md",
    "README.md",
    "CLAUDE.md",
    "container/README.md",
    "examples/compose/README.md",
]

# Some docs are baked into the container image at a layout root that differs
# from where they live in the repo, so their relative links are written
# against the BAKED layout, not their own repo directory. For those files we
# resolve relative links against an explicit base dir (a repo-relative path)
# instead of the file's own parent.
#
#   container/baked-CLAUDE.md is COPYed to /etc/claude-code/CLAUDE.md, and the
#   docs it links to (docs/, README.md, container/README.md,
#   examples/compose/...) are COPYed to /etc/claude-code/<repo-relative-path>.
#   So /etc/claude-code/ mirrors the REPO ROOT, and its `docs/watchers.md` etc.
#   links must be checked against the repo root — not against `container/`,
#   where the source file happens to sit.
LINK_BASE_OVERRIDES = {
    "container/baked-CLAUDE.md": ".",  # resolve relative links against repo root
}


def iter_doc_links(text: str):
    """Yield raw link targets from markdown text."""
    for m in LINK_RE.finditer(text):
        target = m.group(1)
        if target.startswith("<") and target.endswith(">"):
            target = target[1:-1]
        yield target


def is_external(target: str) -> bool:
    return bool(EXTERNAL_RE.match(target))


def normalize_target(target: str) -> str | None:
    """Return the file/dir portion of a link target, or None if it is not a
    local path that needs an on-disk existence check."""
    if not target:
        return None
    if is_external(target):
        return None
    # Pure in-page anchor.
    if target.startswith("#"):
        return None
    # Strip a trailing #anchor (we only verify the path, not the slug) and any
    # trailing ?query.
    path_part = target.split("#", 1)[0].split("?", 1)[0]
    if not path_part:
        return None
    return path_part


def check_file(md_path: Path, repo_root: Path) -> list[str]:
    """Return a list of broken-link error strings for one markdown file."""
    errors: list[str] = []
    try:
        text = md_path.read_text(encoding="utf-8")
    except OSError as exc:  # pragma: no cover - unreadable file
        return [f"{md_path}: cannot read ({exc})"]
    # Resolve relative links against the file's own dir, unless this file is
    # baked at a different layout root (see LINK_BASE_OVERRIDES).
    try:
        rel_key = md_path.relative_to(repo_root).as_posix()
    except ValueError:
        rel_key = None
    if rel_key in LINK_BASE_OVERRIDES:
        base_dir = (repo_root / LINK_BASE_OVERRIDES[rel_key]).resolve()
    else:
        base_dir = md_path.parent
    for target in iter_doc_links(text):
        path_part = normalize_target(target)
        if path_part is None:
            continue
        if path_part.startswith("/"):
            resolved = (repo_root / path_part.lstrip("/")).resolve()
        else:
            resolved = (base_dir / path_part).resolve()
        if not resolved.exists():
            rel = md_path.relative_to(repo_root)
            errors.append(f"{rel}: broken link -> {target}")
    return errors


def collect_docs(args: list[str], repo_root: Path) -> list[Path]:
    if "--all" in args:
        docs: list[Path] = []
        for dirpath, dirnames, filenames in os.walk(repo_root):
            dirnames[:] = [
                d
                for d in dirnames
                if d not in {".git", "target", "node_modules", ".venv", "venv"}
            ]
            for fn in filenames:
                if fn.endswith(".md"):
                    docs.append(Path(dirpath) / fn)
        return sorted(docs)
    explicit = [a for a in args if not a.startswith("-")]
    if explicit:
        return [Path(a).resolve() for a in explicit]
    return [repo_root / d for d in DEFAULT_DOCS]


def main(argv: list[str]) -> int:
    if "--self-test" in argv:
        return _self_test()
    docs = collect_docs(argv, REPO_ROOT)
    all_errors: list[str] = []
    checked = 0
    for md in docs:
        if not md.exists():
            continue
        checked += 1
        all_errors.extend(check_file(md, REPO_ROOT))
    if all_errors:
        print("Broken relative markdown links found:\n", file=sys.stderr)
        for err in all_errors:
            print(f"  {err}", file=sys.stderr)
        print(
            f"\n{len(all_errors)} broken link(s) across {checked} file(s).",
            file=sys.stderr,
        )
        return 1
    print(f"OK: all relative markdown links resolve ({checked} file(s) checked).")
    return 0


# --------------------------------------------------------------------------- #
# Embedded self-tests (run with --self-test). Kept dependency-free.
# --------------------------------------------------------------------------- #
def _self_test() -> int:
    import tempfile

    failures = 0

    def expect(cond: bool, msg: str):
        nonlocal failures
        if not cond:
            failures += 1
            print(f"FAIL: {msg}", file=sys.stderr)

    links = list(iter_doc_links("see [a](docs/x.md) and [b](https://e.com) [c](#h)"))
    expect(links == ["docs/x.md", "https://e.com", "#h"], f"link extraction: {links}")

    expect(is_external("https://example.com"), "https is external")
    expect(is_external("mailto:a@b.c"), "mailto is external")
    expect(not is_external("docs/x.md"), "relative path is not external")
    expect(not is_external("#anchor"), "anchor is not external (handled separately)")

    expect(normalize_target("https://e.com") is None, "external -> None")
    expect(normalize_target("#h") is None, "anchor -> None")
    expect(normalize_target("docs/x.md#h") == "docs/x.md", "strip anchor")
    expect(normalize_target("docs/x.md") == "docs/x.md", "plain path")
    expect(normalize_target("a.md?x=1") == "a.md", "strip query")

    with tempfile.TemporaryDirectory() as td:
        root = Path(td)
        (root / "docs").mkdir()
        (root / "docs" / "real.md").write_text("# real")
        (root / "subdir").mkdir()
        good = root / "main.md"
        good.write_text(
            "[ok](docs/real.md)\n[dir](subdir)\n[ext](https://e.com)\n"
            "[anchor-only](#x)\n[anchored](docs/real.md#heading)\n"
        )
        errs = check_file(good, root)
        expect(errs == [], f"all-good file should have no errors: {errs}")

        bad = root / "bad.md"
        bad.write_text("[missing](docs/nope.md)\n[ok](docs/real.md)\n")
        errs = check_file(bad, root)
        expect(len(errs) == 1, f"exactly one broken link expected: {errs}")
        expect("docs/nope.md" in errs[0], f"broken link names target: {errs}")

        # LINK_BASE_OVERRIDES: a file baked at a different layout root resolves
        # its relative links against the override base (repo root here), not
        # its own parent dir.
        (root / "sub").mkdir(exist_ok=True)
        baked = root / "sub" / "baked.md"
        # Link target docs/real.md must resolve against ROOT (where docs/ is),
        # NOT against sub/ (which has no docs/). Without the override this
        # would be flagged broken.
        baked.write_text("[doc](docs/real.md)\n")
        LINK_BASE_OVERRIDES["sub/baked.md"] = "."
        try:
            errs = check_file(baked, root)
            expect(errs == [], f"override should resolve against root: {errs}")
            # Sanity: removing the override makes it break (resolved vs sub/).
            del LINK_BASE_OVERRIDES["sub/baked.md"]
            errs = check_file(baked, root)
            expect(len(errs) == 1, f"without override the link is broken: {errs}")
        finally:
            LINK_BASE_OVERRIDES.pop("sub/baked.md", None)

    if failures:
        print(f"\n{failures} self-test failure(s).", file=sys.stderr)
        return 1
    print("All self-tests passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
