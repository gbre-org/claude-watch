"""Dependency-free shell-structure parser for obligation gate matching.

Why this module exists
----------------------
The obligation gates (the hardcoded watcher-ctl-bare guard, the
no-pipe-into-signal-send guard, and the generic ``no_pipe_pattern``
predicate) historically matched a regex/substring against the *raw* Bash
command string. That string includes the bodies of quoted arguments and
heredocs -- i.e. *data* that never executes as a command. So a DM whose
message text merely *mentioned* ``| signal-send`` (a payload to ``cat`` /
``signal-stage``), or a queue description that *described* a forbidden
pattern, false-positive-DENIED even though nothing forbidden would run.

This module parses a Bash command string into a small structural model --
just enough to answer three questions the gates actually care about:

  * What are the top-level command segments (split on REAL pipes /
    ``&&`` / ``||`` / ``;`` / ``&`` / newlines, ignoring those operators
    when they appear inside quotes or heredoc bodies)?
  * For each segment, what is the command *head* (argv[0]) -- so we can
    ask "is ``signal-send`` the head of a pipe-RHS segment?" or "is
    ``watcher-ctl run`` an actual command node?".
  * Are there any REAL top-level compound / background / pipe operators
    (so the watcher-ctl-bare guard can refuse a non-bare invocation)?

Deliberately NOT a full bash grammar. It is a tokenizer + a top-level
operator splitter that is quote/heredoc/escape aware. It does not expand
variables, does not descend into ``$(...)`` command substitution bodies
to find nested pipelines, and does not model redirections beyond skipping
their target tokens. The design contract is:

  * Eliminate FALSE POSITIVES (forbidden text inside quoted/heredoc data
    must NOT match) without introducing FALSE NEGATIVES (a real forbidden
    invocation must still match).
  * On ANY parse failure / ambiguity, raise ``ShellParseError`` so the
    caller can FAIL CLOSED back to the old string-match behavior.

Everything here is pure (no I/O, no shelling out) and stdlib-only.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from typing import List, Optional


class ShellParseError(Exception):
    """Raised when the command cannot be parsed structurally.

    Callers MUST treat this as "fall back to the previous string-match
    behavior" -- never as "allow". An unparseable command is the one case
    where we keep the blunt regex so a real violation hidden behind a
    malformed construct still trips the gate.
    """


# Operators we recognize at the top level, longest-match first so ``&&``
# beats ``&`` and ``||`` beats ``|``.
_TWO_CHAR_OPS = ("&&", "||", ";;")
_ONE_CHAR_OPS = ("|", "&", ";")

# Characters that begin a redirection (we skip the operator + its target
# token so e.g. ``2>&1`` or ``> file`` doesn't get mistaken for a command
# head or a stray ``&``).
_REDIR_CHARS = ("<", ">")


@dataclass
class Segment:
    """One top-level command segment (one pipeline stage / one statement).

    ``words`` is the list of shell words after quote-stripping. ``head``
    is ``words[0]`` if present else "". ``op_before`` is the operator
    that preceded this segment in the command (``""`` for the first
    segment); ``op_after`` is the operator that follows it (``""`` for the
    last). Operators are one of ``"|" "&&" "||" ";" "&" "\\n"``.
    """

    words: List[str] = field(default_factory=list)
    op_before: str = ""
    op_after: str = ""

    @property
    def head(self) -> str:
        return self.words[0] if self.words else ""

    def head_phrase(self, n: int) -> str:
        """Return the first ``n`` words joined by single spaces.

        Used to match multi-word command heads like ``watcher-ctl run``.
        """
        return " ".join(self.words[:n])


@dataclass
class ParsedCommand:
    segments: List[Segment]

    def heads(self) -> List[str]:
        return [s.head for s in self.segments if s.head]

    def has_top_level_operator(self) -> bool:
        """True if any REAL top-level pipe/compound/background operator
        (or newline statement separator) exists in the command."""
        return any(s.op_after for s in self.segments if s.op_after)

    def pipe_rhs_segments(self) -> List[Segment]:
        """Segments that sit on the RHS of a real pipe node.

        A segment is a pipe-RHS if the operator immediately before it is
        ``|``. This is exactly the set of commands "piped into".
        """
        return [s for s in self.segments if s.op_before == "|"]


# ---------------------------------------------------------------------------
# Tokenizer
# ---------------------------------------------------------------------------
#
# We walk the string once, tracking quote state, escapes, and heredoc
# bodies. We emit a flat token stream of two kinds:
#   ("word", <text>)   -- a shell word (with quotes resolved/stripped)
#   ("op",   <text>)   -- a top-level operator (| & && || ; \n)
# Redirection operators and their target tokens are consumed but not
# emitted (they are neither command heads nor splitting operators).
#
# Heredocs: when we see ``<<`` (optionally ``<<-``) we read the delimiter
# word, then everything up to a line whose content equals the delimiter is
# the heredoc BODY and is skipped wholesale -- it is data, never command
# structure. This is the key fix for the "forbidden pattern quoted inside
# a heredoc body" false positive.


def _is_op_char(c: str) -> bool:
    return c in ("|", "&", ";")


def tokenize(cmd: str) -> List[tuple]:
    """Return a flat ``[(kind, text), ...]`` token stream.

    ``kind`` is ``"word"`` or ``"op"``. Raises ``ShellParseError`` on
    unbalanced quotes or an unterminated heredoc-delimiter read.
    """
    tokens: List[tuple] = []
    i = 0
    n = len(cmd)
    # Pending heredoc delimiters queued on the current logical line. Bash
    # allows several (``cmd <<A <<B``); bodies are consumed in order at the
    # next newline. We store (delimiter, strip_tabs).
    pending_heredocs: List[tuple] = []

    cur = []  # chars of the in-progress word
    word_started = False  # did we open a word (even an empty quoted one)?

    def flush_word():
        nonlocal cur, word_started
        if word_started:
            tokens.append(("word", "".join(cur)))
        cur = []
        word_started = False

    def consume_heredoc_bodies(j: int) -> int:
        """At a newline (index j points AT the '\\n'), consume queued
        heredoc bodies. Return the new index (just past consumed bodies)."""
        nonlocal pending_heredocs
        j += 1  # step past the newline that ends the opener line
        for delim, strip_tabs in pending_heredocs:
            while True:
                # Read one body line [j, eol)
                eol = cmd.find("\n", j)
                if eol == -1:
                    line = cmd[j:]
                    next_j = n
                else:
                    line = cmd[j:eol]
                    next_j = eol + 1
                check = line.lstrip("\t") if strip_tabs else line
                if check == delim:
                    j = next_j
                    break
                if eol == -1:
                    # Unterminated heredoc: treat the rest as body and stop.
                    j = n
                    break
                j = next_j
        pending_heredocs = []
        return j

    while i < n:
        c = cmd[i]

        # Backslash escape (outside any quoting): the next char is literal.
        if c == "\\":
            if i + 1 < n:
                nxt = cmd[i + 1]
                if nxt == "\n":
                    # line continuation -- drop both chars, stays in word
                    i += 2
                    continue
                cur.append(nxt)
                word_started = True
                i += 2
                continue
            # trailing backslash -- literal
            cur.append("\\")
            word_started = True
            i += 1
            continue

        # Single quotes: everything literal until the next single quote.
        if c == "'":
            word_started = True
            end = cmd.find("'", i + 1)
            if end == -1:
                raise ShellParseError("unterminated single quote")
            cur.append(cmd[i + 1:end])
            i = end + 1
            continue

        # Double quotes: literal except backslash-escapes; no operator or
        # heredoc handling inside. We strip the quotes, keep the content.
        if c == '"':
            word_started = True
            j = i + 1
            buf = []
            closed = False
            while j < n:
                cj = cmd[j]
                if cj == "\\" and j + 1 < n:
                    # In double quotes bash only escapes a few chars; for
                    # our purposes keeping the escaped char literal is safe
                    # (we are stripping, not re-executing).
                    buf.append(cmd[j + 1])
                    j += 2
                    continue
                if cj == '"':
                    closed = True
                    j += 1
                    break
                buf.append(cj)
                j += 1
            if not closed:
                raise ShellParseError("unterminated double quote")
            cur.append("".join(buf))
            i = j
            continue

        # Heredoc opener: ``<<`` or ``<<-`` followed by a delimiter word.
        if c == "<" and i + 1 < n and cmd[i + 1] == "<":
            flush_word()
            k = i + 2
            strip_tabs = False
            if k < n and cmd[k] == "-":
                strip_tabs = True
                k += 1
            # ``<<<`` is a here-STRING, not a here-doc. Treat the third
            # ``<`` as part of a redirection we skip; no body queued.
            if k < n and cmd[k] == "<":
                # here-string: skip the operator, the following word is data
                k += 1
                # skip spaces
                while k < n and cmd[k] in (" ", "\t"):
                    k += 1
                # consume one word (quote-aware-lite): stop at whitespace
                # or top-level operator. Good enough -- it is data.
                k = _skip_data_word(cmd, k)
                i = k
                continue
            # skip spaces between << and delimiter
            while k < n and cmd[k] in (" ", "\t"):
                k += 1
            # read the delimiter word (may be quoted)
            delim, k = _read_heredoc_delim(cmd, k)
            if delim is None:
                raise ShellParseError("heredoc with no delimiter")
            pending_heredocs.append((delim, strip_tabs))
            i = k
            continue

        # Other redirections: ``>`` ``>>`` ``<`` ``2>`` ``&>`` ``>&`` etc.
        # We skip the operator and its target token so the target file
        # isn't read as a command head and ``2>&1`` isn't read as ``&``.
        if c in _REDIR_CHARS:
            flush_word()
            i = _skip_redirection(cmd, i)
            continue

        # A leading fd-number redirection like ``2>file`` / ``2>&1``: if
        # the current word is all digits and the next char starts a
        # redirection, treat the digits as an fd and skip the redirection.
        if c in (">", "<"):
            # handled above; kept for clarity
            pass

        # Newline: statement separator AND heredoc-body trigger.
        if c == "\n":
            flush_word()
            if pending_heredocs:
                i = consume_heredoc_bodies(i)
            else:
                i += 1
            tokens.append(("op", "\n"))
            continue

        # Whitespace: word boundary.
        if c in (" ", "\t"):
            flush_word()
            i += 1
            continue

        # Operators: |, ||, &, &&, ;
        if _is_op_char(c):
            flush_word()
            two = cmd[i:i + 2]
            if two in _TWO_CHAR_OPS:
                # ``&&`` ``||`` -- but ``2>&1`` etc. already handled via
                # redirection skipping above, so a bare ``&`` here is real.
                tokens.append(("op", two))
                i += 2
                continue
            # Single-char operator. Special case ``|&`` (bash pipe+stderr).
            if c == "|" and i + 1 < n and cmd[i + 1] == "&":
                tokens.append(("op", "|"))
                i += 2
                continue
            tokens.append(("op", c))
            i += 1
            continue

        # Subshell / grouping parens and braces: we do NOT descend. Treat
        # an opening ``(`` / ``)`` as a structural boundary that makes the
        # command "non-simple". We emit a sentinel operator so callers that
        # require a BARE command (watcher-ctl) see structure, but pipeline
        # RHS detection stays conservative. Represent as ";" boundary.
        if c in ("(", ")", "{", "}"):
            flush_word()
            # Only treat brace as structural when it stands alone (a word
            # boundary), not when it's part of e.g. ``${VAR}`` or a literal.
            if c in ("(", ")"):
                tokens.append(("op", ";"))  # generic structural boundary
                i += 1
                continue
            # ``{`` / ``}`` are only special as standalone tokens; otherwise
            # part of a word (brace expansion, ${...}). Keep them in-word.
            cur.append(c)
            word_started = True
            i += 1
            continue

        # Command substitution / arithmetic: ``$(`` ... ``)`` and backticks.
        # We do not parse inside; we consume the whole construct as part of
        # the current word so its contents never leak as operators/heads.
        if c == "$" and i + 1 < n and cmd[i + 1] == "(":
            end = _match_paren(cmd, i + 1)
            if end == -1:
                raise ShellParseError("unbalanced $( )")
            cur.append(cmd[i:end + 1])
            word_started = True
            i = end + 1
            continue
        if c == "`":
            end = cmd.find("`", i + 1)
            if end == -1:
                raise ShellParseError("unbalanced backticks")
            cur.append(cmd[i:end + 1])
            word_started = True
            i = end + 1
            continue

        # Ordinary character.
        cur.append(c)
        word_started = True
        i += 1

    flush_word()
    return tokens


def _skip_data_word(cmd: str, k: int) -> int:
    """Skip a single (possibly quoted) data word starting at k. Returns
    the index just past it. Used for here-string operands."""
    n = len(cmd)
    while k < n and cmd[k] not in (" ", "\t", "\n", "|", "&", ";"):
        if cmd[k] == "'":
            end = cmd.find("'", k + 1)
            if end == -1:
                raise ShellParseError("unterminated single quote in data word")
            k = end + 1
            continue
        if cmd[k] == '"':
            j = k + 1
            while j < n and cmd[j] != '"':
                if cmd[j] == "\\":
                    j += 2
                    continue
                j += 1
            if j >= n:
                raise ShellParseError("unterminated double quote in data word")
            k = j + 1
            continue
        k += 1
    return k


def _read_heredoc_delim(cmd: str, k: int):
    """Read a heredoc delimiter word starting at k. Quotes around the
    delimiter are stripped (``<<'EOF'`` and ``<<EOF`` use the same delim
    ``EOF``). Returns ``(delim, new_index)`` or ``(None, k)``."""
    n = len(cmd)
    if k >= n:
        return None, k
    buf = []
    while k < n and cmd[k] not in (" ", "\t", "\n", "|", "&", ";", "<", ">"):
        if cmd[k] in ("'", '"'):
            q = cmd[k]
            end = cmd.find(q, k + 1)
            if end == -1:
                raise ShellParseError("unterminated quote in heredoc delimiter")
            buf.append(cmd[k + 1:end])
            k = end + 1
            continue
        if cmd[k] == "\\":
            if k + 1 < n:
                buf.append(cmd[k + 1])
                k += 2
                continue
            k += 1
            continue
        buf.append(cmd[k])
        k += 1
    if not buf:
        return None, k
    return "".join(buf), k


def _skip_redirection(cmd: str, i: int) -> int:
    """Skip a redirection operator + its target token at index i (which
    points at ``<`` or ``>``). Returns index just past the target."""
    n = len(cmd)
    # consume the operator chars: > >> < &> >& and an optional leading fd
    # already handled by the caller stripping the word; here we just eat
    # > / < / & / digits that form the operator.
    while i < n and cmd[i] in (">", "<", "&"):
        i += 1
    # ``>&1`` / ``>&2`` -- the fd target may directly follow with no space
    while i < n and cmd[i] in (" ", "\t"):
        i += 1
    # consume the target token (a filename or fd) up to whitespace/operator
    while i < n and cmd[i] not in (" ", "\t", "\n", "|", "&", ";", "<", ">"):
        if cmd[i] == "'":
            end = cmd.find("'", i + 1)
            if end == -1:
                raise ShellParseError("unterminated quote in redirection target")
            i = end + 1
            continue
        if cmd[i] == '"':
            j = i + 1
            while j < n and cmd[j] != '"':
                if cmd[j] == "\\":
                    j += 2
                    continue
                j += 1
            if j >= n:
                raise ShellParseError("unterminated quote in redirection target")
            i = j + 1
            continue
        i += 1
    return i


def _match_paren(cmd: str, open_idx: int) -> int:
    """Given index of an opening ``(``, return the index of the matching
    ``)`` accounting for nesting and quotes. -1 if unbalanced."""
    n = len(cmd)
    depth = 0
    i = open_idx
    while i < n:
        c = cmd[i]
        if c == "'":
            end = cmd.find("'", i + 1)
            if end == -1:
                return -1
            i = end + 1
            continue
        if c == '"':
            j = i + 1
            while j < n and cmd[j] != '"':
                if cmd[j] == "\\":
                    j += 2
                    continue
                j += 1
            if j >= n:
                return -1
            i = j + 1
            continue
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return -1


# ---------------------------------------------------------------------------
# Top-level structural parse
# ---------------------------------------------------------------------------


def parse(cmd: str) -> ParsedCommand:
    """Parse ``cmd`` into top-level segments. Raises ``ShellParseError`` on
    any construct we can't structurally resolve (caller falls back to
    string-match)."""
    if cmd is None:
        raise ShellParseError("command is None")
    tokens = tokenize(cmd)

    segments: List[Segment] = []
    cur_words: List[str] = []
    op_before = ""

    def close(op_after: str):
        nonlocal cur_words, op_before
        seg = Segment(words=cur_words, op_before=op_before, op_after=op_after)
        segments.append(seg)
        cur_words = []
        op_before = op_after

    for kind, text in tokens:
        if kind == "word":
            cur_words.append(text)
        else:  # op
            # Newlines that are pure separators between blank statements
            # shouldn't create spurious empty segments unless they carry
            # structure. We DO record them so has_top_level_operator is
            # accurate, but collapse runs of separators around empty
            # segments.
            if not cur_words and not segments and text == "\n":
                # leading blank line -- ignore
                continue
            close(text)

    # final segment (no trailing operator)
    seg = Segment(words=cur_words, op_before=op_before, op_after="")
    # Avoid a trailing empty segment created by a terminal operator with no
    # following command (e.g. ``cmd ;``) unless it's the only segment.
    if seg.words or not segments:
        segments.append(seg)

    return ParsedCommand(segments=segments)


# ---------------------------------------------------------------------------
# High-level query helpers used by the gates
# ---------------------------------------------------------------------------


def command_is_piped_into(cmd: str, target: str) -> bool:
    """True iff ``target`` is the command head of a segment on the RHS of a
    REAL pipe. Quoted/heredoc occurrences of ``target`` do not count.

    Raises ``ShellParseError`` on parse failure (caller falls back).
    """
    parsed = parse(cmd)
    for seg in parsed.pipe_rhs_segments():
        if _head_matches(seg, target):
            return True
    return False


def command_present_as_head(cmd: str, target: str) -> bool:
    """True iff ``target`` is the command head of ANY top-level segment
    (not just pipe-RHS). Quoted/heredoc occurrences don't count."""
    parsed = parse(cmd)
    return any(_head_matches(seg, target) for seg in parsed.segments)


