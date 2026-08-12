---
name: distill
description: "Distill completed work into a reusable artifact (skill / agent-prompt / CLI tool / memory) via IDENTIFY -> CHOOSE -> DRAFT -> PLACE"
argument-hint: "[topic hint]"
allowed-tools: ["Bash", "Read", "Write", "Edit", "Glob", "Grep"]
---
Distill a completed piece of work — a transcript, a finished session, or a pattern you just hand-rolled for the 2nd or 3rd time — into a REUSABLE ARTIFACT (a skill, an agent-prompt template, a CLI tool, or a memory). This is the structured, four-phase superset of the terse `/generalize` nudge: `/generalize` says "generalize this and document it"; `/distill` gives you the decision-tree for WHAT to build, in WHAT format, and WHERE it lives.

**Use this when you catch yourself repeating.** The trigger is recognition: "I've now written this same sweep brief three times", "this decision-tree keeps recurring", "I keep re-deriving this gotcha". Distillation converts that ad-hoc repetition into a first-class, invocable artifact so the next occurrence is one command, not a re-derivation.

## Relationship to `/generalize`

`/generalize` is a **one-line prompt** (`Now generalize this approach into new functionality for our tools and document accordingly.`) — a quick, unstructured nudge with no guidance on artifact type, format, or placement. `/distill` is its **structured superset**: same intent (turn a proven approach into reusable functionality + docs), but with an explicit IDENTIFY → CHOOSE → DRAFT → PLACE workflow and heuristics for each step.

