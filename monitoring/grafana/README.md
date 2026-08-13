# Solarized Grafana image (canonical, in-repo)

A Grafana image with a Solarized dark/light palette baked in, plus a few
usability patches. Ships here for the same reason the Prometheus rules next
door do: it is a build-time artifact several deployments want to share, and the
alternative is each one forking a copy that quietly drifts.

**Pinned to `grafana/grafana:13.1.3`.**

Nothing here is claude-watch-specific — no dashboards, no datasources, no
compose file. It is just the image. Bring your own provisioning.

## Contents

| file | role |
|---|---|
| `Dockerfile` | the image; every patch is documented inline |
| `solarized.css` | runtime stylesheet override, injected into `index.html` |
| `kiosk-button.js` | floating mobile kiosk toggle, injected into `index.html` |
| `pre-sed-snapshot.py` | hashes every JS bundle before mutation |
| `rehash-mutated-bundles.py` | renames mutated bundles to their new content hash and fixes all references |
| `branding/` | optional drop-in icon/favicon; empty by default (see its README) |

## Using it

```bash
docker build -t grafana-solarized:13.1.3 .
```

From compose, pointing at this directory as the build context:

```yaml
services:
  grafana:
    build:
      context: ./path/to/monitoring/grafana
    image: grafana-solarized:13.1.3
    environment:
      GF_USERS_DEFAULT_THEME: "dark"
    volumes:
      - ./provisioning:/etc/grafana/provisioning:ro
      - ./dashboards:/var/lib/grafana/dashboards:ro
    ports:
      - "3000:3000"
```

Build args, all optional:

| arg | default | effect |
|---|---|---|
| `GF_APP_TITLE` | `Grafana` | browser-tab and login-page title; skipped entirely at the default |
| `STRIP_POWERED_BY` | `false` | removes the kiosk / solo-panel "Powered by" footer |
| `BRAND_SENTINEL` | `custom001` | cache-bust tag for a custom sidebar icon |

`STRIP_POWERED_BY` is off by default. The mechanism is documented because it is
the most fragile patch in the file, but check Grafana's trademark and
attribution guidance before enabling it on anything public-facing.

## Why patch the bundles at all

Grafana OSS has no supported way to replace the palette of a built-in theme. So
the image does two complementary things:

1. **Rewrites the compiled bundles** — the palette exists in the CSS *and* as
   JS objects (panel chrome, chart axes and tooltips read `theme.colors.*` at
   runtime). A CSS-only patch leaves half the UI stock.
2. **Injects a stylesheet** — Emotion class names change between releases, so
   the stylesheet catches what a colour sed misses, and vice-versa. It also
   carries selectors that cannot be expressed as a colour substitution at all,
   e.g. Grafana 13's table panel is react-data-grid and paints from a set of
   `--rdg-*` custom properties whose light-mode value is plain `#ffffff` —
   which deliberately is *not* sed'd, since rewriting every `#ffffff` in the
   bundles would wreck text, borders and icons.

This is only repeatable because the bundles are content-hashed but stable
within a release, which is why the `FROM` line is pinned exactly rather than to
a range.

## The two properties that matter most

**Every patch that can fail silently ends in a `grep` guard that fails the
build instead.** A sed that matches nothing is not an error to `sed`: the build
goes green and the patch is silently gone. That is exactly how the kiosk footer
came back once — a literal pattern written against one release matched zero
files in the next. Guards turn that into a red build with a message naming the
patch. If you add a patch, add its guard.

**Minified identifiers are never hardcoded.** Webpack re-mints short names on
essentially every release, and they differ *between bundles within a single
build* — so no literal identifier can be correct for long, and none can cover
every bundle even today. The JS patches anchor on things that are actually
stable: string literals, argument shapes, and method boundaries.

## Bumping the Grafana version

1. Change the `FROM` tag.
2. `docker build .` and read the output. A guard failure names the patch that
   needs re-deriving; the Dockerfile comment above each one includes the
   command to re-grep the new minified form.
3. Run it and look at a dashboard in both themes. Colour seds do not have
   guards — a palette entry that upstream renamed simply stops matching, and
   the only symptom is a stock-grey panel somewhere. Add the new value to the
   lists.

Two busybox-sed constraints the trickier expressions are shaped around, both of
which cost a debugging cycle and neither of which should be "simplified" away:
backreferences do not work on the *search* side of `sed -E`, and using `|` as
the delimiter makes busybox un-escape `\|` back into alternation — which
degenerates to matching the empty string at offset 0 and corrupts the bundle
rather than failing. Both are called out at their use sites.

## Cache invariants

Three separate mechanisms, all solving the same problem — an asset served with
a one-year `max-age` at a URL that did not change is pinned forever, in
browsers and in any CDN in front of the deployment, and hard-refresh does not
reliably help:

- **Mutated JS bundles** → `rehash-mutated-bundles.py` renames each one to its
  new content hash and rewrites every cross-bundle reference, restoring
  webpack's "URL is immutable because it contains the hash" contract.
- **`solarized.css`** → the `<link href>` gets a content-hash query string,
  computed *after* the CSS seds run so it describes the bytes actually served.
  A change to a sed rule busts the cache too, not just a change to the file.
- **`kiosk-button.js`** → same treatment on its `<script src>`.

All three are content-derived rather than timestamp-derived, so an unchanged
build keeps its URLs and does not evict valid cache entries.