def command_names(cmd: str) -> set:
    """Set of effective command-head BASENAMES across all top-level segments.

    For each top-level segment we strip leading ``VAR=val`` env-assignments
    and wrapper words (``sudo`` / ``env`` / ``nohup`` / ``exec`` / ...) via
    ``_strip_command_prefix``, then take the BASENAME of the resulting head
    word. The basename step is what lets an absolute-path invocation like
    ``/usr/local/bin/watcher-ctl run x`` match the plain name ``watcher-ctl``.

    Occurrences of a name inside quoted arguments or heredoc bodies never
    appear here (they were absorbed into a single data word during
    tokenization, so they are never a segment HEAD).

    Raises ``ShellParseError`` on parse failure (caller FAILS CLOSED to the
    previous string-match behavior).
    """
    parsed = parse(cmd)
    out = set()
    for s in parsed.segments:
        words = _strip_command_prefix(s.words)
        if words:
            out.add(os.path.basename(words[0]))
    return out


def command_name_present(cmd: str, targets) -> bool:
    """True iff any effective command-head basename (see ``command_names``)
    matches one of ``targets``.

    ``targets`` is any iterable of command-name strings; empty / falsy
    entries are ignored. Raises ``ShellParseError`` on parse failure.
    """
    tset = {t for t in (targets or []) if t}
    return bool(command_names(cmd) & tset)


