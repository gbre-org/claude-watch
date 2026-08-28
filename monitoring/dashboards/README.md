# claude-watch Grafana dashboards (canonical, in-repo)

Version-controlled Grafana dashboard JSON for claude-watch's own metrics. Ships
here for the same reason the Prometheus rules next door do: it is written
against the metric names this repo's own exporters emit, so it belongs beside
them, and the alternative is every deployment forking a copy that quietly
drifts.

- **`claude-watch.json`** (uid `claude-watch`) — the daemon dashboard: status,
  heartbeat, context, watchers, hook reminders/fallbacks, interrupts, and
  token usage. Build Info (commit/PR of the running binary, plus GitHub's
  latest-merged PR) is `-- Mixed --`: two tiles from Prometheus, one from the
  Infinity datasource; see
  [Infinity](#infinity--one-mixed-tile-in-build-info) below.
- **`claude-events.json`** (uid `claude-events`) — the event bus: backlog
  depth, oldest-unconsumed age, emission rate split by producer and by tag,
  emitted-vs-consumed, and cumulative totals. Needs
  `exporters/claude-events-exporter/`.
- **`work-queue.json`** (uid `work-queue`) — the `session-task` work queue:
  status and priority breakdowns, active groups, throughput, wait/run/total
  latency quantiles, a run-duration heatmap, and a table of currently-running
  items with elapsed time. Needs `exporters/work-queue-exporter/`.
- **`agent-psi.json`** (uid `agent-psi`) — pressure-stall (PSI) metrics over
  the Claude Code agent fleet: live-agent count, fleet inference/tool
  pressure at multiple sliding windows, and per-agent duty cycle. Needs
  `exporters/agent-psi-exporter/`.

Validate with `jq empty monitoring/dashboards/*.json`.

## This is a source of record, NOT a mount target

**Do not bind-mount this directory (or any checkout of this repo) as a
Grafana dashboards volume.** Copy the JSON into the deployment's own
dashboards directory, or symlink individual files from a checkout you control
the lifetime of.

That rule is not stylistic. Grafana's file provisioner treats its dashboards
directory as authoritative: whatever is not in the directory is deleted from
the database. Point that mount at a path whose contents are not guaranteed —
an ephemeral or throwaway checkout, a worktree that gets pruned and recreated,
a build directory — and the moment it is empty or missing Grafana sees zero
dashboards and removes every provisioned dashboard it had. That failure has
happened, twice, silently, and it is why the dashboards were pulled out of
this repo before being restored here as plain source.

Keeping the repo copy purely a source of record is what makes it safe to have
one at all: nothing live reads this path, so the repo's own working tree can
be created, pruned, or rewritten with no effect on a running Grafana.

## Datasource binding

Every panel references its datasource by **`uid: prometheus`**. That is a
deliberate, portable placeholder, not an instance identifier — provision the
Prometheus datasource with a stable `uid: prometheus` and the dashboard binds
with no editing:

```yaml
# provisioning/datasources/prometheus.yml
apiVersion: 1
datasources:
  - name: Prometheus
    type: prometheus
    uid: prometheus
    access: proxy
    url: http://prometheus:9090
    isDefault: true
```

A datasource created through the UI instead gets a generated uid of the form
`P1234567890ABCDEF`, which is meaningful only on the instance that minted it.
If a deployment already has one, re-point the copy rather than committing the
generated value back here:

```bash
jq '(.. | objects | select(.type? == "prometheus") | .uid) = "YOUR_UID"' \
  claude-watch.json > /path/to/deployment/dashboards/claude-watch.json
```

### Infinity — one Mixed tile, in Build Info

**Build Info** is `-- Mixed --`: two tiles (commit, PR) come from Prometheus,
the third ("latest merged") queries GitHub's REST API directly through the
[Infinity](https://grafana.com/grafana/plugins/yesoreyeram-infinity-datasource/)
datasource. It is the only Infinity target in this repo, and it is
deliberately exporter-free: a local exporter scraping GitHub for one number is
a service to run, restart and forget, and the number is already public.

This tile spent 2026-08-25–08-26 as its own standalone panel ("Latest Merged
PR", #716/#5548, then #717): Grafana's `/api/ds/query` batches every target of
a Mixed-datasource panel into one request and fails the WHOLE response the
instant any one target errors, so an *unauthenticated* GitHub rate-limit or a
missing Infinity plugin/datasource used to blank the two otherwise-healthy
Prometheus tiles too, not just the GitHub one (verified against Grafana
13.1.3). It moved back into Build Info as a Mixed tile on 2026-08-26 (Andrew
#5629) because the Infinity datasource is now provisioned with a GitHub token
(andrew-sf-tools, 5,000 req/hour) rather than calling anonymously — at that
rate the batched-failure risk this datasource used to carry is low enough to
accept trading it back for one panel instead of two. **The failure mode is
still real, just rarer**: if the Infinity datasource is missing, misconfigured,
or the token is revoked/exhausted, Build Info's commit/PR tiles blank along
with "latest merged" — they are one panel again, not two.

This repo ships no Grafana service — nothing here provisions one, so there is
no compose file to add the plugin to. A deployment that wants the tile needs
three things in **its own** Grafana:

1. The plugin installed. On the official image that is one env var —
   `GF_INSTALL_PLUGINS=yesoreyeram-infinity-datasource` (verified against
   plugin 4.0.0) — or `grafana-cli plugins install
   yesoreyeram-infinity-datasource` on a package install.
2. The datasource provisioned with the stable uid the dashboard binds to,
   exactly as with `uid: prometheus` above.
3. A GitHub token in that datasource's config (bearer auth header), so the
   call runs at 5,000 req/hour instead of 60 req/hour/IP anonymous —
   otherwise the rate-limit exposure described below reappears, now against
   the whole Build Info panel rather than one tile.

```yaml
# provisioning/datasources/infinity.yml
apiVersion: 1
datasources:
  - name: Infinity
    type: yesoreyeram-infinity-datasource
    uid: infinity
    access: proxy
    jsonData: {}
    # + a secureJsonData bearer token for the GitHub call, in deployments
    # that want the 5,000/hour authenticated rate limit above.
```

Skip the datasource entirely and the whole **Build Info** panel goes
warning-icon-blank (Grafana batches the missing-datasource error in with the
two healthy Prometheus targets) — this is the tradeoff #5629 explicitly
accepted, not an oversight.

**Rate limit is still the design constraint, and it still sets this
dashboard's refresh.** Grafana has no per-panel refresh interval — while the
panel is in the viewport it fetches once per *dashboard* refresh (panels
scrolled out of view are not queried). Authenticated at 5,000/hour, the
`refresh: 5m` this dashboard ships with (12 fetches/hour) is nowhere near the
ceiling; it was originally sized (2026-08-22) for the *unauthenticated*
60/hour case, where a viewport parked on the panel at `30s` refresh exhausted
the hour's budget in 30 minutes and GitHub answered 403 until the window
rolled. The panel's `interval: 5m` documents the intended ceiling but does not
enforce it, because Infinity ignores min interval — the dashboard `refresh` is
what actually caps the call rate.

The query ranks by `merged_at`, not by PR number — number order and merge order
diverge whenever two PRs are open at once — via the JSONata root selector
`$[merged_at != null]^(>merged_at)[0]`, evaluated by Infinity's **backend**
parser. `sort=updated` in the URL only pulls recent PRs into the first page; a
comment bumps `updated_at` without merging anything, so the ordering that
decides the answer is redone in the selector.

## Layout conventions

- **No `row` panels.** Even an expanded row costs a chevron, a title line and
  an indent guide, and the indent shifts every panel in the group inward —
  expensive on a phone, where Grafana stacks panels full width anyway. Convey
  grouping with panel titles and ordering instead.
- Each band of panels starts at `x: 0` and its widths sum to 24.
- **`options.text.valueSize` is chosen at 390px, never at desktop width.** A
  phone stacks every panel to full width, so a three-tile stat gets ~93px of
  text per tile — and Grafana does not shrink to fit. A value with no break in
  it (a commit sha, `#699`) overflows sideways THROUGH its neighbour; a value
  with a space (`57.7 mins`) wraps its unit onto a second line and grows
  downward until it clips the panel. 24 is where every three-tile stat here
  landed; anything larger has to be re-measured at 390px before it ships.
- Check both viewports before calling a change done. Valid JSON is not
  evidence that the layout renders.

## Metric provenance

- `claude_watch_*` / `claude_code_*` / `claude_*` → the `claude-watch metrics`
  textfile collector (`src/metrics.rs`)
- `claude_events_*` → `exporters/claude-events-exporter/`
- `worktask_queue_*` → `exporters/work-queue-exporter/`
- `agent_psi_*` / `agent_duty_ratio` → `exporters/agent-psi-exporter/`
- `worktask_exporter_build_info` → the work-queue-exporter, stamped at image
  build. No dashboard tile since 2026-08-23 (the Build Info panel is about
  the daemon, not the exporter); read it with `curl -s localhost:9099/metrics
  | grep build_info`. See `monitoring/prometheus/README.md`.
- "latest merged" (Build Info's third tile) → no metric at all: Infinity
  fetches `api.github.com/repos/hndrewaall/claude-watch/pulls` at render time

Alerting on the same metrics lives in `monitoring/prometheus/`.
