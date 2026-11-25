#!/usr/bin/env bash
# Record real HA with MQTT discovery (Mosquitto, five discovered entities on one device) into
# compat/fixtures/<scenario>/: the frontend's traffic, every MQTT message, HA's subscriptions,
# and the .storage files HA wrote. Needs docker, curl, jq, node and /usr/bin/chromium.
#   usage: compat/capture-mqtt.sh [mqtt-discovery|automations]
# automations adds compat/automations/*.yaml to HA's config and runs automations_steps.
set -euo pipefail
SCENARIO=${1:-mqtt-discovery}
case $SCENARIO in mqtt-discovery|automations) ;; *) echo "unknown scenario $SCENARIO" >&2; exit 2 ;; esac
ROOT=$(cd "$(dirname "$0")/.." && pwd); WORK=$ROOT/bench/.work/capture-$SCENARIO
. "$ROOT/compat/versions.env"
# HA_VER tries another release (e.g. while upgrading); the default is the pinned one.
IMG=${HA_VER:+ghcr.io/home-assistant/home-assistant:$HA_VER}; IMG=${IMG:-$HA_IMAGE}; MQTT_IMG=$MOSQUITTO_IMAGE
PORT=${PORT:-18123}; URL=http://127.0.0.1:$PORT; OUT=$ROOT/compat/fixtures/$SCENARIO
NET=lh-mqtt; HA=lh-mqtt-ha; MQ=lh-mosq

cleanup() { docker rm -f "$HA" "$MQ" >/dev/null 2>&1 || true; docker network rm "$NET" >/dev/null 2>&1 || true; }
cleanup
# HA writes root-owned files, so clean up from inside a container.
mkdir -p "$WORK"; docker run --rm -v "$WORK:/w" --entrypoint rm "$IMG" -rf /w/config /w/mosquitto
mkdir -p "$WORK/config" "$WORK/mosquitto" "$OUT"
echo "default_config:" > "$WORK/config/configuration.yaml"
[ "$SCENARIO" = automations ] && cp "$ROOT"/compat/automations/*.yaml "$WORK/config/"
cat > "$WORK/mosquitto/mosquitto.conf" <<'EOF'
listener 1883
allow_anonymous true
log_dest stdout
log_type all
log_timestamp false
EOF

docker network create "$NET" >/dev/null
trap 'kill $(jobs -p) 2>/dev/null || true; cleanup' EXIT
docker run -d --name "$MQ" --network "$NET" -v "$WORK/mosquitto/mosquitto.conf:/mosquitto/config/mosquitto.conf:ro" "$MQTT_IMG" >/dev/null
. "$ROOT/compat/mqtt-scenario.sh"
sleep 1

mqtt_logger > "$WORK/mqtt.tsv" &
mqtt_responder &
sleep 1
mqtt_devices

docker run -d --name "$HA" --network "$NET" -p "127.0.0.1:$PORT:8123" -e TZ=UTC -v "$WORK/config:/config" "$IMG" >/dev/null
for _ in $(seq 180); do curl -sf "$URL/api/onboarding" >/dev/null && break; sleep 1; done
curl -sf "$URL/api/onboarding" >/dev/null || { echo "HA did not start; docker logs $HA" >&2; exit 1; }

# Onboarding over REST, as capture.sh does it.
CID=$URL/
code=$(curl -sf "$URL/api/onboarding/users" -H 'content-type: application/json' \
  -d "{\"client_id\":\"$CID\",\"name\":\"Dev\",\"username\":\"dev\",\"password\":\"dev\",\"language\":\"en\"}" | jq -r .auth_code)
tok=$(curl -sf "$URL/auth/token" -d "grant_type=authorization_code&code=$code&client_id=$CID" | jq -r .access_token)
H=(-H "authorization: Bearer $tok" -H 'content-type: application/json')
curl -sf "${H[@]}" "$URL/api/onboarding/core_config" -d '{}' >/dev/null
curl -sf "${H[@]}" "$URL/api/onboarding/analytics" -d '{}' >/dev/null
curl -sf "${H[@]}" "$URL/api/onboarding/integration" -d "{\"client_id\":\"$CID\",\"redirect_uri\":\"${CID}?auth_callback=1\"}" >/dev/null

# The MQTT integration through its config flow, defaults except the broker.
flow=$(curl -sf "${H[@]}" "$URL/api/config/config_entries/flow" -d '{"handler":"mqtt"}' | jq -r .flow_id)
result=$(curl -s "${H[@]}" "$URL/api/config/config_entries/flow/$flow" -d "{\"broker\":\"$MQ\",\"port\":1883,\"other_settings\":{\"set_ca_cert\":\"off\",\"set_client_cert\":false}}")
[ "$(jq -r .type <<< "$result")" = create_entry ] || { echo "mqtt config flow failed: $result" >&2; exit 1; }
sleep 5

STEPS=$( [ "$SCENARIO" = automations ] && automations_steps || mqtt_steps )
MQTT_LOG=$WORK/mqtt.tsv STEPS=$STEPS NODE_PATH=$ROOT/bench/.work/node/node_modules node "$ROOT/compat/capture.mjs" "$URL" "$OUT"
rm -f "$OUT/index.html" # same page as frontend-load

# Stopping HA flushes its delayed .storage writes.
docker stop "$HA" >/dev/null
for f in core.config_entries core.entity_registry core.device_registry; do
  docker run --rm -v "$WORK/config:/c:ro" --entrypoint cat "$IMG" "/c/.storage/$f" > "$WORK/$f"
  node "$ROOT/compat/norm.mjs" "$URL" "$WORK/$f" "$OUT/$f.json"
done
mqtt_jsonl < "$WORK/mqtt.tsv" > "$WORK/mqtt.jsonl"
node "$ROOT/compat/norm.mjs" "$URL" "$WORK/mqtt.jsonl" "$OUT/mqtt.jsonl"
# HA's subscriptions, in order: mosquitto logs each granted topic as "<client-id> <qos> <topic>";
# our own tools' client ids start with lh-.
docker logs "$MQ" 2>&1 | awk 'NF == 3 && $2 ~ /^[012]$/ && $1 !~ /^lh-/ { print $3 " (QoS " $2 ")" }' > "$OUT/broker-subscriptions.txt"
docker logs "$MQ" > "$WORK/broker.log" 2>&1
echo "mqtt=$(wc -l < "$OUT/mqtt.jsonl") subscriptions=$(wc -l < "$OUT/broker-subscriptions.txt")"