def _head_matches(seg: Segment, target: str) -> bool:
    """Does this segment's command head equal ``target``?

    ``target`` may be a multi-word phrase (e.g. ``watcher-ctl run``); we
    compare against the segment's leading words. Leading environment
    assignments (``FOO=bar cmd``) and a leading ``sudo`` / ``command`` /
    ``env`` wrapper are skipped so ``sudo watcher-ctl run`` still matches
    ``watcher-ctl run``.
    """
    words = _strip_command_prefix(seg.words)
    parts = target.split()
    if len(words) < len(parts):
        return False
    return words[:len(parts)] == parts


_PREFIX_WRAPPERS = ("sudo", "command", "env", "nohup", "exec", "time",
                    "builtin", "stdbuf", "nice", "ionice", "setsid",
                    "doas", "xargs")


def _strip_command_prefix(words: List[str]) -> List[str]:
    """Drop leading ``VAR=value`` assignments and common command wrappers
    so the *effective* command head is exposed."""
    out = list(words)
    changed = True
    while out and changed:
        changed = False
        w = out[0]
        # env assignment: NAME=VALUE where NAME is an identifier
        if _is_env_assignment(w):
            out = out[1:]
            changed = True
            continue
        if w in _PREFIX_WRAPPERS:
            # skip the wrapper word; for env, also skip following NAME=VAL
            out = out[1:]
            changed = True
            continue
    return out


