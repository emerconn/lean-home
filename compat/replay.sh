#!/usr/bin/env bash
# Replay a recorded scenario against lean-home, the way it was recorded against HA, into
# bench/.work/replay/<scenario>/; compat/compare.py then diffs it with the recording.
#   usage: compat/replay.sh frontend-load|mqtt-discovery|automations
# Needs cargo, python3, node with puppeteer-core in bench/.work/node, a Chromium ($CHROMIUM,
# default /usr/bin/chromium), the hass_frontend dir ($FRONTEND), and for the MQTT scenarios docker + jq.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd); . "$ROOT/compat/versions.env"; SCENARIO=${1:?usage: replay.sh frontend-load|mqtt-discovery|automations}
OUT=$ROOT/bench/.work/replay/$SCENARIO; FIXTURES=$ROOT/compat/fixtures/$SCENARIO
FRONTEND=${FRONTEND:-$ROOT/bench/.work/hass_frontend}; export CHROMIUM=${CHROMIUM:-/usr/bin/chromium}
PORT=${PORT:-18200}; URL=http://127.0.0.1:$PORT
MQ=lh-replay-mosq; MQTT_PORT=${MQTT_PORT:-18201}
rm -rf "$OUT"; mkdir -p "$OUT"

# Debug build: the replay workload is tiny, and the optimised release build costs CI time.
(cd "$ROOT" && cargo build --locked -q)

# Poll a condition every 0.1 s, at most 5 s, instead of a fixed sleep.
wait_for() { for _ in $(seq 50); do "$@" && return; sleep 0.1; done; echo "timed out waiting for: $*" >&2; return 1; }
broker_up() { docker exec "$MQ" mosquitto_pub -i lh-probe -t lh-probe -n 2>/dev/null; }
subscribed() { [[ $(docker logs "$MQ" 2>&1) == *"$1"* ]]; } # no grep -q pipe: SIGPIPE + pipefail
cleanup() { kill $(jobs -p) 2>/dev/null || true; docker rm -f "$MQ" >/dev/null 2>&1 || true; }
trap cleanup EXIT
args=(); STEPS=
case $SCENARIO in
  frontend-load)
    python3 "$ROOT/compat/make-config.py" "$OUT/config"
    args=(--demo) # the recording toggles light.kitchen, which the demo provides
    ;;
  mqtt-discovery|automations)
    . "$ROOT/compat/mqtt-scenario.sh"
    # log_type subscribe: the waits below read subscriptions from the broker log instead of sleeping.
    printf 'listener 1883\nallow_anonymous true\nlog_type error\nlog_type warning\nlog_type notice\nlog_type information\nlog_type subscribe\n' > "$OUT/mosquitto.conf"
    docker rm -f "$MQ" >/dev/null 2>&1 || true
    docker run -d --name "$MQ" -p "127.0.0.1:$MQTT_PORT:1883" -v "$OUT/mosquitto.conf:/mosquitto/config/mosquitto.conf:ro" "$MOSQUITTO_IMAGE" >/dev/null
    wait_for broker_up
    export MQTT_LOG=$OUT/mqtt.tsv # capture.mjs waits for it to go quiet too
    mqtt_logger > "$MQTT_LOG" &
    mqtt_responder &
    # Both subscribed before the devices publish, so the logger sees every message live.
    wait_for subscribed 'lh-logger 0 #'
    wait_for subscribed 'lh-responder 0 lh/+/brightness/set'
    mqtt_devices
    python3 "$ROOT/compat/make-config.py" "$OUT/config" "$FIXTURES" 127.0.0.1 "$MQTT_PORT"
    if [ "$SCENARIO" = automations ]; then
      cp "$ROOT"/compat/automations/*.yaml "$OUT/config/"
      # The nightly automation fires at 03:00, which the recording never reached. Near 03:00 UTC,
      # replay in a zone where it's afternoon (UTC-12) so it doesn't fire mid-scenario.
      case $(date -u +%H) in 02|03) printf 'homeassistant:\n  time_zone: Etc/GMT+12\n' >> "$OUT/config/configuration.yaml" ;; esac
      STEPS=$(automations_steps)
    else
      STEPS=$(mqtt_steps)
    fi
    ;;
  *) echo "unknown scenario $SCENARIO" >&2; exit 2 ;;
esac

"$ROOT/target/debug/lean-home" --config "$OUT/config" --frontend "$FRONTEND" --listen "127.0.0.1:$PORT" "${args[@]}" > "$OUT/server.log" 2>&1 &
LH=$!
for _ in $(seq 50); do curl -sf "$URL/manifest.json" >/dev/null && break; sleep 0.2; done
curl -sf "$URL/manifest.json" >/dev/null || { echo "lean-home did not start:" >&2; cat "$OUT/server.log" >&2; exit 1; }
STEPS=${STEPS:-} NODE_PATH=$ROOT/bench/.work/node/node_modules node "$ROOT/compat/capture.mjs" "$URL" "$OUT"
# A clean stop, so the will message HA sends on shutdown is part of what's compared.
kill -TERM "$LH"; wait "$LH"
if [ "$SCENARIO" != frontend-load ]; then
  # The last message: the will lean-home sends on shutdown, as HA does.
  wait_for grep -qP '\thomeassistant/status\toffline$' "$MQTT_LOG"
  mqtt_jsonl < "$OUT/mqtt.tsv" > "$OUT/mqtt.raw.jsonl"
  node "$ROOT/compat/norm.mjs" "$URL" "$OUT/mqtt.raw.jsonl" "$OUT/mqtt.jsonl"
fi
echo "replay $SCENARIO: $OUT"
