#!/usr/bin/env bash
# Replay a recorded scenario against lean-home, the way it was recorded against HA, into
# bench/.work/replay/<scenario>/; compat/compare.py then diffs it with the recording.
#   usage: compat/replay.sh frontend-load|mqtt-discovery
# Needs cargo, python3, node with puppeteer-core in bench/.work/node, a Chromium ($CHROMIUM,
# default /usr/bin/chromium), the hass_frontend dir ($FRONTEND), and for mqtt-discovery docker + jq.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd); . "$ROOT/compat/versions.env"; SCENARIO=${1:?usage: replay.sh frontend-load|mqtt-discovery}
OUT=$ROOT/bench/.work/replay/$SCENARIO; FIXTURES=$ROOT/compat/fixtures/$SCENARIO
FRONTEND=${FRONTEND:-$ROOT/bench/.work/hass_frontend}; export CHROMIUM=${CHROMIUM:-/usr/bin/chromium}
PORT=${PORT:-18200}; URL=http://127.0.0.1:$PORT
MQ=lh-replay-mosq; MQTT_PORT=${MQTT_PORT:-18201}
rm -rf "$OUT"; mkdir -p "$OUT"

(cd "$ROOT" && cargo build --release --locked -q)

cleanup() { kill $(jobs -p) 2>/dev/null || true; docker rm -f "$MQ" >/dev/null 2>&1 || true; }
trap cleanup EXIT
args=(); STEPS=
case $SCENARIO in
  frontend-load)
    python3 "$ROOT/compat/make-config.py" "$OUT/config"
    args=(--demo) # the recording toggles light.kitchen, which the demo provides
    ;;
  mqtt-discovery)
    . "$ROOT/compat/mqtt-scenario.sh"
    printf 'listener 1883\nallow_anonymous true\n' > "$OUT/mosquitto.conf"
    docker rm -f "$MQ" >/dev/null 2>&1 || true
    docker run -d --name "$MQ" -p "127.0.0.1:$MQTT_PORT:1883" -v "$OUT/mosquitto.conf:/mosquitto/config/mosquitto.conf:ro" "$MOSQUITTO_IMAGE" >/dev/null
    sleep 1
    export MQTT_LOG=$OUT/mqtt.tsv # capture.mjs waits for it to go quiet too
    mqtt_logger > "$MQTT_LOG" &
    mqtt_responder &
    sleep 1
    mqtt_devices
    python3 "$ROOT/compat/make-config.py" "$OUT/config" "$FIXTURES" 127.0.0.1 "$MQTT_PORT"
    STEPS=$(mqtt_steps)
    ;;
  *) echo "unknown scenario $SCENARIO" >&2; exit 2 ;;
esac

"$ROOT/target/release/lean-home" --config "$OUT/config" --frontend "$FRONTEND" --listen "127.0.0.1:$PORT" "${args[@]}" > "$OUT/server.log" 2>&1 &
LH=$!
for _ in $(seq 50); do curl -sf "$URL/manifest.json" >/dev/null && break; sleep 0.2; done
curl -sf "$URL/manifest.json" >/dev/null || { echo "lean-home did not start:" >&2; cat "$OUT/server.log" >&2; exit 1; }
STEPS=${STEPS:-} NODE_PATH=$ROOT/bench/.work/node/node_modules node "$ROOT/compat/capture.mjs" "$URL" "$OUT"
# A clean stop, so the will message HA sends on shutdown is part of what's compared.
kill -TERM "$LH"; wait "$LH"
if [ "$SCENARIO" = mqtt-discovery ]; then
  sleep 1
  mqtt_jsonl < "$OUT/mqtt.tsv" > "$OUT/mqtt.raw.jsonl"
  node "$ROOT/compat/norm.mjs" "$URL" "$OUT/mqtt.raw.jsonl" "$OUT/mqtt.jsonl"
fi
echo "replay $SCENARIO: $OUT"
