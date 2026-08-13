# Optional custom branding

This directory is intentionally empty of images. The image builds with **stock
Grafana branding** unless you put files here.

| file | what it replaces |
|---|---|
| `grafana_icon.svg` | the sidebar mark and the loading logo |
| `fav32.png` | favicon and apple-touch-icon (a square PNG; 192×192 or larger is fine) |

Drop either or both in and rebuild. Neither is required, and supplying only one
is supported.

Two details the Dockerfile handles for you, both worth knowing before you
hand-roll something similar:

- Grafana 13 serves branding out of `public/build/img/`, where 11.x used
  `public/img/`. The build writes both.
- The sidebar mark is a webpack **content-hashed** URL
  (`static/img/grafana_icon.<hash>.svg`) served with a one-year `max-age`.
  Overwriting the file in place leaves the URL unchanged, so browsers that
  already cached the stock icon keep serving it indefinitely — the copy is
  still "fresh" as far as they are concerned. The build therefore renames the
  asset to a sentinel and rewrites every JS reference to it. If you later
  change your icon, bump the sentinel so the URL changes again:

  ```
  docker build --build-arg BRAND_SENTINEL=custom002 .
  ```

**Not replaced:** `grafana_text_logo_{dark,light}.*.svg`, the wide (~4:1)
wordmark SVGs used in the dashboard header. Substituting a square icon there
collapses the whole nav row on narrow viewports — dashboard title, breadcrumbs,
user menu and share button all disappear. If you want to rebrand the wordmark,
supply a matching wide-aspect asset and add a separate copy step; do not reuse
the square icon.