def _is_env_assignment(word: str) -> bool:
    eq = word.find("=")
    if eq <= 0:
        return False
    name = word[:eq]
    if not (name[0].isalpha() or name[0] == "_"):
        return False
    return all(ch.isalnum() or ch == "_" for ch in name)


def has_real_compound_operator(cmd: str) -> bool:
    """True iff the command has a REAL top-level pipe / && / || / ; / & /
    newline operator (outside quotes & heredocs). Raises on parse failure."""
    return parse(cmd).has_top_level_operator()


def backgrounded_segment_heads(cmd: str) -> List[str]:
    """Return the effective command heads of every segment that is
    BACKGROUNDED by a REAL top-level ``&`` operator.

    A segment is backgrounded iff its ``op_after`` is the literal ``&``
    background operator (not ``&&``, which the tokenizer emits as the
    distinct ``"&&"`` op, and not a ``&`` that appears inside quotes /
    heredocs / a redirection like ``2>&1``, all of which the tokenizer
    already excludes from being top-level ops). The subshell form
    ``(cmd &)`` is also caught: the tokenizer maps ``(`` / ``)`` to ``;``
    boundaries, so the inner ``&`` is a real top-level op on the segment
    holding ``cmd``.

    Each returned head is run through the same prefix-stripping as
    ``command_present_as_head`` (leading ``VAR=val`` env assignments and
    wrapper words like ``sudo`` / ``env`` / ``nohup`` are removed) so the
    EFFECTIVE launcher is exposed -- e.g. ``nohup watcher-ctl run x &``
    yields head ``watcher-ctl``.

    Returns the first word of each backgrounded segment's stripped words
    (``""`` for an empty segment, which is filtered out). Raises
    ``ShellParseError`` on parse failure (caller FAILS CLOSED).
    """
    parsed = parse(cmd)
    out: List[str] = []
    for seg in parsed.segments:
        if seg.op_after != "&":
            continue
        words = _strip_command_prefix(seg.words)
        if words:
            out.append(words[0])
    return out


