# Grafana with Solarized dark/light theme baked in.
#
# This image patches Grafana's compiled CSS and JS bundles at build time to
# replace Grafana's default dark palette with Solarized Dark/Light colors. It
# also injects a solarized.css stylesheet that overrides CSS custom properties
# and broad selectors at runtime (belt-and-suspenders for Emotion-generated
# class names that change between Grafana versions).
#
# The sed approach works because Grafana's webpack bundles are content-hashed
# but STABLE within a release version — a build-time patch is repeatable as
# long as the Grafana version is pinned (see FROM line).
#
# Version note: keep this in sync with the grafana: image: in docker-compose.yml.
# When upgrading Grafana, rebuild the image to pick up updated bundle hashes.

FROM grafana/grafana:11.2.0

# Inject custom Solarized CSS theme
COPY solarized.css /usr/share/grafana/public/build/solarized.css

USER root

# Patch index.html to load our Solarized CSS.
RUN sed -i 's|</head>|<link rel="stylesheet" href="public/build/solarized.css"></head>|' \
    /usr/share/grafana/public/views/index.html

# Patch compiled dark theme CSS: replace Grafana's dark backgrounds with Solarized Dark.
# Grafana 11 palette -> Solarized mapping:
#   gray05 #111217 (canvas)     -> #002b36 (base03)
#   gray10 #181b1f (primary)    -> #073642 (base02)
#   gray15 #22252b (secondary)  -> #073642 (base02)
#   gray25 #2c3235              -> #0a4050
#   darkLayer0 #18181a          -> #002b36 (base03)
#   darkLayer1 #212124          -> #073642 (base02)
#   darkLayer2 #2a2a2f          -> #0a4050
#   darkBorder1 #34343b         -> #586e75 (base01)
#   darkBorder2 #64646b         -> #657b83 (base00)
RUN find /usr/share/grafana/public/build/ -name '*.css' -exec sed -i \
    -e 's/#111217/#002b36/g' \
    -e 's/#181b1f/#073642/g' \
    -e 's/#22252b/#073642/g' \
    -e 's/#2c3235/#0a4050/g' \
    -e 's/#18181a/#002b36/g' \
    -e 's/#212124/#073642/g' \
    -e 's/#2a2a2f/#0a4050/g' \
    -e 's/#34343b/#586e75/g' \
    -e 's/#64646b/#657b83/g' \
    -e 's/#141414/#002b36/g' \
    -e 's/#161719/#002b36/g' \
    -e 's/#1f1f20/#073642/g' \
    -e 's/#222426/#073642/g' \
    -e 's/#262628/#073642/g' \
    -e 's/#292a2d/#073642/g' \
    -e 's/#2f2f32/#0a4050/g' \
    -e 's/#343436/#586e75/g' \
    -e 's/#424345/#586e75/g' \
    -e 's/#414350/#586e75/g' \
    -e 's/#404357/#586e75/g' \
    -e 's/#303032/#0a4050/g' \
    {} +

# Patch compiled light theme CSS: replace Grafana's light backgrounds with Solarized Light.
RUN find /usr/share/grafana/public/build/ -name 'grafana.light.*.css' -exec sed -i \
    -e 's/#f4f5f5/#fdf6e3/g' \
    -e 's/#24292e/#657b83/g' \
    -e 's/#d8d9da/#93a1a1/g' \
    -e 's/#f4f5f8/#fdf6e3/g' \
    -e 's/#fbfbfb/#fdf6e3/g' \
    -e 's/#ececec/#eee8d5/g' \
    -e 's/#e4e7e7/#93a1a1/g' \
    -e 's/#181a1b/#657b83/g' \
    {} +

# Patch JS theme palettes — dark + light.
RUN find /usr/share/grafana/public/build/ -name '*.js' -exec sed -i \
    -e 's/#111217/#002b36/g' \
    -e 's/#181b1f/#073642/g' \
    -e 's/#22252b/#073642/g' \
    -e 's/#2c3235/#0a4050/g' \
    -e 's/#18181a/#002b36/g' \
    -e 's/#212124/#073642/g' \
    -e 's/#2a2a2f/#0a4050/g' \
    -e 's/#34343b/#586e75/g' \
    -e 's/#64646b/#657b83/g' \
    -e 's/#141414/#002b36/g' \
    -e 's/#161719/#002b36/g' \
    -e 's/#1f1f20/#073642/g' \
    -e 's/#222426/#073642/g' \
    -e 's/#262628/#073642/g' \
    -e 's/#292a2d/#073642/g' \
    -e 's/#2f2f32/#0a4050/g' \
    -e 's/#343436/#586e75/g' \
    -e 's/#424345/#586e75/g' \
    -e 's/#414350/#586e75/g' \
    -e 's/#404357/#586e75/g' \
    -e 's/#303032/#0a4050/g' \
    -e 's/#f4f5f5/#fdf6e3/g' \
    -e 's/#d0d1d3/#93a1a1/g' \
    -e 's/#e4e7e7/#93a1a1/g' \
    -e 's/#d8d9da/#93a1a1/g' \
    -e 's/#ececec/#eee8d5/g' \
    -e 's/#f4f5f8/#fdf6e3/g' \
    -e 's/#fbfbfb/#fdf6e3/g' \
    -e 's/#181a1b/#657b83/g' \
    -e 's/blackBase="36, 41, 46"/blackBase="101, 123, 131"/g' \
    -e 's/primary:r.white/primary:"#eee8d5"/g' \
    {} +

USER grafana