- Reach for **`/generalize`** when the target is obvious and you just want the express-lane nudge (you already know it's "a new flag on tool X, documented").
- Reach for **`/distill`** when you need to DECIDE what shape the artifact takes — skill vs. agent-prompt vs. CLI vs. memory — and where it should live. `/distill` is the default when distilling a whole session or a cross-cutting pattern.

They sit **beside** each other: `/generalize` stays as the terse quick-invoke; `/distill` is the full workflow. If you invoke `/generalize` and realize the scope is bigger than "one obvious change", escalate to `/distill`.

## The workflow

### (a) IDENTIFY the pattern

Look back over the work and name the *generalizable core* — the part that will recur, stripped of this instance's specifics.

Ask:

- **What got hand-rolled 2–3 times?** A brief you re-typed, a command sequence you re-assembled, a classification you re-made by hand. Repetition count is the strongest signal — one occurrence is not yet a pattern; three is a mandate.
- **What decision-tree recurred?** If you found yourself re-deriving the same "if X collapse, if Y keep, if Z draft" branching, that tree is the artifact.
- **What toil was purely mechanical?** Fetching, filtering, formatting, GraphQL calls — anything a script could do deterministically.
- **What gotcha bit you (or would bite the next session)?** A non-obvious constraint, a preference, a "don't do X because Y" — that's a memory, not a tool.

Write one sentence: *"The reusable core is: ___."* If you can't, the work isn't ready to distill — it's still too instance-specific.

### (b) CHOOSE the artifact type

Pick by the nature of the reusable core. Heuristics (in priority order — check top-down):

| If the core is… | Build a… | Because |
| --- | --- | --- |
| **Deterministic, mechanizable toil** (fetch/filter/format/API-call with no judgment) | **CLI tool** | A script does it identically every time, zero tokens, zero drift. If a human/agent would do the exact same keystrokes each time, script it. |
| **A repeatable multi-step WORKFLOW a human/agent drives** (has judgment calls, a decision-tree, "it depends") | **SKILL** (`/name`) | Skills encode *how to think through* a recurring task. The judgment stays with the agent; the skill supplies the structure + guardrails. |
| **A one-shot task you'll DISPATCH to a subagent** with variable inputs (a "go do this sweep/check/build" brief) | **AGENT-PROMPT template** | Parameterized (`{{VAR}}`) prompt bodies spawned via the Agent tool. The template is the *contract* for a delegated unit of work. |
| **A gotcha, preference, or non-obvious constraint** (no steps — just a fact the next session must know) | **MEMORY** | Memories are searchable background facts, not procedures. "Never do X", "Andrew prefers Y", "Z lies about its state." |

Common combinations — distilling one session often yields SEVERAL artifacts:

- A **skill** (the workflow) + an **agent-prompt** (the dispatch brief the skill spawns) + a **memory** (the load-bearing gotcha the skill must honor). This is the canonical trio.
- **Split judgment from mechanism**: when a task has both a dumb mechanical part and a judgment part, build a **CLI tool** for the mechanism (a detector/producer) AND a **skill or agent-prompt** for the judgment — never bake judgment into the script. (See `feedback_pr_comment_triage_act_or_collapse`: "scripts should not be triaging, that's a job for an agent; you can have a job that DETECTS when an agent is needed and emits an event.")

### (c) DRAFT the artifact in the right format

- **Skill** → a Markdown file, first line = the prompt-injection summary (what shows in listings), then `## Steps` / `## Important` / `## When NOT to use`. Match the tone of the siblings in this dir (short, punchy). See the [skills/ README](README.md) for the shape and for the shared-vs-container-only split.
- **Agent-prompt template** → a Markdown file with a `## Prompt` fenced block containing the parameterized body (`{{VAR}}` placeholders), plus `## Scope` and `## Used by`. Agent-prompts live in the operator's own private config repo (a per-operator `agent-prompts/` dir), not here.
- **CLI tool** → follow the repo's existing tool conventions (Rust for low-level helpers per `feedback_rust_not_c_for_lowlevel`; Python via `uv` for higher-level). Baked container tools live in `container/bin/`; host tools under `~/repos/<tool>/`.
- **Memory** → a `feedback_*` (preference/correction) or `reference_*` (fact/how-to) file in the memory dir, with the generic-but-personal guidance (not work-state). Back every correction with a memory per `feedback_always_save_corrections`.

Draft it FULLY — a stub is not a distillation. The test: could a fresh session invoke it and succeed without you re-explaining?

### (d) PLACE it — note where it lives + how it's invoked

- **Shared claude-watch skill** (works in BOTH deployment modes) → `skills/<name>.md` in the claude-watch repo → installed on the host by `make install-skills` as `~/.claude/commands/cw-<name>.md`, invoked `/cw-<name>`; ALSO baked into the container image, invoked `/claude-container:<name>`. See the [skills/ README](README.md).
- **Container-only claude-watch skill** → `container/skills/<name>.md` → baked to `/opt/claude-container/skills/` + the plugin `commands/` dir → invoked as `/claude-container:<name>`. Requires container-build + force-recreate to go live (NOT `cwsr`). See the [container/skills/ README](../container/skills/README.md) "How to add a new skill".
- **Operator-private host skill** (not shippable in a public repo — names private paths, personal services, work accounts) → the operator's own `commands/` dir, symlinked into `~/.claude/commands/<name>.md` → invoked as `/<name>`.
- **Agent-prompt** → the operator's private config repo, under its `agent-prompts/<name>.md` (a SEPARATE repo — its own commit + host-only push). Referenced by skills / spawned via the Agent tool.
- **CLI tool** → baked (`container/bin/`, needs rebuild) or host (`~/repos/<tool>/`); note the PATH + any launchd/cron wiring.
- **Memory** → the project memory dir; add an index line to `MEMORY.md`.

State the path AND the invocation in your report so the artifact is discoverable.

## Important

- **Don't force-bake.** Drafting a container skill/tool does NOT deploy it — baked artifacts need container-build + force-recreate, which is the operator's call. Draft the file, open a draft PR (`/cw-pr` for claude-watch), and report; let the operator decide when to rebuild.
- **Cross-repo artifacts = separate commits.** A session often produces a claude-watch skill AND an agent-prompt in the operator's private config repo AND a memory — three different repos/paths, three commits. Flag each explicitly; don't assume one PR covers all.
- **One artifact per concern.** If the session yielded three unrelated reusable pieces, draft three artifacts — don't cram them into one mega-skill.
- **The first instance of this metaskill was `/pr-comment-triage`** — the PR-comment triage workflow distilled from a session that hand-wrote the same sweep brief three times. Use it as the worked reference for what "good distillation" looks like: a skill (the triage workflow) + an agent-prompt (the sweep brief) + a referenced memory (`feedback_pr_comment_triage_act_or_collapse`).

## When NOT to use

- **The work was a genuine one-off** — no recurrence signal, no future caller. Distilling premature abstractions is toil that produces dead skills. Wait for the 2nd or 3rd occurrence.
- **The reusable core is already captured** by an existing skill/tool — extend it instead of drafting a duplicate (the `/generalize` "into new functionality for OUR TOOLS" framing: prefer growing what exists).
