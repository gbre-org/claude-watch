# Grafana dashboards — CANONICAL, edit HERE

`claude-watch.json` in this directory is the **canonical claude-watch Grafana
dashboard**. It is the source of truth and is what the live Grafana loads.

## Shared across hosts — this is the ONLY correct place to edit

The `claude-watch` repo is checked out on **both** the container/dev host and
the **gb / gomorrah** host. Because both machines have this repo, this JSON is
the **shared, portable** definition every host can see and provision from.

- **Edit dashboards HERE**, in `claude-watch/monitoring/dashboards/`.
- **Do NOT edit them in `andrew-sf-tools`** (or any private Salesforce-work
  repo). `andrew-sf-tools` is **SF-private — gb / gomorrah CANNOT see it**, so
  any dashboard work done there is invisible to the other host and drifts out
  of sync with the canonical copy. A "Session Timers" consolidation was once
  built there by mistake and had to be ported back into this file.

If you improve a dashboard, the change belongs in this file and this repo so
it is shared with every host and picked up by the live Grafana. Treat a
dashboard edited only in `andrew-sf-tools` as a mistake to be ported here.
