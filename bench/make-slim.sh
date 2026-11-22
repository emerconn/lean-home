#!/usr/bin/env bash
# Build the "slim" HA 2026.11.2 files from the official image + bench/patches, and write the
# docker -v flags that overlay them. LEVEL=a: lazy httpx2 alias only. LEVEL=b (default): + lazy
# hass_nabucasa.remote in helpers/network.py + lazy websocket_api in components/zeroconf.
set -euo pipefail
BENCH=$(cd "$(dirname "$0")" && pwd); WORK=${WORK:-$BENCH/.work}; S=$WORK/slim; LEVEL=${LEVEL:-b}
IMG=ghcr.io/home-assistant/home-assistant:2026.11.2; R=/usr/src/homeassistant/homeassistant
rm -rf "$S"; mkdir -p "$S/homeassistant/helpers" "$S/homeassistant/components/zeroconf"
cp "$BENCH/patches/homeassistant-init-lazy-httpx2.py" "$S/homeassistant/__init__.py"
mounts="-v $S/homeassistant/__init__.py:$R/__init__.py:ro"
if [ "$LEVEL" = b ]; then
  docker run --rm --entrypoint cat "$IMG" $R/helpers/network.py > "$S/homeassistant/helpers/network.py"
  docker run --rm --entrypoint cat "$IMG" $R/components/zeroconf/__init__.py > "$S/homeassistant/components/zeroconf/__init__.py"
  (cd "$S" && patch -s -p1 < "$BENCH/patches/helpers-network-lazy-cloud-remote.diff" \
            && patch -s -p1 < "$BENCH/patches/zeroconf-lazy-websocket-api.diff")
  mounts+=" -v $S/homeassistant/helpers/network.py:$R/helpers/network.py:ro"
  mounts+=" -v $S/homeassistant/components/zeroconf/__init__.py:$R/components/zeroconf/__init__.py:ro"
fi
echo "$mounts" > "$S/mounts"; echo "slim level $LEVEL ready: $S/mounts"
