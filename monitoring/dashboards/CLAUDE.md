# Grafana dashboards — upstream artifact, NOT a live path

This directory holds the version-controlled Grafana dashboard JSON for
claude-watch's own metrics (`claude-watch.json`, `claude-events.json`,
`work-queue.json`). It is the **upstream template** and the source of truth
*for the project*: an improvement belongs here so every deployment can pick it
up from one place.

**Nothing live reads this path.** No running Grafana loads a file from this
directory, and no deploy surface in this repo provisions one: `make
deploy-systemd` builds and installs the binary, installs skills, and restarts
the service; the container image ships no Grafana. Neither path reads
`monitoring/dashboards/*.json`.

## Editing here changes no live dashboard

A deployment provisions Grafana from **its own** dashboards directory, which it
populates by copying (or symlinking individual files) from a checkout whose
lifetime it controls. Consequences, both directions:

- Writing, reviewing and merging a dashboard PR here deploys **nothing**. The
  running dashboard is unchanged until someone updates that deployment's own
  copy. Do not report a merged PR as a shipped dashboard change.
- A dashboard improved anywhere else — Grafana's UI, a deployment's own
  directory, another repo — is invisible here until it is ported back, and
  drifts from this copy meanwhile. Port it into these files.

## Any sync is uid-aware, never a plain `cp`

Panels here bind their datasource by the portable placeholder
`uid: prometheus` (and, for the one GitHub-reading tile in `claude-watch.json`,
`uid: infinity`). A deployment whose Prometheus datasource was created through
the Grafana UI instead has a generated uid (`P1234567890ABCDEF`), so its copy
will differ from this file by at least that value. Copying verbatim in
**either** direction can therefore point every panel at a datasource that does
not exist. Use the uid-rewriting `jq` recipe in
[`README.md`](README.md#datasource-binding), and never commit a generated uid
back here.

## Read the README before editing a JSON file

[`README.md`](README.md) in this directory is the reference and this file
defers to it: what each dashboard covers, the mount-target hazard (Grafana's
file provisioner **deletes** every dashboard not present in its directory, so
never bind-mount a checkout of this repo as that volume), datasource binding,
layout conventions, and metric provenance.

Validate any edit with `jq empty monitoring/dashboards/*.json`.
