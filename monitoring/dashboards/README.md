# claude-watch Grafana dashboards (canonical, in-repo)

Version-controlled Grafana dashboard JSON for claude-watch's own metrics. Ships
here for the same reason the Prometheus rules next door do: it is written
against the metric names this repo's own exporters emit, so it belongs beside
them, and the alternative is every deployment forking a copy that quietly
drifts.

- **`claude-watch.json`** (uid `claude-watch`) — the daemon dashboard: status,
  heartbeat, context, watchers, hook reminders/fallbacks, interrupts, and
  token usage.
- **`claude-events.json`** (uid `claude-events`) — the event bus: backlog
  depth, oldest-unconsumed age, emission rate split by producer and by tag,
  emitted-vs-consumed, and cumulative totals. Needs
  `exporters/claude-events-exporter/`.
- **`work-queue.json`** (uid `work-queue`) — the `session-task` work queue:
  status and priority breakdowns, active groups, throughput, wait/run/total
  latency quantiles, a run-duration heatmap, and a table of currently-running
  items with elapsed time. Needs `exporters/work-queue-exporter/`.

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

## Layout conventions

- **No `row` panels.** Even an expanded row costs a chevron, a title line and
  an indent guide, and the indent shifts every panel in the group inward —
  expensive on a phone, where Grafana stacks panels full width anyway. Convey
  grouping with panel titles and ordering instead.
- Each band of panels starts at `x: 0` and its widths sum to 24.
- Check both viewports before calling a change done. Valid JSON is not
  evidence that the layout renders.

## Metric provenance

- `claude_watch_*` / `claude_code_*` / `claude_*` → the `claude-watch metrics`
  textfile collector (`src/metrics.rs`)
- `claude_events_*` → `exporters/claude-events-exporter/`
- `worktask_queue_*` → `exporters/work-queue-exporter/`

Alerting on the same metrics lives in `monitoring/prometheus/`.
