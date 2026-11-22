#!/usr/bin/env bash
# Plugin-host floor: probe.py layers (bare -> core -> helpers -> integrations), each in its own
# capped cgroup; reports cgroup anon memory.
#   MODE=venv   (default) HA 2026.1.1 from setup-hars.sh's venv; STUB=1 neutralizes hass_nabucasa's
#               eager litellm import (bench/stub/sitecustomize.py).
#   MODE=docker HA_VER image (default 2026.11.2); SLIM=1 overlays make-slim.sh output.
set -uo pipefail
BENCH=$(cd "$(dirname "$0")" && pwd); WORK=${WORK:-$BENCH/.work}; mkdir -p "$WORK"
MODE=${MODE:-venv}; IMG=ghcr.io/home-assistant/home-assistant:${HA_VER:-2026.11.2}

report() { # label cgroup ready-file
  awk -v l="$1" -v a="$(awk '/^anon /{print $2}' "$2/memory.stat")" -v p="$(cat "$2/memory.peak")" -v i="$(cat "$3" 2>/dev/null)" \
    'BEGIN{printf "%-16s anon_MiB=%6.1f peak_MiB=%6.1f %s\n", l, a/1048576, p/1048576, i}'
}
wait_ready() { local t=0; until [ -s "$1" ]; do sleep 0.2; t=$((t+1)); [ $t -gt 600 ] && { echo "$2 timeout"; return 1; }; done; sleep 5; }

probe_venv() { # label layer [integrations...]
  local label=$1${STUB:+-nollm}; shift; local unit=probe-$label ready=$WORK/ready-$label
  rm -f "$ready"
  systemd-run --user --quiet --scope --unit="$unit" -p MemoryMax=1G -p MemorySwapMax=0 -p CPUQuota=100% \
    env PYTHONPATH="${STUB:+$BENCH/stub:}$WORK/hars/vendor/ha-core" "$WORK/hars/.venv/bin/python" \
    "$BENCH/probe.py" "$ready" "$@" > "$WORK/log-probe-$label.txt" 2>&1 &
  sleep 0.5
  local cg=/sys/fs/cgroup$(systemctl --user show "$unit.scope" -p ControlGroup --value)
  wait_ready "$ready" "$label" && report "$label" "$cg" "$ready"
  systemctl --user stop "$unit.scope" 2>/dev/null; wait 2>/dev/null
}

probe_docker() { # label layer [integrations...]
  local label=$1${SLIM:+-slim}; shift; local ready=ready-d-$label
  rm -f "$WORK/$ready"; docker rm -f probe-d >/dev/null 2>&1
  local id; id=$(docker run -d --name probe-d --memory=1G --memory-swap=1G --cpus=1 \
    -v "$BENCH/probe.py:/w/probe.py:ro" -v "$WORK:/out" ${SLIM:+$(cat "$WORK/slim/mounts")} \
    --entrypoint python3 "$IMG" /w/probe.py "/out/$ready" "$@")
  wait_ready "$WORK/$ready" "$label" && report "$label" "/sys/fs/cgroup/system.slice/docker-$id.scope" "$WORK/$ready"
  docker rm -f probe-d >/dev/null
}

probe() { "probe_$MODE" "$@"; }
probe bare bare
probe core core
probe helpers helpers
for i in mqtt shelly hue esphome met; do probe "i-$i" integ "$i"; done
probe all-five integ mqtt shelly hue esphome met
