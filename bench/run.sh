#!/usr/bin/env bash
# End-to-end benchmark: each target runs in its own capped cgroup with an identical fresh config.
# Targets: hars-nopy hars-py (need setup-hars.sh), ha-core, ha-core-nollm (HA 2026.1.1),
#          ha-2611, ha-2611-slim (HA 2026.11.2; slim needs make-slim.sh).
# Env: TARGETS, MEM (default 1G), SETTLE (60), WIN (30), WORK (default bench/.work).
set -uo pipefail
BENCH=$(cd "$(dirname "$0")" && pwd); WORK=${WORK:-$BENCH/.work}; mkdir -p "$WORK"
H=$WORK/hars; MEM=${MEM:-1G}
IMG_OLD=ghcr.io/home-assistant/home-assistant:2026.1.1; IMG_NEW=ghcr.io/home-assistant/home-assistant:2026.11.2

# Same workload everywhere: homeassistant-rs's test config (demo + wemo config entries) + frontend.
fresh_cfg() { rm -rf "$1"; cp -r "$H/tests/config" "$1"; printf '\nfrontend:\n' >> "$1/configuration.yaml"; }

run_hars() { # name
  local name=$1 cfg=$WORK/cfg-$1 unit=bench-$1 pyhome sp
  pyhome=$(cat "$WORK/pyhome"); sp=$(cat "$WORK/sp"); fresh_cfg "$cfg"
  local t0=$(( $(date +%s%N) / 1000000 ))
  systemd-run --user --quiet --scope --unit="$unit" -p MemoryMax="$MEM" -p MemorySwapMax=0 -p CPUQuota=100% \
    env LD_LIBRARY_PATH="$pyhome/lib" PYTHONPATH="$H/crates/ha-py-ext/python:$sp" HA_FRONTEND_PATH="$sp/hass_frontend" \
    HA_COMPONENTS_PATH="$H/vendor/ha-core/homeassistant/components" HA_CONFIG_DIR="$cfg" HA_PORT=18123 \
    "$WORK/$name" > "$WORK/log-$name.txt" 2>&1 &
  sleep 1
  local cg=/sys/fs/cgroup$(systemctl --user show "$unit.scope" -p ControlGroup --value)
  bash "$BENCH/bench.sh" "$name" 18123 "$cg" "$t0"
  # homeassistant-rs answers /api/states without auth (a finding, not a feature).
  echo -e "$name\tstates=$(curl -s localhost:18123/api/states | python3 -c 'import sys,json;print(len(json.load(sys.stdin)))' 2>/dev/null || echo ?)"
  systemctl --user stop "$unit.scope" 2>/dev/null; wait 2>/dev/null
}

run_ha() { # name image [extra docker args]
  local name=$1 img=$2 cfg=$WORK/cfg-$1; shift 2
  # The container writes root-owned files; clean up through docker.
  docker run --rm --entrypoint rm -v "$WORK:/w" "$img" -rf "/w/cfg-$name"
  fresh_cfg "$cfg"; printf "http:\n  server_port: 18124\n" >> "$cfg/configuration.yaml"
  docker rm -f bench-ha >/dev/null 2>&1
  local t0=$(( $(date +%s%N) / 1000000 ))
  local id; id=$(docker run -d --name bench-ha --network host --memory="$MEM" --memory-swap="$MEM" --cpus=1 "$@" \
    -v "$cfg:/config" "$img")
  sleep 1
  bash "$BENCH/bench.sh" "$name" 18124 "/sys/fs/cgroup/system.slice/docker-$id.scope" "$t0"
  echo -e "$name\tregistry_entities=$(docker exec bench-ha python3 -c 'import json;print(len(json.load(open("/config/.storage/core.entity_registry"))["data"]["entities"]))' 2>/dev/null || echo ?)"
  docker logs bench-ha > "$WORK/log-$name.txt" 2>&1; docker rm -f bench-ha >/dev/null
}

for target in ${TARGETS:-hars-nopy hars-py ha-core}; do
  case $target in
    ha-core) run_ha ha-core $IMG_OLD ;;
    ha-core-nollm) run_ha ha-core-nollm $IMG_OLD -e PYTHONPATH=/stub -v "$BENCH/stub:/stub:ro" ;;
    ha-2611) run_ha ha-2611 $IMG_NEW ;;
    ha-2611-slim) run_ha ha-2611-slim $IMG_NEW $(cat "$WORK/slim/mounts") ;;
    hars-*) run_hars "$target" ;;
    *) echo "unknown target $target" >&2 ;;
  esac
done