def has_backgrounded_head(cmd: str, targets) -> bool:
    """True iff ANY segment backgrounded by a real top-level ``&`` has a
    command head (after prefix-stripping) matching one of ``targets``.

    ``targets`` is an iterable of head specs. Each spec may be:
      * a plain command name (``"claude-event-watch"``) -- matched against
        the segment's first stripped word, OR
      * a multi-word phrase (``"watcher-ctl run"``) -- matched against the
        segment's leading stripped words, OR
      * a path basename match: a spec containing ``/`` (e.g.
        ``"/opt/claude-container/watchers/"``) matches when the head's
        directory prefix equals the spec (so any
        ``/opt/claude-container/watchers/<x>.sh`` launcher matches the
        spec ``"/opt/claude-container/watchers/"``).

    Quoted / heredoc occurrences never count (they were absorbed into a
    single data word and carry no ``&`` op). Raises ``ShellParseError`` on
    parse failure (caller FAILS CLOSED).
    """
    parsed = parse(cmd)
    specs = list(targets or [])
    for seg in parsed.segments:
        if seg.op_after != "&":
            continue
        if _segment_head_matches_any(seg, specs):
            return True
    return False


def _segment_head_matches_any(seg: "Segment", specs: List[str]) -> bool:
    """Does this segment's effective head match any spec in ``specs``?

    Reuses the same prefix-stripping as ``_head_matches`` and supports
    plain names, multi-word phrases, and directory-prefix path specs
    (a spec ending in ``/`` or containing ``/`` matches by path prefix
    on the head token)."""
    words = _strip_command_prefix(seg.words)
    if not words:
        return False
    head = words[0]
    for spec in specs:
        if not isinstance(spec, str) or not spec:
            continue
        # Path-prefix spec: a spec containing a slash matches when the
        # head is a path under that prefix (or equals it). Catches
        # ``/opt/claude-container/watchers/<name>.sh``.
        if "/" in spec:
            if head == spec or head.startswith(spec):
                return True
            continue
        # Multi-word phrase (e.g. ``watcher-ctl run``): compare against
        # the leading stripped words.
        parts = spec.split()
        if len(parts) > 1:
            if len(words) >= len(parts) and words[:len(parts)] == parts:
                return True
            continue
        # Plain command name.
        if head == spec:
            return True
    return False


