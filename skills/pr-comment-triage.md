---
name: pr-comment-triage
description: "Triage accumulated bot + human comments on recently-updated tracked PRs: classify each by content, collapse bot noise, keep real signal, and DRAFT (never auto-post) human replies"
argument-hint: "[PR number or repo hint]"
allowed-tools: ["Bash", "Read", "Grep"]
---
Triage the accumulated bot + human comments on recently-updated tracked PRs: pull each PR's comment state, classify every comment by CONTENT (never by author), then ACT — collapse bare-status bot noise, KEEP real signal visible, and DRAFT (never auto-post) any human-facing reply. Scope is PRs updated within the last ~24h. Collapse, don't delete. This is agent work, not a script — the judgment of signal-vs-noise is the whole point.

**Governing memory: `feedback_pr_comment_triage_act_or_collapse`.** Read it — it is the source of truth for the rules below (Andrew botchat #3328–#3354, 2026-08-10). Two load-bearing corrections it encodes: (1) triage = ACT if appropriate, not merely collapse; (2) do NOT post replies autonomously — DRAFT them for Andrew to review.

## The model

pr-watch surfaces PR issue-comments (comments on the PR conversation, not just review-thread/diff comments) as ambient events. A dumb producer script only DETECTS "this PR has bot comments an agent should look at"; ALL signal-vs-noise judgment lives here, in the dispatched agent. That split is deliberate (`feedback_dispatcher_not_worker`): "scripts should not be triaging, that's a job for an agent."

## Scope

- **PRs updated within the last ~24h**, from the pr-watch tracked set (`pr-watch list`, or the runtime `tracked.json`). Don't sweep the whole backlog — recency bounds the work.
- **Multiple gh accounts:** personal and work PRs typically authenticate as different GitHub users. Select the right one **per call** with `GH_CONFIG_DIR=<that account's gh config dir>` — NEVER `gh auth switch`, which mutates global state and races any concurrent agent.

## Steps

1. **Pull comment state per PR.** Issue-comments:
   `env GH_CONFIG_DIR=<dir> gh api repos/<owner>/<repo>/issues/<n>/comments --paginate`.
   (Review/diff comments if in scope: `.../pulls/<n>/comments`.) Capture each comment's author, body, and node id (the node id is what `minimizeComment` needs).

2. **Classify each comment by CONTENT — never hardcode an author as always-noise** (not even `sonarqube-as-a-service` or `sfci-gec-github-app`). Decision tree:

   - **Bare-status bot noise** → COLLAPSE. Pure "Quality Gate passed", build-status pings, superseded/duplicate status posts, consumed retrigger acks. No content beyond the status.
   - **Real signal — KEEP VISIBLE (and maybe act):**
     - SonarQube flagging a *real* new-code issue or coverage gap (not a bare pass),
     - STAR failure-analysis with actual root-cause detail,
     - prizm findings with a genuine bug,
     - Provision-Pipeline-Required / branch-protection / reviewer-count blockers,
     - any real review finding or change request.
   - **Human question** → surface to Andrew / answer it.
   - **Default to KEEPING VISIBLE when unsure.** Collapsing real signal is the costly error; leaving a borderline bot comment up is cheap.

3. **ACT per classification:**
   - **Collapse** bot noise via the GitHub GraphQL `minimizeComment` mutation with `classifier: OUTDATED` — reversible, NOT deletion:
     ```graphql
     mutation { minimizeComment(input: {subjectId: "<node-id>", classifier: OUTDATED}) { minimizedComment { isMinimized } } }
     ```
     (via `gh api graphql -f query='...'`, with the right `GH_CONFIG_DIR`). Same approach used to collapse 33 automation comments across the rc-prep PRs on 2026-08-10.
   - **Real review finding** → address it: fix the code + resolve if genuinely handled (a self-review reword is fine to push per normal PR conventions). But any reply text to ANOTHER human gets DRAFTED, not auto-posted.
   - **Human-facing reply** → DRAFT it for Andrew (via botchat) to post/approve. **NEVER auto-post comments/replies to PRs** (Andrew #3354: "going forward please send me drafts of replies you want to post. dont send autonomously"). Drafts are the deliverable (`feedback_drafts_are_finished_work`).

4. **Report** the sweep: per PR, what was collapsed (count + why), what was kept visible (and any action taken / drafted), and any human questions surfaced. Do NOT put counts in PR descriptions (they go stale — `feedback_no_counts_in_pr`).

## Important

- **Collapse, NOT delete.** `minimizeComment` is reversible; deletion is not. Andrew accepts the noise of surfacing bot comments *because* they auto-collapse.
- **Judge by content, every time.** The reason this is a skill (agent work) and not a script is that the same author emits both noise AND signal. A script can only detect "comments exist"; the agent decides which are which.
- **Never auto-post.** Human-facing replies are drafted for review. Code fixes follow normal PR conventions (push OK for claude-watch; ASK for work repos per `feedback_never_open_pr_unprompted`).
- Retrigger convention is unchanged: empty-commit, NOT comments (`feedback_auto_retry_flakes`).

## Escalation: `comment-triage-STUCK` (fallback gate, botchat #3610)

The detector re-emits `comment-triage-needed` on a dwell while a backlog stays
live. Acking those events is NOT the same as resolving them: if the loop
ack-loops "residual" cycle after cycle without the un-triaged count ever
DROPPING, the detector escalates — after ~5 consecutive no-progress emissions it
emits the distinct, louder `comment-triage-STUCK` event (still actionable) and
sends an operator Pushover ping. When you see `comment-triage-STUCK`, a plain
residual-ack is no longer acceptable: you must either **make real progress so
the un-triaged count drops** (collapse the bot noise / answer or resolve the
human comment/thread — which self-heals the escalation on the next poll) or, if
the backlog genuinely cannot be resolved autonomously, **explicitly surface it to
Andrew** (a botchat ping) rather than acking it again. The count-drop is what the
detector measures, so partial collapses that actually shrink the backlog reset
the stuck counter.

## Dispatching a sweep

For a multi-PR sweep, delegate to an agent using a reusable
`pr-comment-triage-sweep` agent-prompt template kept in the operator's own
private config repo (parameterized by PR set + account). Queue it
(`session-task queue`, scope the relevant repo) and spawn per the 5-step
protocol.

## Related

- Memory: `feedback_pr_comment_triage_act_or_collapse` (source of truth), `reference_sonarqube_nonrequired_gate_pr_warning` (bot-gate noise).
- `/pr-watch` — how PR state/comments get surfaced as ambient events.
- `agent-prompts/pr-comment-triage-sweep.md` — the dispatchable sweep template.
- This skill is the first worked instance of `/claude-container:distill` (the distillation metaskill).
