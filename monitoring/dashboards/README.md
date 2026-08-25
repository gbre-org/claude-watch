# claude-watch Grafana dashboards (canonical, in-repo)

Version-controlled Grafana dashboard JSON for claude-watch's own metrics. Ships
here for the same reason the Prometheus rules next door do: it is written
against the metric names this repo's own exporters emit, so it belongs beside
them, and the alternative is every deployment forking a copy that quietly
drifts.

- **`claude-watch.json`** (uid `claude-watch`) — the daemon dashboard: status,
  heartbeat, context, watchers, hook reminders/fallbacks, interrupts, and
  token usage. Build Info (version/commit/PR of the running binary) is pure
  Prometheus — no token, no GitHub call. One separate, optional panel
  ("Latest Merged PR") needs the Infinity datasource; see
  [Infinity](#infinity--optional-one-panel-in-claude-watchjson) below.
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

### Infinity — optional, one panel in `claude-watch.json`

One panel is not Prometheus: **Latest Merged PR** queries GitHub's REST API
directly through the
[Infinity](https://grafana.com/grafana/plugins/yesoreyeram-infinity-datasource/)
datasource. It is the only Infinity target in this repo, and it is
deliberately exporter-free: a local exporter scraping GitHub for one number is
a service to run, restart and forget, and the number is already public.

This panel used to be a third tile inside **Build Info**, sharing that panel's
`-- Mixed --` datasource with the two Prometheus tiles (version/commit/PR).
That was the recurring failure mode Andrew hit as #5482/#5548: Grafana's
`/api/ds/query` batches every target of a Mixed-datasource panel into one
request and fails the WHOLE response the instant any one target errors, so a
GitHub rate-limit (below) or a missing Infinity plugin/datasource blanked the
two otherwise-healthy Prometheus tiles too, not just the GitHub one — a
"skip it and only that tile fails to render" assumption that does not hold in
practice (verified against Grafana 13.1.3). As of 2026-08-25 this is its own
panel on its own single datasource, so a GitHub outage is confined to this one
panel and can never again take the build-identity tiles down with it.

This repo ships no Grafana service — nothing here provisions one, so there is
no compose file to add the plugin to. A deployment that wants the panel needs
two things in **its own** Grafana:

1. The plugin installed. On the official image that is one env var —
   `GF_INSTALL_PLUGINS=yesoreyeram-infinity-datasource` (verified against
   plugin 4.0.0) — or `grafana-cli plugins install
   yesoreyeram-infinity-datasource` on a package install.
2. The datasource provisioned with the stable uid the dashboard binds to,
   exactly as with `uid: prometheus` above:

```yaml
# provisioning/datasources/infinity.yml
apiVersion: 1
datasources:
  - name: Infinity
    type: yesoreyeram-infinity-datasource
    uid: infinity
    access: proxy
    jsonData: {}
```

Skip both and nothing else breaks: this one panel does not render (Grafana
flags the missing datasource on it), and Build Info's Prometheus tiles are
unaffected — they are on a different panel now, not just a different tile.

**Rate limit is the design constraint here, and it is what sets this
dashboard's refresh.** The call is unauthenticated, so GitHub allows 60
requests/hour/IP, and Grafana has no per-panel refresh interval — while the
panel is in the viewport it fetches once per *dashboard* refresh (panels
scrolled out of view are not queried). At the `30s` this file used to ship,
that is ~120/hour worst case: a viewport parked on the panel exhausts the
budget in half an hour, GitHub answers 403, and the tile shows a query error
until the hour rolls. The panel's `interval: 5m` documents the intended
ceiling but does not enforce it, because Infinity ignores min interval.

So `refresh` is `5m` (2026-08-22), which puts the worst case at 12/hour. It
costs this board nothing: the fastest-moving tiles are second-resolution ages
a viewer reads as "roughly how long", and every alerting decision is made by
the Prometheus rules in `monitoring/prometheus/`, never by a rendered panel.
A deployment that genuinely wants sub-minute refresh should give the Infinity
datasource a GitHub token (5,000/hour) first, and only then lower `refresh`.

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
- `worktask_exporter_build_info` → the work-queue-exporter, stamped at image
  build. No dashboard tile since 2026-08-23 (the Build Info panel is about
  the daemon, not the exporter); read it with `curl -s localhost:9099/metrics
  | grep build_info`. See `monitoring/prometheus/README.md`.
- "latest merged" (Latest Merged PR panel) → no metric at all: Infinity
  fetches `api.github.com/repos/hndrewaall/claude-watch/pulls` at render time

Alerting on the same metrics lives in `monitoring/prometheus/`.