def structure_string(cmd: str) -> str:
    """Reconstruct a "structure-only" rendering of ``cmd``.

    Each top-level segment's words are re-joined with single spaces and the
    REAL inter-segment operators are re-inserted, but the *contents* of
    quoted arguments and heredoc bodies are flattened to their stripped
    literal form (and any operators/pipes that appeared INSIDE that data
    are gone, because they were absorbed into a single word).

    The point: an arbitrary ``no_pipe_pattern`` regex (e.g.
    ``\\|\\s*signal-send``) can be applied against this string instead of
    the raw command, and it will match ONLY when the pipe is a real
    top-level pipe -- not when ``| signal-send`` sits inside a heredoc body
    or quoted argument. This preserves the configured regex's intent
    without having to parse the regex itself.

    Raises ``ShellParseError`` on parse failure (caller falls back to the
    raw string).
    """
    parsed = parse(cmd)
    out: List[str] = []
    for seg in parsed.segments:
        if seg.op_before:
            # render the operator with surrounding spaces so a regex like
            # ``\|\s*signal-send`` still matches ``| signal-send``.
            op = "\n" if seg.op_before == "\n" else f" {seg.op_before} "
            out.append(op)
        # Words are atomic DATA in the structural rendering. Neutralize any
        # shell-operator-lookalike characters that survived quote-stripping
        # (``|`` ``&`` ``;`` ``<`` ``>`` plus backtick / ``$(``) so a
        # no_pipe_pattern regex applied to this string can only match a REAL
        # top-level operator, never an operator character that was quoted
        # DATA. We replace each with a space rather than deleting it so word
        # boundaries are preserved.
        out.append(" ".join(_neutralize_operator_chars(w) for w in seg.words))
    return "".join(out)


_OP_LOOKALIKE = str.maketrans({c: " " for c in "|&;<>`"})


def _neutralize_operator_chars(word: str) -> str:
    return word.translate(_OP_LOOKALIKE)


# ---------------------------------------------------------------------------
# Embedded test suite (`python3 shell_ast.py --test`)
# ---------------------------------------------------------------------------


def _run_tests() -> int:
    cases = []

    def ok(name, cond, detail=""):
        cases.append((name, bool(cond), detail))

    # --- command_is_piped_into ---
    ok("real pipe into signal-send -> True",
       command_is_piped_into("cat x | signal-send --dm a hi", "signal-send"))
    ok("no pipe, signal-send as standalone head -> False",
       not command_is_piped_into("signal-send --dm a hi", "signal-send"))
    # signal-send mentioned only inside a single-quoted argument
    ok("signal-send in single-quoted arg -> False",
       not command_is_piped_into(
           "echo 'do not pipe | signal-send'", "signal-send"))
    # signal-send mentioned inside a double-quoted argument
    ok("signal-send in double-quoted arg -> False",
       not command_is_piped_into(
           'echo "use | signal-send wrong"', "signal-send"))
    # heredoc body mentions | signal-send; real send is a separate statement
    heredoc = (
        'f=$(signal-stage); cat > "$f" <<\'EOF\'\n'
        'reminder: never pipe | signal-send -- use signal-stage + -F\n'
        'EOF\n'
        'signal-send --dm andrew -F "$f"'
    )
    ok("| signal-send in heredoc body -> not piped",
       not command_is_piped_into(heredoc, "signal-send"))
    ok("heredoc: real send IS present as a head",
       command_present_as_head(heredoc, "signal-send"))
    # real pipe through tee then into signal-send
    ok("multi-stage pipe RHS into signal-send -> True",
       command_is_piped_into("cat x | tee y | signal-send hi", "signal-send"))

    # --- watcher-ctl run as head ---
    ok("bare watcher-ctl run -> head present, no operator",
       command_present_as_head("watcher-ctl run signal", "watcher-ctl run")
       and not has_real_compound_operator("watcher-ctl run signal"))
    ok("watcher-ctl run with 2>&1 -> no operator",
       not has_real_compound_operator("watcher-ctl run signal 2>&1"))
    ok("watcher-ctl run && echo -> has operator",
       has_real_compound_operator("watcher-ctl run foo " + "&&" + " echo hi"))
    ok("watcher-ctl run text inside quoted arg -> not a head",
       not command_present_as_head(
           "session-task queue add 'watcher-ctl run foo "
           + "&&" + " bar must be bare'", "watcher-ctl run"))
    ok("watcher-ctl run text inside quoted arg -> no real operator",
       not has_real_compound_operator(
           "session-task queue add 'watcher-ctl run foo "
           + "&&" + " bar must be bare'"))
    ok("sudo watcher-ctl run -> head still matches",
       command_present_as_head("sudo watcher-ctl run signal", "watcher-ctl run"))
    ok("subshell (watcher-ctl run X &) -> has operator",
       has_real_compound_operator("(watcher-ctl run X " + "&" + ")"))

    # --- structure_string ---
    ss = structure_string("cat x | signal-send hi")
    ok("structure_string keeps real pipe", "| signal-send" in ss)
    ss2 = structure_string("echo 'a | signal-send b'")
    ok("structure_string drops quoted pipe",
       "| signal-send" not in ss2)

    # --- redirections don't create spurious operators ---
    ok("2>&1 not treated as background &",
       not has_real_compound_operator("foo bar 2>&1"))
    ok("> file not an operator",
       not has_real_compound_operator("foo > out.txt"))

    # --- backgrounded_segment_heads / has_backgrounded_head ---
    WATCHERS = ["/opt/claude-container/watchers/", "watcher-ctl run",
                "claude-event-watch"]
    # Real trailing & on a watcher launcher -> MUST match.
    ok("claude-event-watch & -> backgrounded head",
       has_backgrounded_head("claude-event-watch &", WATCHERS))
    ok("watcher-ctl run X & -> backgrounded head",
       has_backgrounded_head("watcher-ctl run signal &", WATCHERS))
    ok("nohup watcher-ctl run X & -> backgrounded head (prefix stripped)",
       has_backgrounded_head("nohup watcher-ctl run signal &", WATCHERS))
    ok("/opt watcher path & -> backgrounded head (path prefix)",
       has_backgrounded_head(
           "/opt/claude-container/watchers/foo.sh &", WATCHERS))
    ok("subshell (claude-event-watch &) -> backgrounded head",
       has_backgrounded_head("(claude-event-watch &)", WATCHERS))
    # & followed by another command (cmd & echo done) still backgrounds cmd.
    ok("watcher & echo done -> backgrounded head",
       has_backgrounded_head("claude-event-watch & echo done", WATCHERS))
    # FALSE-POSITIVE guards: a & that is NOT a real background op.
    ok("claude-event-watch with no & -> NOT backgrounded",
       not has_backgrounded_head("claude-event-watch", WATCHERS))
    ok("run_in_background mention quoted -> NOT backgrounded",
       not has_backgrounded_head(
           "echo 'launch claude-event-watch &'", WATCHERS))
    ok("& inside heredoc body -> NOT backgrounded",
       not has_backgrounded_head(
           "cat <<'EOF'\nclaude-event-watch &\nEOF", WATCHERS))
    ok("2>&1 on a watcher (foreground) -> NOT backgrounded",
       not has_backgrounded_head("claude-event-watch 2>&1", WATCHERS))
    ok("watcher && other (AND, not bg) -> NOT backgrounded",
       not has_backgrounded_head(
           "claude-event-watch " + "&&" + " echo ok", WATCHERS))
    # A non-watcher backgrounded with & must NOT match the watcher specs.
    ok("non-watcher sleep & -> NOT a watcher background",
       not has_backgrounded_head("sleep 30 &", WATCHERS))
    # backgrounded_segment_heads enumerates the heads regardless of target.
    ok("backgrounded_segment_heads sees the bg head",
       backgrounded_segment_heads("sleep 30 & echo hi") == ["sleep"])
    ok("backgrounded_segment_heads empty when no bg",
       backgrounded_segment_heads("echo hi") == [])

    # --- command_names / command_name_present ---
    ok("command_names: bare command",
       command_names("watcher-ctl run signal") == {"watcher-ctl"})
    ok("command_names: VAR=x prefix stripped",
       command_names("FOO=bar watcher-ctl run") == {"watcher-ctl"})
    ok("command_names: absolute path basenamed",
       command_names("/usr/local/bin/watcher-ctl run") == {"watcher-ctl"})
    ok("command_names: sudo wrapper stripped",
       command_names("sudo watcher-ctl run") == {"watcher-ctl"})
    ok("command_names: compound cd && watcher-ctl",
       command_names("cd ~ && watcher-ctl run x") == {"cd", "watcher-ctl"})
    ok("command_name_present: matched in compound",
       command_name_present("cd ~ && watcher-ctl run x", ["watcher-ctl"]))
    ok("command_name_present: arg-only mention NOT matched",
       not command_name_present("echo watcher-ctl", ["watcher-ctl"]))
    ok("command_name_present: quoted mention NOT matched",
       not command_name_present("echo 'run watcher-ctl now'", ["watcher-ctl"]))
    ok("command_name_present: heredoc body NOT matched",
       not command_name_present(
           "cat <<'EOF'\nwatcher-ctl run x\nEOF", ["watcher-ctl"]))
    ok("command_name_present: env+path combined",
       command_name_present(
           "env FOO=1 /usr/local/bin/watcher-restart", ["watcher-restart"]))
    ok("command_name_present: empty targets -> False",
       not command_name_present("watcher-ctl run", []))
    ok("command_name_present: multi-target hits second",
       command_name_present("event-ack list", ["watcher-ctl", "event-ack"]))

    # --- parse-failure cases raise ShellParseError ---
    for bad in ("echo 'unterminated", 'echo "unterminated', "echo $(unbal"):
        try:
            parse(bad)
            ok(f"unparseable {bad!r} raises", False, "no exception")
        except ShellParseError:
            ok(f"unparseable {bad!r} raises", True)

    # --- ; separator + plain commands ---
    ok("a ; b -> two heads",
       parse("ls ; pwd").heads() == ["ls", "pwd"])

    passed = sum(1 for _, c, _ in cases if c)
    for name, cond, detail in cases:
        status = "PASS" if cond else "FAIL"
        line = f"  {status}  {name}"
        if not cond and detail:
            line += f"  ({detail})"
        print(line)
    print(f"shell_ast: {passed}/{len(cases)} passed")
    return 0 if passed == len(cases) else 1


if __name__ == "__main__":
    import sys as _sys
    if "--test" in _sys.argv:
        raise SystemExit(_run_tests())
    print(__doc__)
